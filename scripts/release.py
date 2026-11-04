"""Version, package and sign Frames releases. No third-party Python packages."""
import argparse
import hashlib
import json
from pathlib import Path
import plistlib
import re
import shutil
import struct
import subprocess
import tempfile
import time
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
REPO = "monokaijs/frames"
BUNDLE_ID = "dev.creaton.frames"
OPENSSL = next((str(path) for path in (
    Path("/opt/homebrew/opt/openssl@3/bin/openssl"),
    Path("/usr/local/opt/openssl@3/bin/openssl"),
) if path.is_file()), "openssl")

def version(value):
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value):
        raise ValueError("Expected a stable major.minor.patch version")
    return value

def next_version(bump):
    current = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    major, minor, patch = map(int, version(current).split("."))
    if bump == "major": return f"{major + 1}.0.0"
    if bump == "minor": return f"{major}.{minor + 1}.0"
    return f"{major}.{minor}.{patch + 1}"

def workspace_packages():
    """Names of workspace members that inherit `workspace.package.version`."""
    members = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["members"]
    names = []
    for member in members:
        package = tomllib.loads((ROOT / member / "Cargo.toml").read_text())["package"]
        if package.get("version") == {"workspace": True}:
            names.append(package["name"])
    assert "frames" in names, f"The app package must inherit the workspace version: {names}"
    return names

def set_version(value):
    value = version(value)
    path = ROOT / "Cargo.toml"
    text, count = re.subn(r'(?m)^version = "[^"]+"$', f'version = "{value}"', path.read_text(), count=1)
    assert count == 1
    path.write_text(text)
    names = workspace_packages()
    path = ROOT / "Cargo.lock"
    pattern = r'(\[\[package\]\]\nname = "(?:' + "|".join(map(re.escape, names)) + r')"\nversion = ")[^"]+'
    text, count = re.subn(pattern, r'\g<1>' + value, path.read_text())
    # `cargo --locked` in the release jobs fails if any member's lock entry
    # still carries the previous version.
    assert count == len(names), f"Expected {len(names)} workspace packages in Cargo.lock, found {count}"
    path.write_text(text)
    path = ROOT / "native/macos/Info.plist"
    data = plistlib.loads(path.read_bytes())
    data.update(CFBundleShortVersionString=value, CFBundleVersion=value)
    path.write_bytes(plistlib.dumps(data, sort_keys=False))

