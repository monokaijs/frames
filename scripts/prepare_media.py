"""Fetch the pinned LGPL Windows codec SDK. No FFmpeg executable is shipped."""
import hashlib
import pathlib
import urllib.request
import zipfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
DEST = ROOT / '.media-runtime' / 'windows'
URL = ('https://github.com/BtbN/FFmpeg-Builds/releases/download/'
       'autobuild-2026-10-31-12-59/'
       'ffmpeg-n8.1.3-27-g8dca1586f4-win64-lgpl-shared-8.1.zip')
SHA256 = '5cef7bc48e513f3cee7c881e8b2999d835541edb1846c3e89443a67f8f8556ab'

def main():
    DEST.mkdir(parents=True, exist_ok=True)
    archive = DEST.parent / 'ffmpeg-windows.zip'
    if not archive.exists() or hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
        print('Downloading pinned LGPL codec SDK...', flush=True)
        urllib.request.urlretrieve(URL, archive)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
        raise RuntimeError('Codec SDK checksum mismatch')
    with zipfile.ZipFile(archive) as z:
        for entry in z.infolist():
            relative = pathlib.PurePosixPath(*pathlib.PurePosixPath(entry.filename).parts[1:])
            if '..' in relative.parts or relative.is_absolute():
                raise RuntimeError('Unsafe SDK path')
            if entry.is_dir() or not relative.parts:
                continue
            # Headers for the shim and shared libraries for in-process loading.
            if relative.parts[0] == 'include' or relative.suffix.lower() == '.dll' or relative.name.startswith(('LICENSE', 'COPYING')):
                target = DEST.joinpath(*relative.parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(z.read(entry))
    (DEST / 'SOURCE.txt').write_text(f'LGPL FFmpeg shared libraries\n{URL}\nSHA256: {SHA256}\n'
        'Source: https://github.com/FFmpeg/FFmpeg/tree/8dca1586f4\n'
        'Build recipes and dependency sources: https://github.com/BtbN/FFmpeg-Builds\n', encoding='utf-8')
    print(DEST)

if __name__ == '__main__':
    main()
