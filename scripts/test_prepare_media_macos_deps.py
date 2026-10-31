"""Pinned codec sources must survive temporary server and transfer failures."""
import hashlib
import http.client
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import call, patch
import urllib.error

import prepare_media_macos_deps as media


class SourceDownloadTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.archive = Path(self.directory.name) / 'source.tar.gz'
        self.payload = b'pinned codec source'
        self.digest = hashlib.sha256(self.payload).hexdigest()
        self.url = 'https://source.example/archive.tar.gz'

    def download(self, **kwargs):
        media.ensure_source(self.url, self.archive, 'libvpx', self.digest, **kwargs)

    def assert_no_partial_files(self):
        self.assertEqual(list(self.archive.parent.glob('*.part')), [])

    @patch.object(media.time, 'sleep')
    @patch.object(media.urllib.request, 'urlopen')
    def test_503_and_timeout_recover_before_caching_verified_source(self, urlopen, sleep):
        urlopen.side_effect = [
            urllib.error.HTTPError(self.url, 503, 'Service Unavailable', None, None),
            TimeoutError('transfer timed out'),
            io.BytesIO(self.payload),
        ]
        self.download()
        self.assertEqual(self.archive.read_bytes(), self.payload)
        self.assertEqual(urlopen.call_args_list, [call(self.url, timeout=60)] * 3)
        self.assertEqual(sleep.call_args_list, [call(1), call(2)])
        self.assert_no_partial_files()

    @patch.object(media.time, 'sleep')
    @patch.object(media.urllib.request, 'urlopen')
    def test_truncated_response_is_retried(self, urlopen, sleep):
        class TruncatedResponse(io.BytesIO):
            def read(self, size=-1):
                raise http.client.IncompleteRead(b'partial', 100)
        urlopen.side_effect = [TruncatedResponse(), io.BytesIO(self.payload)]
        self.download()
        self.assertEqual(self.archive.read_bytes(), self.payload)
        sleep.assert_called_once_with(1)
        self.assert_no_partial_files()

    @patch.object(media.time, 'sleep')
    @patch.object(media.urllib.request, 'urlopen')
    def test_missing_source_is_not_retried(self, urlopen, sleep):
        urlopen.side_effect = urllib.error.HTTPError(self.url, 404, 'Not Found', None, None)
        with self.assertRaises(urllib.error.HTTPError):
            self.download()
        urlopen.assert_called_once()
        sleep.assert_not_called()
        self.assertFalse(self.archive.exists())
        self.assert_no_partial_files()

    @patch.object(media.time, 'sleep')
    @patch.object(media.urllib.request, 'urlopen')
    def test_exhausted_retries_preserve_existing_cache(self, urlopen, sleep):
        self.archive.write_bytes(b'previous archive')
        urlopen.side_effect = urllib.error.HTTPError(self.url, 503, 'Unavailable', None, None)
        with self.assertRaises(urllib.error.HTTPError):
            self.download(attempts=3)
        self.assertEqual(urlopen.call_count, 3)
        self.assertEqual(sleep.call_args_list, [call(1), call(2)])
        self.assertEqual(self.archive.read_bytes(), b'previous archive')
        self.assert_no_partial_files()

    @patch.object(media.urllib.request, 'urlopen')
    def test_checksum_mismatch_never_replaces_cached_source(self, urlopen):
        self.archive.write_bytes(b'previous archive')
        urlopen.return_value = io.BytesIO(b'wrong source')
        with self.assertRaisesRegex(RuntimeError, 'checksum mismatch'):
            self.download()
        self.assertEqual(self.archive.read_bytes(), b'previous archive')
        self.assert_no_partial_files()

    @patch.object(media.urllib.request, 'urlopen')
    def test_verified_cache_avoids_network(self, urlopen):
        self.archive.write_bytes(self.payload)
        self.download()
        urlopen.assert_not_called()

    @patch.object(media.urllib.request, 'urlopen')
    def test_corrupt_aom_tar_cache_is_replaced_after_tree_verification(self, urlopen):
        payload = io.BytesIO()
        with tarfile.open(fileobj=payload, mode='w:gz') as archive:
            entry = tarfile.TarInfo('README')
            entry.size = len(self.payload)
            archive.addfile(entry, io.BytesIO(self.payload))
        valid = self.archive.with_name('valid.tar.gz')
        valid.write_bytes(payload.getvalue())
        digest = media.checksum(valid, 'aom')
        self.archive.write_bytes(b'interrupted download')
        urlopen.return_value = io.BytesIO(valid.read_bytes())
        media.ensure_source(self.url, self.archive, 'aom', digest)
        self.assertEqual(media.checksum(self.archive, 'aom'), digest)
        self.assert_no_partial_files()


if __name__ == '__main__':
    unittest.main()