def checksum(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def staple(path):
    """Staple and validate, tolerating temporary CloudKit ticket lookup failures."""
    for action in ("staple", "validate"):
        for attempt in range(5):
            try:
                subprocess.run(["xcrun", "stapler", action, str(path)], check=True)
                break
            except subprocess.CalledProcessError as error:
                # stapler reports CloudKit failures as EX_NOHOST (68), including
                # a dropped connection after Apple has accepted notarization.
                if error.returncode != 68 or attempt == 4:
                    raise
                delay = 5 * 2 ** attempt
                print(f"stapler {action}: CloudKit lookup failed (68); retrying in {delay}s "
                      f"(attempt {attempt + 2}/5)", flush=True)
                time.sleep(delay)


def verify_windows_executable(data):
    if len(data) < 64 or data[:2] != b"MZ":
        raise ValueError("Frames.exe is not a Windows executable")
    pe = struct.unpack_from("<I", data, 60)[0]
    if pe + 94 > len(data) or data[pe:pe + 4] != b"PE\0\0":
        raise ValueError("Frames.exe has an invalid PE header")
    if struct.unpack_from("<H", data, pe + 4)[0] != 0x8664:
        raise ValueError("Frames.exe must target Windows x64")
    if struct.unpack_from("<H", data, pe + 24 + 68)[0] != 2:
        raise ValueError("Frames.exe must use the GUI subsystem to avoid a terminal window")


def verify_package(archive, platform):
    with zipfile.ZipFile(archive) as package:
        if platform == "windows-x86_64":
            if package.namelist() != ["Frames.exe"]:
                raise ValueError("Windows release ZIP must contain only Frames.exe")
            verify_windows_executable(package.read("Frames.exe"))
        else:
            for name in package.namelist():
                if Path(name).parts[0] not in ("Frames.app", "__MACOSX"):
                    raise ValueError("Mac release ZIP must contain only Frames.app")
            prefix = "Frames.app/Contents/"
            for name in ("MacOS/Frames", "MacOS/FramesHelper", "Resources/Frames.icns", "Library/LaunchAgents/dev.creaton.frames.agent.plist"):
                if prefix + name not in package.namelist():
                    raise ValueError(f"Mac release ZIP is missing {name}")
            info = plistlib.loads(package.read(prefix + "Info.plist"))
            if (info.get("CFBundleIdentifier"), info.get("CFBundleName"), info.get("CFBundleDisplayName"), info.get("CFBundleExecutable"), info.get("CFBundleIconFile")) != (BUNDLE_ID, "Frames", "Frames", "Frames", "Frames.icns"):
                raise ValueError("Mac release identity must be Frames / dev.creaton.frames")
            for name in ("MacOS/Frames", "MacOS/FramesHelper"):
                if not package.getinfo(prefix + name).external_attr >> 16 & 0o111:
                    raise ValueError(f"Mac release executable is not executable: {name}")


# Installer window layout in points, shared with generate_dmg_background.py.
# The artwork is taller than the window so a taller title bar never exposes
# an unpainted strip; nothing essential sits in the extra height.
DMG_CANVAS = (640, 440)
DMG_WINDOW = (640, 428)
DMG_ICON_SIZE = 128
DMG_ICON_Y = 190
DMG_APP_X = 170
DMG_APPLICATIONS_X = 470
DMGBUILD_VERSION = "1.6.7"


def disk_image(value):
    """A designed drag-to-Applications window; the ZIP remains the updater payload.

    dmgbuild writes the Finder layout straight into .DS_Store, so no Finder
    scripting (and no automation permission) is involved on build machines.
    """
    value = version(value)
    try:
        import dmgbuild
    except ImportError:
        raise SystemExit(f"Building the installer needs dmgbuild: python3 -m pip install dmgbuild=={DMGBUILD_VERSION}") from None
    app = ROOT / "dist/macos/Frames.app"
    info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
    if info["CFBundleIdentifier"] != BUNDLE_ID or info["CFBundleShortVersionString"] != value:
        raise ValueError("Installer identity/version does not match the app")
    output = ROOT / "dist/release" / f"Frames-{value}-macos-aarch64.dmg"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.unlink(missing_ok=True)
    width, height = DMG_WINDOW
    dmgbuild.build_dmg(str(output), "Frames", settings={
        "files": [str(app)],
        "symlinks": {"Applications": "/Applications"},
        "icon": str(app / "Contents/Resources/Frames.icns"),
        # The @2x file beside it is picked up and joined into a HiDPI TIFF.
        "background": str(ROOT / "assets/installer/dmg-background.png"),
        "window_rect": ((200, 140), (width, height)),
        "default_view": "icon-view",
        "icon_size": DMG_ICON_SIZE,
        "text_size": 13,
        "icon_locations": {"Frames.app": (DMG_APP_X, DMG_ICON_Y), "Applications": (DMG_APPLICATIONS_X, DMG_ICON_Y)},
        # No "hide_extensions": it stores Finder info on the signed bundle, and
        # codesign --strict then rejects the installed app ("resource fork,
        # Finder information, or similar detritus not allowed"). Finder hides
        # the .app extension by default anyway.
        "include_list_view_settings": False,
        "format": "UDZO",
        "filesystem": "HFS+",
    })
    print(output)
    return output


def verify_disk_image(image, value):
    """Mount, copy like Finder, and run the installed payload, leaving no mounted disk."""
    value = version(value)
    with tempfile.TemporaryDirectory(prefix="frames-install-test-") as temporary:
        root = Path(temporary)
        mount = root / "volume"
        subprocess.run(["hdiutil", "attach", "-readonly", "-nobrowse", "-mountpoint", str(mount), str(image)], check=True)
        try:
            if (mount / "Applications").readlink() != Path("/Applications"):
                raise ValueError("Installer is missing its Applications shortcut")
            visible = sorted(entry.name for entry in mount.iterdir() if not entry.name.startswith("."))
            if visible != ["Applications", "Frames.app"]:
                raise ValueError(f"Installer window shows unexpected items: {visible}")
            if not (mount / ".DS_Store").is_file() or not any(mount.glob(".background.*")):
                raise ValueError("Installer is missing its window layout")
            installed = root / "Applications/Frames.app"
            subprocess.run(["ditto", str(mount / "Frames.app"), str(installed)], check=True)
            info = plistlib.loads((installed / "Contents/Info.plist").read_bytes())
            if info.get("CFBundleIdentifier") != BUNDLE_ID or info.get("CFBundleName") != "Frames" or info.get("CFBundleShortVersionString") != value:
                raise ValueError("Installed application identity/version is incorrect")
            subprocess.run(["codesign", "--verify", "--deep", "--strict", str(installed)], check=True)
            actual = subprocess.check_output([str(installed / "Contents/MacOS/Frames"), "--version"], text=True).strip()
            subprocess.run([str(installed / "Contents/MacOS/Frames"), "media-check"], check=True)
            if actual.lower() != f"frames {value}".lower():
                raise ValueError(f"Installed executable reports {actual!r}")
            if not (installed / "Contents/Resources/Frames.icns").read_bytes().startswith(b"icns"):
                raise ValueError("Installed app icon is invalid")
        finally:
            subprocess.run(["hdiutil", "detach", str(mount)], check=True)


def package(platform, value):
    value = version(value)
    output = ROOT / "dist/release"
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"Frames-{value}-{platform}.zip"
    if platform == "macos-aarch64":
        subprocess.run(["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(ROOT / "dist/macos/Frames.app"), str(archive)], check=True)
    else:
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=6) as zip:
            zip.write(ROOT / "dist/windows/Frames.exe", "Frames.exe")
    verify_package(archive, platform)
    archive.with_suffix(".zip.sha256").write_text(f"{checksum(archive)}  {archive.name}\n")
    print(archive)


def windows_installer(value):
    value = version(value)
    source = ROOT / "dist/windows/Frames.exe"
    verify_windows_executable(source.read_bytes())
    compiler = shutil.which("ISCC.exe") or next((str(path) for path in (
        Path("C:/Program Files (x86)/Inno Setup 6/ISCC.exe"),
        Path("C:/Program Files/Inno Setup 6/ISCC.exe"),
    ) if path.is_file()), None)
    if compiler is None:
        raise RuntimeError("Install Inno Setup 6 to build the Windows installer")
    output = ROOT / "dist/release"
    output.mkdir(parents=True, exist_ok=True)
    subprocess.run([compiler, "/Qp", f"/DAppVersion={value}", f"/DAppSource={source}",
                    f"/DAppIcon={ROOT / 'assets/icons/Frames.ico'}", f"/O{output}",
                    str(ROOT / "native/windows/installer.iss")], check=True)
    installer = output / f"Frames-{value}-windows-x86_64-setup.exe"
    installer.with_suffix(".exe.sha256").write_text(f"{checksum(installer)}  {installer.name}\n")
    print(installer)
    return installer

def manifest(value, directory, key):
    value = version(value)
    targets = {}
    for platform in ("windows-x86_64", "macos-aarch64"):
        archive = directory / f"Frames-{value}-{platform}.zip"
        targets[platform] = {"url": f"https://github.com/{REPO}/releases/download/v{value}/{archive.name}", "sha256": checksum(archive), "size": archive.stat().st_size}
    output = directory / "update.json"
    output.write_text(json.dumps({"schema": 1, "version": value, "targets": targets}, sort_keys=True, separators=(",", ":")) + "\n")
    public = subprocess.check_output([OPENSSL, "pkey", "-in", str(key), "-pubout", "-outform", "DER"])[-32:].hex()
    if public != (ROOT / "scripts/update-public-key.hex").read_text().strip():
        raise ValueError("Update signing key does not match the application public key")
    subprocess.run([OPENSSL, "pkeyutl", "-sign", "-rawin", "-inkey", str(key), "-in", str(output), "-out", str(directory / "update.json.sig")], check=True)

def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    next_cmd = commands.add_parser("next"); next_cmd.add_argument("bump", choices=["patch", "minor", "major"])
    set_cmd = commands.add_parser("set-version"); set_cmd.add_argument("version")
    pack_cmd = commands.add_parser("package"); pack_cmd.add_argument("platform", choices=["windows-x86_64", "macos-aarch64"]); pack_cmd.add_argument("version")
    dmg_cmd = commands.add_parser("dmg"); dmg_cmd.add_argument("version")
    installer_cmd = commands.add_parser("windows-installer"); installer_cmd.add_argument("version")
    verify_dmg_cmd = commands.add_parser("verify-dmg"); verify_dmg_cmd.add_argument("image", type=Path); verify_dmg_cmd.add_argument("version")
    staple_cmd = commands.add_parser("staple"); staple_cmd.add_argument("path", type=Path)
    sign_cmd = commands.add_parser("manifest"); sign_cmd.add_argument("version"); sign_cmd.add_argument("directory", type=Path); sign_cmd.add_argument("key", type=Path)
    args = parser.parse_args()
    if args.command == "next": print(next_version(args.bump))
    elif args.command == "set-version": set_version(args.version)
    elif args.command == "package": package(args.platform, args.version)
    elif args.command == "dmg": disk_image(args.version)
    elif args.command == "windows-installer": windows_installer(args.version)
    elif args.command == "verify-dmg": verify_disk_image(args.image, args.version)
    elif args.command == "staple": staple(args.path)
    else: manifest(args.version, args.directory, args.key)

if __name__ == "__main__": main()
