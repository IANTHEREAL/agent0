#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

CACHE_DIR="${DB9_CI_ENV_CACHE_DIR:-$ROOT_DIR/.cache/ci-env}"
DEB_DIR="$CACHE_DIR/debs"
EXTRACT_DIR="$CACHE_DIR/ubuntu24"
LIB_DIR="$EXTRACT_DIR/usr/lib/x86_64-linux-gnu"

UBUNTU_MIRROR="${DB9_CI_UBUNTU_MIRROR:-https://archive.ubuntu.com/ubuntu}"
ICU74_DEB="${DB9_CI_ICU74_DEB:-libicu74_74.2-1ubuntu3.1_amd64.deb}"
ICU74_URL="${UBUNTU_MIRROR}/pool/main/i/icu/${ICU74_DEB}"

usage() {
  cat <<'EOF'
Usage:
  scripts/ci_env.sh prepare-libs
  scripts/ci_env.sh env
  scripts/ci_env.sh run -- <command> [args...]

Commands:
  prepare-libs      Download/extract Ubuntu 24.04 libicu74 runtime libs locally.
  env               Print export command for LD_LIBRARY_PATH.
  run -- <cmd...>   Run a command with CI-compatible lib path injected.

Environment:
  DB9_CI_ENV_CACHE_DIR   Cache root (default: .cache/ci-env)
  DB9_CI_UBUNTU_MIRROR   Ubuntu mirror base URL
  DB9_CI_ICU74_DEB       libicu74 deb filename

Examples:
  bash scripts/ci_env.sh prepare-libs
  bash scripts/ci_env.sh run -- /tmp/db9-server --help
  bash scripts/ci_env.sh env
EOF
}

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "ERROR: missing required command: $cmd" >&2
    exit 1
  fi
}

ensure_libs() {
  require_cmd curl
  require_cmd dpkg-deb

  if [[ -f "$LIB_DIR/libicuuc.so.74" && -f "$LIB_DIR/libicui18n.so.74" && -f "$LIB_DIR/libicudata.so.74" ]]; then
    return
  fi

  mkdir -p "$DEB_DIR" "$EXTRACT_DIR"
  local deb_path="$DEB_DIR/$ICU74_DEB"

  if [[ ! -f "$deb_path" ]]; then
    echo "Downloading: $ICU74_URL"
    curl -fL --retry 3 --retry-all-errors --connect-timeout 10 --max-time 300 \
      -o "$deb_path" "$ICU74_URL"
  fi

  echo "Extracting: $deb_path"
  dpkg-deb -x "$deb_path" "$EXTRACT_DIR"

  if [[ ! -f "$LIB_DIR/libicuuc.so.74" || ! -f "$LIB_DIR/libicui18n.so.74" || ! -f "$LIB_DIR/libicudata.so.74" ]]; then
    echo "ERROR: extracted libicu74 package is missing expected .so files" >&2
    exit 1
  fi
}

print_env_export() {
  ensure_libs
  printf 'export LD_LIBRARY_PATH=%q:${LD_LIBRARY_PATH:-}\n' "$LIB_DIR"
}

run_with_env() {
  ensure_libs
  if [[ $# -eq 0 ]]; then
    echo "ERROR: missing command for 'run --'" >&2
    exit 2
  fi
  LD_LIBRARY_PATH="$LIB_DIR:${LD_LIBRARY_PATH:-}" "$@"
}

if [[ $# -lt 1 ]]; then
  usage
  exit 2
fi

case "$1" in
  prepare-libs)
    ensure_libs
    echo "Prepared CI-compatible libs at: $LIB_DIR"
    ;;
  env)
    print_env_export
    ;;
  run)
    shift
    if [[ "${1:-}" != "--" ]]; then
      echo "ERROR: 'run' requires '-- <command> [args...]'" >&2
      usage >&2
      exit 2
    fi
    shift
    run_with_env "$@"
    ;;
  -h|--help|help)
    usage
    ;;
  *)
    echo "ERROR: unknown command '$1'" >&2
    usage >&2
    exit 2
    ;;
esac
