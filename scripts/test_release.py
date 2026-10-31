"""Exercise version bumps and the archive-to-signed-manifest contract."""
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import unittest

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
