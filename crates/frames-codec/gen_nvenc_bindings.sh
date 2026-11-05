#!/bin/bash
# Regenerates src/nvenc/v<major>_<minor>/sys.rs from a vendored NVENC header
# (MIT, nv-codec-headers n13.1.15.0, n12.2.72.0, n12.1.14.1 and n12.0.16.2).
# Bindings are committed; no bindgen at build time. Requires bindgen-cli 0.71
# and libclang. Run from crates/frames-codec as `./gen_nvenc_bindings.sh 13.1`
# (or 12.2, 12.1, 12.0).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
version=${1:?usage: gen_nvenc_bindings.sh 13.1|12.2|12.1|12.0}
header="$here/third_party/nvEncodeAPI-$version.h"
stub=$(mktemp -d)
touch "$stub/stdlib.h"
clang_inc=$(ls -d "$(dirname "$(xcrun --find clang 2>/dev/null || command -v clang)")"/../lib/clang/*/include 2>/dev/null | head -1)
# Generated for x86_64 Windows layouts; _WIN32 is undefined so the header uses
# its own GUID typedef instead of <windows.h> (identical layout; __stdcall is
# ignored on x86_64).
bindgen "$header" \
  --use-core --no-layout-tests --with-derive-default --no-prepend-enum-name \
  --default-enum-style consts \
  --allowlist-type 'NV_ENC.*|NVENC.*|_NV_ENC.*|NV_ENCODE.*|GUID|PNVENC.*' \
  --allowlist-var 'NV_ENC.*|NVENC.*' --blocklist-item 'NV_ENC_.*_GUID' \
  --raw-line '#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code, unused_imports, clippy::all)]' \
  -o "$here/src/nvenc/v${version/./_}/sys.rs" \
  -- --target=x86_64-pc-windows-msvc -U_WIN32 -nostdinc -isystem "$stub" -isystem "$clang_inc"
rm -rf "$stub"
