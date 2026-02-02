#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

MIN_GO_MAJOR=1
MIN_GO_MINOR=22

die() {
  echo "setup_dev.sh: $*" >&2
  exit 1
}

have() { command -v "$1" >/dev/null 2>&1; }

require_cmd() {
  local cmd="$1"
  have "$cmd" || die "missing required command: $cmd"
}

go_version_ok() {
  local goversion major minor rest
  goversion="$(go env GOVERSION 2>/dev/null || true)"
  goversion="${goversion#go}"
  major="${goversion%%.*}"
  rest="${goversion#*.}"
  minor="${rest%%.*}"

  [[ "$major" =~ ^[0-9]+$ ]] || return 1
  [[ "$minor" =~ ^[0-9]+$ ]] || return 1

  if (( major > MIN_GO_MAJOR )); then
    return 0
  fi
  if (( major < MIN_GO_MAJOR )); then
    return 1
  fi
  (( minor >= MIN_GO_MINOR ))
}

print_usage() {
  cat <<'USAGE'
Usage: ./setup_dev.sh [--skip-tools] [--skip-test]

Installs pinned dev tools into ./bin and runs a quick local verification.

Flags:
  --skip-tools   Do not install/update tooling (golangci-lint, goimports, govulncheck).
  --skip-test    Do not run go test ./... at the end.
USAGE
}

SKIP_TOOLS=0
SKIP_TEST=0

for arg in "$@"; do
  case "$arg" in
    --skip-tools) SKIP_TOOLS=1 ;;
    --skip-test) SKIP_TEST=1 ;;
    -h|--help) print_usage; exit 0 ;;
    *) die "unknown argument: $arg (try --help)" ;;
  esac
done

require_cmd git
require_cmd go

if ! go_version_ok; then
  die "Go >= ${MIN_GO_MAJOR}.${MIN_GO_MINOR} is required (found $(go env GOVERSION 2>/dev/null || echo 'unknown'))"
fi

if [[ ! -f "go.mod" ]]; then
  die "go.mod not found; run this script from the repo root"
fi

echo "setup_dev.sh: repo=$REPO_ROOT"
echo "setup_dev.sh: go=$(go env GOVERSION)"

if (( SKIP_TOOLS == 0 )); then
  require_cmd make
  echo "setup_dev.sh: installing dev tools into ./bin"
  make tools
fi

echo "setup_dev.sh: downloading module deps"
go mod download

if (( SKIP_TEST == 0 )); then
  echo "setup_dev.sh: running unit tests"
  go test ./...
fi

cat <<'DONE'
setup_dev.sh: done

Tip: add ./bin to your PATH to use installed tools directly:
  export PATH="$(pwd)/bin:$PATH"
DONE

