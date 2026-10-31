"""Exercise version bumps and the archive-to-signed-manifest contract."""
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import struct
import tempfile
import unittest
import zipfile

import release


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.original = release.ROOT
        self.addCleanup(setattr, release, "ROOT", self.original)
        release.ROOT = Path(self.directory.name)
        for name in ["Cargo.toml", "Cargo.lock", "native/macos/Info.plist"]:
            destination = release.ROOT / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(self.original / name, destination)

    @staticmethod
    def windows_executable(subsystem=2):
        data = bytearray(512)
        data[:2] = b"MZ"
        struct.pack_into("<I", data, 60, 128)
        data[128:132] = b"PE\0\0"
        struct.pack_into("<H", data, 132, 0x8664)
        struct.pack_into("<H", data, 152, 0x20B)
        struct.pack_into("<H", data, 220, subsystem)
        return data

    def test_windows_zip_contains_only_executable_even_with_stale_docs(self):
        source = release.ROOT / "dist/windows"
        (source / "docs").mkdir(parents=True)
        (source / "Frames.exe").write_bytes(self.windows_executable())
        (source / "README.md").write_text("must not ship")
        (source / "docs/VALIDATION.md").write_text("must not ship")
        (source / "Frames.exe.sha256").write_text("must not ship inside zip")
        release.package("windows-x86_64", "1.2.3")
        archive = release.ROOT / "dist/release/Frames-1.2.3-windows-x86_64.zip"
        with zipfile.ZipFile(archive) as package:
            self.assertEqual(package.namelist(), ["Frames.exe"])

    def test_console_executable_and_extra_archive_entries_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "GUI subsystem"):
            release.verify_windows_executable(self.windows_executable(subsystem=3))
        archive = release.ROOT / "bad.zip"
        with zipfile.ZipFile(archive, "w") as package:
            package.writestr("Frames.exe", self.windows_executable())
            package.writestr("README.md", "must not ship")
        with self.assertRaisesRegex(ValueError, "only Frames.exe"):
            release.verify_package(archive, "windows-x86_64")

    def test_bumps_and_package_versions_stay_in_sync(self):
        release.set_version("1.2.3")
        self.assertEqual(release.next_version("patch"), "1.2.4")
        self.assertEqual(release.next_version("minor"), "1.3.0")
        self.assertEqual(release.next_version("major"), "2.0.0")
        import tomllib
        packages = tomllib.loads((release.ROOT / "Cargo.lock").read_text())["package"]
        self.assertEqual([p["version"] for p in packages if p["name"] in ["frames", "frames-core", "frames-native"]], ["1.2.3"] * 3)
        bundle = plistlib.loads((release.ROOT / "native/macos/Info.plist").read_bytes())
        self.assertEqual(bundle["CFBundleVersion"], "1.2.3")
        self.assertEqual(bundle["CFBundleShortVersionString"], "1.2.3")

    def test_mac_package_requires_frames_identity_icon_and_executable_modes(self):
        archive = release.ROOT / "mac.zip"
        def write_bundle(bundle_id=release.BUNDLE_ID, executable_mode=0o100755, icon=True):
            info = plistlib.loads((release.ROOT / "native/macos/Info.plist").read_bytes())
            info["CFBundleIdentifier"] = bundle_id
            with zipfile.ZipFile(archive, "w") as package:
                package.writestr("Frames.app/Contents/Info.plist", plistlib.dumps(info))
                for name in ("Frames", "FramesHelper"):
                    entry = zipfile.ZipInfo("Frames.app/Contents/MacOS/" + name)
                    entry.external_attr = executable_mode << 16
                    package.writestr(entry, b"binary")
                package.writestr("Frames.app/Contents/Library/LaunchAgents/dev.creaton.frames.agent.plist", b"agent")
                if icon:
                    package.writestr("Frames.app/Contents/Resources/Frames.icns", b"icns")
        write_bundle()
        release.verify_package(archive, "macos-aarch64")
        write_bundle(bundle_id="app.frames.desktop")
        with self.assertRaisesRegex(ValueError, "identity"):
            release.verify_package(archive, "macos-aarch64")
        write_bundle(executable_mode=0o100644)
        with self.assertRaisesRegex(ValueError, "not executable"):
            release.verify_package(archive, "macos-aarch64")
        write_bundle(icon=False)
        with self.assertRaisesRegex(ValueError, "Frames.icns"):
            release.verify_package(archive, "macos-aarch64")

    def test_manifest_signature_covers_actual_archive_bytes(self):
        root = release.ROOT
        key = root / "test-key.pem"
        public = root / "test-public.pem"
        subprocess.run([release.OPENSSL, "genpkey", "-algorithm", "ED25519", "-out", str(key)], check=True)
        subprocess.run([release.OPENSSL, "pkey", "-in", str(key), "-pubout", "-out", str(public)], check=True)
        raw = subprocess.check_output([release.OPENSSL, "pkey", "-in", str(key), "-pubout", "-outform", "DER"])[-32:]
        (root / "scripts").mkdir()
        (root / "scripts/update-public-key.hex").write_text(raw.hex())
        for platform in ["windows-x86_64", "macos-aarch64"]:
            (root / f"Frames-1.2.3-{platform}.zip").write_bytes(platform.encode())
        release.manifest("1.2.3", root, key)
        manifest = root / "update.json"
        signature = root / "update.json.sig"
        verify = [release.OPENSSL, "pkeyutl", "-verify", "-rawin", "-pubin", "-inkey", str(public), "-in", str(manifest), "-sigfile", str(signature)]
        self.assertEqual(subprocess.run(verify, capture_output=True).returncode, 0)
        data = json.loads(manifest.read_bytes())
        for target, asset in data["targets"].items():
            archive = root / f"Frames-1.2.3-{target}.zip"
            self.assertEqual(asset["sha256"], release.checksum(archive))
            self.assertEqual(asset["size"], archive.stat().st_size)
        manifest.write_bytes(manifest.read_bytes().replace(b"1.2.3", b"9.9.9"))
        self.assertNotEqual(subprocess.run(verify, capture_output=True).returncode, 0)


if __name__ == "__main__":
    unittest.main()
