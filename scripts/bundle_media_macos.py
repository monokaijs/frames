"""Flatten codec dylibs into the app and remove Homebrew loader dependencies."""
from pathlib import Path
import os
import shutil
import subprocess

ROOT = Path(__file__).resolve().parent.parent
SDK = Path(os.environ.get('FRAMES_MEDIA_SDK', ROOT / '.media-runtime/macos'))
APP = ROOT / 'dist/macos/Frames.app'
DEST = APP / 'Contents/Frameworks'

def dependencies(path):
    lines = subprocess.check_output(['otool', '-L', str(path)], text=True).splitlines()[1:]
    return [line.strip().split(' (', 1)[0] for line in lines]

def main():
    DEST.mkdir(parents=True, exist_ok=True)
    pending = [min(SDK.joinpath('lib').glob(f'lib{name}.*.dylib'),key=lambda p:p.name.count('.'))
               for name in ('avcodec','avutil','swscale','swresample')]
    sources = {}
    while pending:
        source = pending.pop()
        if source.name in sources:
            if sources[source.name].resolve() != source.resolve():
                raise RuntimeError(f'Dylib name collision: {source.name}')
            continue
        sources[source.name] = source
        for dependency in dependencies(source):
            if dependency.startswith(('/usr/lib/', '/System/Library/')):
                continue
            dep = Path(dependency)
            if dependency.startswith(('@rpath/', '@loader_path/')):
                dep = source.parent / dep.name
                if not dep.is_file():
                    dep = SDK.parent/'macos-deps/lib'/Path(dependency).name
            if not dep.is_file():
                raise RuntimeError(f'Unresolved library {dependency} in {source}')
            if dep.resolve() != source.resolve():
                pending.append(dep)
    # All entries are regular files so the secure updater need not accept symlinks.
    for name, source in sources.items():
        target = DEST / name
        if target.is_symlink():
            target.unlink()
        shutil.copyfile(source, target)
        target.chmod(0o755)
        subprocess.run(['install_name_tool','-id',f'@rpath/{name}',str(target)], check=True)
        for dependency in dependencies(source):
            if dependency.startswith(('/usr/lib/', '/System/Library/')):
                continue
            subprocess.run(['install_name_tool','-change',dependency,
                            '@loader_path/' + Path(dependency).name,str(target)],check=True)
    for path in DEST.glob('*.dylib'):
        if path.name not in sources:
            path.unlink()
    notices = APP / 'Contents/Resources/MediaLicenses'
    notices.mkdir(parents=True,exist_ok=True)
    shutil.copyfile(ROOT/'THIRD_PARTY.md',notices/'THIRD_PARTY.md')
    for path in ROOT.joinpath('licenses').glob('*.txt'):
        shutil.copyfile(path,notices/path.name)
    for path in SDK.glob('COPYING*'):
        shutil.copyfile(path,notices/path.name)
    shutil.copyfile(SDK/'SOURCE.txt',notices/'FFmpeg-SOURCE.txt')
    shutil.copyfile(SDK/'DEPENDENCY-SOURCE.json',notices/'DEPENDENCY-SOURCE.json')
    shutil.copytree(SDK/'DependencyLicenses',notices/'Dependencies',dirs_exist_ok=True)
    for name, source in sources.items():
        resolved = source.resolve()
        if '/Cellar/' not in str(resolved):
            continue
        cellar = resolved.parent.parent
        package = cellar.parent.name
        for path in cellar.rglob('*'):
            if path.is_file() and path.name.upper().startswith(('COPYING','LICENSE','PATENTS')):
                folder=notices/package
                folder.mkdir(exist_ok=True)
                shutil.copyfile(path,folder/path.name)
    for target in DEST.glob('*.dylib'):
        if any(dep.startswith(('/opt/', '/usr/local/', str(ROOT))) for dep in dependencies(target)):
            raise RuntimeError(f'External library dependency in {target}')
        build=subprocess.check_output(['xcrun','vtool','-show-build',str(target)],text=True)
        import re
        if not re.search(r'platform\s+MACOS',build):
            raise RuntimeError(f'{target.name} is not a macOS library')
        for major,minor in re.findall(r'minos\s+(\d+)\.(\d+)',build):
            if (int(major),int(minor))>(14,2):
                raise RuntimeError(f'{target.name} requires macOS {major}.{minor}; maximum is 14.2')
    print(f'Bundled {len(sources)} in-process codec libraries')

if __name__ == '__main__':
    main()
