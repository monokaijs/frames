"""Version, package and sign Frames releases. No third-party Python packages."""
import argparse
import hashlib
import json
from pathlib import Path
import plistlib
import re
import struct
import subprocess
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
REPO = "monokaijs/frames"
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

def set_version(value):
    value = version(value)
    path = ROOT / "Cargo.toml"
    text, count = re.subn(r'(?m)^version = "[^"]+"$', f'version = "{value}"', path.read_text(), count=1)
    assert count == 1
    path.write_text(text)
    path = ROOT / "Cargo.lock"
    text, count = re.subn(r'(\[\[package\]\]\nname = "frames(?:-core|-native)?"\nversion = ")[^"]+', r'\g<1>' + value, path.read_text())
    assert count == 3, f"Expected three workspace packages, found {count}"
    path.write_text(text)
    path = ROOT / "native/macos/Info.plist"
    data = plistlib.loads(path.read_bytes())
    data.update(CFBundleShortVersionString=value, CFBundleVersion=value)
    path.write_bytes(plistlib.dumps(data, sort_keys=False))

def checksum(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()

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
            if "Frames.app/Contents/MacOS/frames" not in package.namelist():
                raise ValueError("Mac release ZIP is missing the app executable")


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
    sign_cmd = commands.add_parser("manifest"); sign_cmd.add_argument("version"); sign_cmd.add_argument("directory", type=Path); sign_cmd.add_argument("key", type=Path)
    args = parser.parse_args()
    if args.command == "next": print(next_version(args.bump))
    elif args.command == "set-version": set_version(args.version)
    elif args.command == "package": package(args.platform, args.version)
    else: manifest(args.version, args.directory, args.key)

if __name__ == "__main__": main()
