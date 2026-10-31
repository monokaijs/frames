"""Signing selection must keep an app's privacy identity stable across builds."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'sign.sh'
FIRST = '1' * 40
SECOND = '2' * 40


class SigningIdentityTests(unittest.TestCase):
    def select(self, identities='', authority='', explicit=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, body in {
                'security': 'printf "%s\\n" "$TEST_IDENTITIES"',
                'codesign': 'printf "Authority=%s\\n" "$TEST_AUTHORITY" >&2',
            }.items():
                executable = root / name
                executable.write_text('#!/bin/sh\n' + body + '\n')
                executable.chmod(0o755)
            env = dict(os.environ, PATH=f'{root}:/usr/bin:/bin',
                       TEST_IDENTITIES=identities, TEST_AUTHORITY=authority)
            env.pop('FRAMES_SIGN_IDENTITY', None)
            if explicit is not None:
                env['FRAMES_SIGN_IDENTITY'] = explicit
            return subprocess.check_output(
                ['bash', str(SCRIPT), '--print-identity', '/test/Frames.app'],
                env=env, text=True).strip()

    def test_prefers_unique_developer_id(self):
        self.assertEqual(self.select(
            f'  1) {FIRST} "Developer ID Application: Team A"\n'
            f'  2) {SECOND} "Apple Development: Team B"'), FIRST)

    def test_reuses_existing_certificate_before_switching_teams(self):
        self.assertEqual(self.select(
            f'  1) {FIRST} "Apple Development: Existing Team"\n'
            f'  2) {SECOND} "Developer ID Application: Another Team"',
            'Apple Development: Existing Team'), FIRST)

    def test_does_not_choose_between_multiple_teams(self):
        self.assertEqual(self.select(
            f'  1) {FIRST} "Developer ID Application: Team A"\n'
            f'  2) {SECOND} "Developer ID Application: Team B"'), '')

    def test_explicit_local_signing_is_respected(self):
        self.assertEqual(self.select(explicit='-'), '-')

    def test_missing_certificate_keeps_ci_builds_possible(self):
        self.assertEqual(self.select(), '')

    def test_release_uses_secure_timestamp_and_inline_team_requirement(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = root / 'codesign'
            executable.write_text('#!/bin/sh\nprintf "%s\\0" "$@" >> "$TEST_CODESIGN_ARGS"\n')
            executable.chmod(0o755)
            output = root / 'arguments'
            env = dict(os.environ, PATH=f'{root}:/usr/bin:/bin',
                       FRAMES_SIGN_IDENTITY=FIRST, FRAMES_RELEASE_BUILD='1',
                       TEST_CODESIGN_ARGS=str(output))
            subprocess.run(['bash', str(SCRIPT), '/test/Frames.app'], env=env,
                           check=True, capture_output=True)
            args = output.read_bytes().decode().split('\0')
            self.assertIn('--timestamp', args)
            self.assertNotIn('--timestamp=none', args)
            requirement = args[args.index('-R') + 1]
            self.assertTrue(requirement.startswith('=anchor apple generic'))
            self.assertIn('certificate leaf[subject.OU] = "V59Z6S9A36"', requirement)


if __name__ == '__main__':
    unittest.main()
