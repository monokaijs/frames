#!/bin/bash
# Build a minimal LGPL FFmpeg runtime; never use Homebrew's GPL FFmpeg binary.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASE="$ROOT/.media-runtime"
SDK="$BASE/macos"
COMMIT=8dca1586f468bfcbb78ae3ed618af1361d9f140a
SHA=5ce832c43ca0a336c747db9dcd448655c64c25e33cb9868e1cc5b24abb04dbaa
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
export MACOSX_DEPLOYMENT_TARGET=14.2
for dependency in cmake pkgconf meson ninja; do
  brew list --versions "$dependency" >/dev/null || brew install "$dependency"
done
mkdir -p "$BASE"
BUILD_ID="$COMMIT-sdk-v4"
if [[ -f "$SDK/frames-build-id" && "$(cat "$SDK/frames-build-id")" == "$BUILD_ID" ]]; then
  printf 'Prepared codec SDK: %s\n' "$SDK"; exit
fi
python3 "$ROOT/scripts/prepare_media_macos_deps.py"
export PKG_CONFIG_LIBDIR="$BASE/macos-deps/lib/pkgconfig"
export PKG_CONFIG_PATH="$PKG_CONFIG_LIBDIR"
archive="$BASE/ffmpeg-source.tar.gz"
if [[ ! -f "$archive" ]] || [[ "$(shasum -a 256 "$archive" | cut -d ' ' -f 1)" != "$SHA" ]]; then
  curl -fL --retry 3 "https://github.com/FFmpeg/FFmpeg/archive/$COMMIT.tar.gz" -o "$archive"
fi
echo "$SHA  $archive" | shasum -a 256 -c -
tar -xzf "$archive" -C "$BASE"
cd "$BASE/FFmpeg-$COMMIT"
if [[ -f ffbuild/config.mak ]]; then make clean; fi
./configure --prefix="$SDK" --cc=clang --arch=arm64 --enable-shared --disable-static \
  --extra-ldflags="-Wl,-rpath,$BASE/macos-deps/lib" \
  --disable-programs --disable-doc --disable-debug --disable-everything --disable-autodetect \
  --disable-avformat --disable-avdevice --disable-avfilter \
  --enable-avcodec --enable-avutil --enable-swscale --enable-swresample \
  --enable-videotoolbox --enable-libvpx --enable-libaom --enable-libdav1d \
  --enable-encoder=h264_videotoolbox,hevc_videotoolbox,libvpx_vp9,libaom_av1 \
  --enable-decoder=h264,hevc,av1,vp9,libdav1d \
  --enable-hwaccel=h264_videotoolbox,hevc_videotoolbox,av1_videotoolbox,vp9_videotoolbox
make -j"$(sysctl -n hw.logicalcpu)"
make install
cp COPYING.LGPLv2.1 "$SDK/"
cp -R "$BASE/macos-deps/share/licenses" "$SDK/DependencyLicenses"
cp "$BASE/macos-deps/SOURCE.json" "$SDK/DEPENDENCY-SOURCE.json"
printf 'Source: https://github.com/FFmpeg/FFmpeg/tree/%s\nConfigure: %s\n' "$COMMIT" "$(cat ffbuild/config.sh)" > "$SDK/SOURCE.txt"
printf '%s\n' "$BUILD_ID" > "$SDK/frames-build-id"
