#!/bin/bash
# Reuse a certificate identity so TCC grants survive application rebuilds.
set -euo pipefail

frames_signing_identity() {
  if [[ -n "${FRAMES_SIGN_IDENTITY:-}" ]]; then
    printf '%s\n' "$FRAMES_SIGN_IDENTITY"
    return
  fi
  local app="$1" identities authority match candidates count
  identities="$(security find-identity -v -p codesigning 2>/dev/null || true)"
  authority="$(codesign -d --verbose=2 "$app" 2>&1 | sed -n 's/^Authority=//p' | head -n 1 || true)"
  if [[ -n "$authority" ]]; then
    match="$(printf '%s\n' "$identities" | awk -F '"' -v name="$authority" '$2 == name {split($1, fields, " "); print fields[2]}')"
    if [[ -n "$match" ]]; then printf '%s\n' "$match"; return; fi
  fi
  for kind in 'Developer ID Application:' 'Apple Development:'; do
    candidates="$(printf '%s\n' "$identities" | awk -F '"' -v kind="$kind" 'index($2, kind) == 1 {split($1, fields, " "); print fields[2]}')"
    count="$(printf '%s\n' "$candidates" | awk 'NF {n++} END {print n+0}')"
    if [[ "$count" == 1 ]]; then printf '%s\n' "$candidates"; return; fi
    # Do not silently switch to another team when several identities exist.
    if [[ "$count" -gt 1 ]]; then return; fi
  done
}

frames_sign_app() {
  local app="$1" identity
  identity="$(frames_signing_identity "$app")"
  if [[ -z "$identity" ]]; then
    printf 'No unambiguous signing identity. Set FRAMES_SIGN_IDENTITY; unsigned/ad-hoc builds can lose macOS privacy grants after rebuilds.\n' >&2
    return
  fi
  codesign --force --sign "$identity" --identifier app.frames.helper --options runtime --timestamp=none "$app/Contents/MacOS/FramesHelper"
  codesign --force --sign "$identity" --options runtime --timestamp=none "$app"
  codesign --verify --deep --strict "$app"
  if [[ "$identity" == - ]]; then
    printf 'Ad-hoc signed %s; macOS privacy grants may reset after rebuilds.\n' "$app"
  else
    printf 'Signed %s with a persistent certificate identity.\n' "$app"
  fi
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  if [[ "${1:-}" == --print-identity ]]; then
    frames_signing_identity "${2:?app path required}"
  else
    frames_sign_app "${1:?app path required}"
  fi
fi
