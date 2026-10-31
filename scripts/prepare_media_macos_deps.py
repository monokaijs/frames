"""Build codec dependencies from pinned source for macOS 14.2 (not brew bottles)."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.request

ROOT=Path(__file__).resolve().parent.parent
BASE=ROOT/'.media-runtime'
PREFIX=BASE/'macos-deps'
SOURCES={
    'libvpx': ('https://github.com/webmproject/libvpx/archive/refs/tags/v1.17.0.tar.gz',
               '1020f184046187baa2985dbde38e0691f49c44088bca7a1842b0236c6081dc0a'),
    'aom': ('https://aomedia.googlesource.com/aom/+archive/af3dc9aadc793c1b00edc003e3fc19a44fdeb574.tar.gz',
            '65614807056996d241d5f6f03c53ffd7d59966aca9e6f00ef050e5c8940c8404'),
    'dav1d': ('https://code.videolan.org/videolan/dav1d/-/archive/1.5.4/dav1d-1.5.4.tar.bz2',
              '2abfb0c89212e6e4733a54e0ae509ec00a5b845a6360946f918806e14aedb011'),
}

def run(args,cwd=None):
    subprocess.run([str(a) for a in args],cwd=cwd,check=True)

def checksum(archive,name):
    if name!='aom':
        return hashlib.sha256(archive.read_bytes()).hexdigest()
    # Gitiles inserts the request time into tar metadata. Pin the actual file
    # tree (names, executable bits, types, content), excluding those timestamps.
    digest=hashlib.sha256()
    with tarfile.open(archive) as t:
        for entry in sorted(t.getmembers(),key=lambda e:e.name):
            if entry.isdir():continue
            payload=t.extractfile(entry).read() if entry.isfile() else entry.linkname.encode()
            digest.update(f'{entry.name}\0{bool(entry.mode&0o111)}\0{entry.type!r}\0'.encode())
            digest.update(hashlib.sha256(payload).digest())
    return digest.hexdigest()

def ensure_source(url,archive,name,digest,attempts=5):
    try:
        if checksum(archive,name)==digest:
            return
    except (FileNotFoundError,tarfile.TarError,EOFError):
        # Older downloads could leave an incomplete archive in the cache.
        pass
    for attempt in range(attempts):
        with tempfile.NamedTemporaryFile(dir=archive.parent,prefix=archive.name+'.',
                                         suffix='.part',delete=False) as temporary:
            pending=Path(temporary.name)
        try:
            with urllib.request.urlopen(url,timeout=60) as response,pending.open('wb') as output:
                shutil.copyfileobj(response,output)
            if checksum(pending,name)!=digest:
                raise RuntimeError(f'{name} source checksum mismatch')
            pending.replace(archive)
            return
        except (urllib.error.URLError,TimeoutError,ConnectionError,http.client.IncompleteRead) as error:
            if isinstance(error,urllib.error.HTTPError):
                error.close()
                if error.code not in (408,429,500,502,503,504):
                    raise
            if attempt+1==attempts:
                raise
            pending.unlink(missing_ok=True)
            delay=2**attempt
            print(f'{name} source download failed ({error}); retrying in {delay}s '
                  f'({attempt+1}/{attempts})',flush=True)
            time.sleep(delay)
        finally:
            pending.unlink(missing_ok=True)

def main():
    BASE.mkdir(exist_ok=True)
    identity=json.dumps({'sources':SOURCES,'recipe':2},sort_keys=True)
    stamp=PREFIX/'frames-build-id'
    if stamp.exists() and stamp.read_text()==identity:
        return
    os.environ.update(MACOSX_DEPLOYMENT_TARGET='14.2',
                      CFLAGS='-O3 -mmacosx-version-min=14.2',
                      CXXFLAGS='-O3 -mmacosx-version-min=14.2',
                      LDFLAGS='-mmacosx-version-min=14.2')
    PREFIX.mkdir(exist_ok=True)
    jobs=str(os.cpu_count() or 4)
    for name,(url,digest) in SOURCES.items():
        archive=BASE/(name+'-source.tar'+('.bz2' if url.endswith('.bz2') else '.gz'))
        ensure_source(url,archive,name,digest)
        destination=BASE/(name+'-source')
        destination.mkdir(exist_ok=True)
        with tarfile.open(archive) as t:
            t.extractall(destination,filter='data')
        source=destination if name=='aom' else next(p for p in destination.iterdir() if p.is_dir())
        if name=='libvpx':
            run([source/'configure',f'--prefix={PREFIX}','--target=arm64-darwin23-gcc',
                 '--enable-shared','--disable-static','--disable-examples','--disable-tools',
                 '--disable-docs','--disable-unit-tests'],source)
            run(['make','-j'+jobs],source);run(['make','install'],source)
            for library in PREFIX.joinpath('lib').glob('libvpx.*.dylib'):
                if not library.is_symlink():
                    run(['install_name_tool','-id',library,library])
        elif name=='aom':
            build=BASE/'aom-build'
            run(['cmake','-S',source,'-B',build,f'-DCMAKE_INSTALL_PREFIX={PREFIX}',
                 '-DCMAKE_BUILD_TYPE=Release','-DCMAKE_OSX_DEPLOYMENT_TARGET=14.2',
                 '-DCMAKE_OSX_ARCHITECTURES=arm64','-DBUILD_SHARED_LIBS=ON',
                 '-DENABLE_DOCS=OFF','-DENABLE_EXAMPLES=OFF','-DENABLE_TOOLS=OFF',
                 '-DENABLE_TESTS=OFF','-DENABLE_LIBVMAF=OFF'])
            run(['cmake','--build',build,'--parallel',jobs]);run(['cmake','--install',build])
        else:
            build=BASE/'dav1d-build'
            args=['meson','setup',build,source,f'--prefix={PREFIX}','--libdir=lib',
                  '--buildtype=release','--default-library=shared',
                  '-Denable_tools=false','-Denable_tests=false','-Denable_examples=false']
            if (build/'meson-private/coredata.dat').exists():args.append('--reconfigure')
            run(args);run(['meson','compile','-C',build,'-j',jobs]);run(['meson','install','-C',build])
        notices=PREFIX/'share/licenses'/name;notices.mkdir(parents=True,exist_ok=True)
        for path in source.iterdir():
            if path.is_file() and path.name.upper().startswith(('COPYING','LICENSE','PATENTS','AUTHORS')):
                shutil.copyfile(path,notices/path.name)
    (PREFIX/'SOURCE.json').write_text(identity+'\n')
    stamp.write_text(identity)

if __name__=='__main__':
    main()
