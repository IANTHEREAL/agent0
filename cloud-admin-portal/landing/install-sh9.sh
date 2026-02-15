#!/bin/sh
set -e

# sh9 installer
# Usage: curl -fsSL https://db9.shared.aws.tidbcloud.com/install-sh9 | sh

BASE_URL="https://db9.shared.aws.tidbcloud.com/releases"
INSTALL_DIR="${SH9_INSTALL_DIR:-/usr/local/bin}"

RED='\033[0;31m'
GREEN='\033[0;32m'
DIM='\033[2m'
BOLD='\033[1m'
RESET='\033[0m'

info()    { printf "  ${DIM}%s${RESET}\n" "$1"; }
success() { printf "  ${GREEN}%s${RESET}\n" "$1"; }
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
  printf "  ${BOLD}sh9${RESET} installer\n"
  printf "  ${DIM}────────────────────────────${RESET}\n"
  printf "\n"

  OS=$(detect_os)
  ARCH=$(detect_arch)
  info "Platform: ${OS}/${ARCH}"

  DOWNLOAD_URL="${BASE_URL}/sh9-${OS}-${ARCH}"

  if [ ! -d "$INSTALL_DIR" ]; then
    mkdir -p "$INSTALL_DIR" 2>/dev/null || sudo mkdir -p "$INSTALL_DIR"
  fi

  TMP_DIR=$(mktemp -d)
  trap 'rm -rf "$TMP_DIR"' EXIT

  info "Downloading sh9..."
  if ! download "$DOWNLOAD_URL" "$TMP_DIR/sh9"; then
    error "No pre-built binary available for ${OS}/${ARCH}.\n  Available: linux/amd64, linux/arm64, darwin/amd64, darwin/arm64"
  fi

  chmod +x "$TMP_DIR/sh9"

  if [ -w "$INSTALL_DIR" ]; then
    mv "$TMP_DIR/sh9" "$INSTALL_DIR/sh9"
  else
    info "Installing to ${INSTALL_DIR} (requires sudo)..."
    sudo mv "$TMP_DIR/sh9" "$INSTALL_DIR/sh9"
  fi

  printf "\n"
  success "sh9 installed successfully! ($(${INSTALL_DIR}/sh9 --version 2>/dev/null || echo 'sh9'))"
  printf "\n"
  printf "  Usage:\n"
  printf "    ${DIM}\$${RESET} db9 sh             ${DIM}# launch via db9${RESET}\n"
  printf "    ${DIM}\$${RESET} sh9 --server <url>  ${DIM}# connect directly${RESET}\n"
  printf "\n"
}

main
