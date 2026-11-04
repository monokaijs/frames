#!/bin/bash
# Keep only the most recently used GitHub Actions cache whose key starts with
# $1 (one dependency cache per platform). Runs before rust-cache saves, so a
# dependency change briefly leaves two entries; the next run removes the older.
set -euo pipefail
prefix="$1"
ids=$(gh cache list --repo "$GITHUB_REPOSITORY" --key "$prefix" --sort last_accessed_at --order desc --limit 100 --json id --jq '.[1:][].id')
for id in $ids; do
  gh cache delete "$id" --repo "$GITHUB_REPOSITORY" || true
done
echo "Pruned $(printf '%s\n' $ids | grep -c . || true) superseded caches for $prefix"
