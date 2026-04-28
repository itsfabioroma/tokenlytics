#!/bin/sh
# tokenlytics installer · curl -fsSL https://ultracontext.com/tokenlytics.sh | sh
# detects OS/arch, downloads the matching binary from the latest GitHub release,
# drops it in ~/.local/bin (override via $TOKENLYTICS_INSTALL_DIR).
# POSIX sh compatible (works with dash, bash, ash, zsh).

set -eu

REPO="ultracontext/tokenlytics"

# pick install dir: env override > existing tokenlytics location > ~/.local/bin
if [ -n "${TOKENLYTICS_INSTALL_DIR:-}" ]; then
  DEST_DIR="$TOKENLYTICS_INSTALL_DIR"
elif EXISTING="$(command -v tokenlytics 2>/dev/null)" && [ -n "$EXISTING" ]; then
  DEST_DIR="$(dirname "$EXISTING")"
else
  DEST_DIR="$HOME/.local/bin"
fi
DEST="$DEST_DIR/tokenlytics"

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$OS-$ARCH" in
  darwin-arm64)              TRIPLE="aarch64-apple-darwin" ;;
  darwin-x86_64)             TRIPLE="x86_64-apple-darwin" ;;
  linux-x86_64|linux-amd64)  TRIPLE="x86_64-unknown-linux-gnu" ;;
  *)
    echo "tokenlytics: unsupported platform $OS-$ARCH" >&2
    echo "             open an issue: https://github.com/$REPO/issues" >&2
    exit 1
    ;;
esac

URL="https://github.com/$REPO/releases/latest/download/tokenlytics-$TRIPLE"

echo "tokenlytics: fetching $URL"
mkdir -p "$DEST_DIR"

# download to a temp path, then atomic rename so an in-place upgrade survives
# (macOS refuses to overwrite a running executable; mv -f swaps inodes safely).
TMP="$DEST.new"
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$URL" -o "$TMP"
elif command -v wget >/dev/null 2>&1; then
  wget -q "$URL" -O "$TMP"
else
  echo "tokenlytics: need curl or wget" >&2
  exit 1
fi

chmod +x "$TMP"
mv -f "$TMP" "$DEST"

echo "✓ tokenlytics installed at $DEST"

case ":$PATH:" in
  *":$DEST_DIR:"*) ;;
  *)
    echo
    echo "  add $DEST_DIR to PATH:"
    echo "    echo 'export PATH=\"$DEST_DIR:\$PATH\"' >> ~/.zshrc  # or ~/.bashrc"
    ;;
esac

echo
echo "  start: tokenlytics"
