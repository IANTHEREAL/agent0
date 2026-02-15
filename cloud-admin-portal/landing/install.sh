#!/bin/sh
set -e

# db9 installer (includes sh9 filesystem shell)
# Usage: curl -fsSL https://db9.shared.aws.tidbcloud.com/install | sh

BASE_URL="https://db9.shared.aws.tidbcloud.com/releases"
INSTALL_DIR="${DB9_INSTALL_DIR:-/usr/local/bin}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
DIM='\033[2m'
BOLD='\033[1m'
RESET='\033[0m'

info()    { printf "  ${DIM}%s${RESET}\n" "$1"; }
success() { printf "  ${GREEN}%s${RESET}\n" "$1"; }
warn()    { printf "  ${YELLOW}%s${RESET}\n" "$1"; }
error()   { printf "  ${RED}error:${RESET} %s\n" "$1" >&2; exit 1; }

detect_os() {
  case "$(uname -s)" in
    Linux*)   echo "linux" ;;
    Darwin*)  echo "darwin" ;;
    MINGW*|MSYS*|CYGWIN*) echo "windows" ;;
    *) error "Unsupported OS: $(uname -s)" ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64)   echo "amd64" ;;
    aarch64|arm64)   echo "arm64" ;;
    *) error "Unsupported architecture: $(uname -m)" ;;
  esac
}

download() {
  if command -v curl > /dev/null 2>&1; then
    curl -fsSL -o "$2" "$1"
  elif command -v wget > /dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    error "Neither curl nor wget found."
  fi
}

main() {
  printf "\n"
  printf "  ${BOLD}db9${RESET} installer\n"
  printf "  ${DIM}────────────────────────────${RESET}\n"
  printf "\n"

  OS=$(detect_os)
  ARCH=$(detect_arch)
  info "Platform: ${OS}/${ARCH}"

  # Ensure install dir exists
  if [ ! -d "$INSTALL_DIR" ]; then
    mkdir -p "$INSTALL_DIR" 2>/dev/null || sudo mkdir -p "$INSTALL_DIR"
  fi

  TMP_DIR=$(mktemp -d)
  trap 'rm -rf "$TMP_DIR"' EXIT

  # Download db9
  info "Downloading db9..."
  if ! download "${BASE_URL}/db9-${OS}-${ARCH}" "$TMP_DIR/db9"; then
    error "No pre-built binary available for ${OS}/${ARCH}.\n  Available: linux/amd64, linux/arm64, darwin/amd64, darwin/arm64\n  Visit https://db9.shared.aws.tidbcloud.com for more info."
  fi
  chmod +x "$TMP_DIR/db9"

  # Download sh9
  info "Downloading sh9..."
  if ! download "${BASE_URL}/sh9-${OS}-${ARCH}" "$TMP_DIR/sh9"; then
    warn "sh9 binary not available for ${OS}/${ARCH} — skipping (install later with: curl -fsSL https://db9.shared.aws.tidbcloud.com/install-sh9 | sh)"
    SH9_OK=0
  else
    chmod +x "$TMP_DIR/sh9"
    SH9_OK=1
  fi

  # Install
  if [ -w "$INSTALL_DIR" ]; then
    mv "$TMP_DIR/db9" "$INSTALL_DIR/db9"
    [ "$SH9_OK" = "1" ] && mv "$TMP_DIR/sh9" "$INSTALL_DIR/sh9"
  else
    info "Installing to ${INSTALL_DIR} (requires sudo)..."
    sudo mv "$TMP_DIR/db9" "$INSTALL_DIR/db9"
    [ "$SH9_OK" = "1" ] && sudo mv "$TMP_DIR/sh9" "$INSTALL_DIR/sh9"
  fi

  printf "\n"
  success "db9 installed successfully! ($(${INSTALL_DIR}/db9 --version 2>/dev/null || echo 'db9'))"
  if [ "$SH9_OK" = "1" ]; then
    success "sh9 installed successfully! ($(${INSTALL_DIR}/sh9 --version 2>/dev/null || echo 'sh9'))"
  fi
  printf "\n"
  printf "  Get started:\n"
  printf "    ${DIM}\$${RESET} db9 db create --name myapp\n"
  printf "    ${DIM}\$${RESET} db9 sh                       ${DIM}# filesystem shell${RESET}\n"
  printf "\n"
  printf "  ${DIM}No account needed — an anonymous account is created automatically.${RESET}\n"
  printf "  ${DIM}Claim it later with: db9 claim${RESET}\n"
  printf "\n"
}

main
