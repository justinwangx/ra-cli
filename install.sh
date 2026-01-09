#!/usr/bin/env sh

set -eu

REPO="${RA_REPO:-justinwangx/ra-cli}"
VERSION="${RA_VERSION:-latest}"

if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required to install ra." >&2
  exit 1
fi

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux) OS="linux" ;;
  Darwin) OS="macos" ;;
  *)
    echo "Unsupported OS: $OS" >&2
    exit 1
    ;;
esac

case "$ARCH" in
  x86_64|amd64) ARCH="x86_64" ;;
  arm64|aarch64) ARCH="aarch64" ;;
  *)
    echo "Unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

if [ "$OS" = "linux" ]; then
  TARGET="${ARCH}-unknown-linux-musl"
else
  TARGET="${ARCH}-apple-darwin"
fi

ARCHIVE="ra-${TARGET}.tar.gz"

if [ "$VERSION" = "latest" ]; then
  URL="https://github.com/${REPO}/releases/latest/download/${ARCHIVE}"
else
  URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARCHIVE}"
fi

TMPDIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMPDIR"
}
trap cleanup EXIT

echo "Downloading ${URL}..."
curl -fsSL "$URL" -o "$TMPDIR/${ARCHIVE}"

tar -xzf "$TMPDIR/${ARCHIVE}" -C "$TMPDIR"

if [ ! -f "$TMPDIR/ra" ]; then
  echo "ra binary not found in archive." >&2
  exit 1
fi

if [ -w "/usr/local/bin" ]; then
  INSTALL_DIR="/usr/local/bin"
else
  INSTALL_DIR="${HOME}/.local/bin"
fi

mkdir -p "$INSTALL_DIR"
mv "$TMPDIR/ra" "$INSTALL_DIR/ra"
chmod +x "$INSTALL_DIR/ra"

echo "Installed ra to ${INSTALL_DIR}/ra"
echo "Ensure ${INSTALL_DIR} is on your PATH."
