#!/usr/bin/env bash
# Fetch the pinned sqllogictest corpus files and verify against CHECKSUMS.sha256.
# Pinned to gregrahn/sqllogictest @ a fixed commit; not vendored.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
SHA="c67f97bf3ca7e590d12e073408bcacaf2ff0f3a0"
BASE="https://raw.githubusercontent.com/gregrahn/sqllogictest/$SHA/test"
if sha256sum -c CHECKSUMS.sha256 >/dev/null 2>&1; then echo "[fetch] slt corpus present + verified"; exit 0; fi
mkdir -p tests
for f in $(awk '{print $2}' CHECKSUMS.sha256); do
  echo "[fetch] $f"; curl -fsSL -o "tests/$(basename $f)" "$BASE/$(basename $f)"
done
( cd tests && sha256sum -c ../CHECKSUMS.sha256 >/dev/null )
echo "[fetch] OK: $(ls tests/*.test | wc -l) .test files"
