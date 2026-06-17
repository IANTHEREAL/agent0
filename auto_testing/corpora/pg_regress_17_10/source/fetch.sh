#!/usr/bin/env bash
# Fetch the pinned PostgreSQL regress corpus (sql/, data/, parallel_schedule)
# and verify it against CHECKSUMS.sha256. The upstream files are NOT vendored
# into the repo (they are large and license-clean-to-fetch); this script
# materialises them on demand. Idempotent: re-running re-verifies.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
TAG="$(awk '/^corpus_tag:/{print $2; exit}' ../manifest.yaml)"
TAG="${TAG:-REL_17_10}"
URL="https://codeload.github.com/postgres/postgres/tar.gz/refs/tags/${TAG}"

if sha256sum -c CHECKSUMS.sha256 >/dev/null 2>&1; then
  echo "[fetch] corpus present and verified ($TAG)"; exit 0
fi
echo "[fetch] downloading PostgreSQL $TAG regress corpus ..."
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/src.tgz" "$URL"
tar -xzf "$tmp/src.tgz" -C "$tmp" --wildcards \
  '*/src/test/regress/sql/*' '*/src/test/regress/data/*' '*/src/test/regress/parallel_schedule'
base="$(find "$tmp" -type d -name regress | head -1)"
rm -rf sql data parallel_schedule
cp -r "$base/sql" "$base/data" "$base/parallel_schedule" .
echo "[fetch] verifying checksums ..."
sha256sum -c CHECKSUMS.sha256 >/dev/null
echo "[fetch] OK: $(ls sql/*.sql | wc -l) sql files, $(ls data | wc -l) data files."
