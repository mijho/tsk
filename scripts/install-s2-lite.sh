#!/bin/sh
set -eu

VERSION=0.42.12
SHA256=a01828ec01e72e11d247119cbd3784cf64c65e15e9255819b480132edc7c7da4
ARCHIVE="${TMPDIR:-/tmp}/s2-${VERSION}-x86_64-unknown-linux-gnu.zip"
INSTALL_ROOT="${S2_INSTALL_ROOT:-$HOME/.local/lib/s2/$VERSION}"
BIN_DIR="${S2_BIN_DIR:-$HOME/.local/bin}"
URL="https://github.com/s2-streamstore/s2/releases/download/s2-cli-v${VERSION}/s2-x86_64-unknown-linux-gnu.zip"

mkdir -p "$INSTALL_ROOT" "$BIN_DIR"
curl --fail --location --proto '=https' --tlsv1.2 "$URL" -o "$ARCHIVE"
printf '%s  %s\n' "$SHA256" "$ARCHIVE" | sha256sum -c -
unzip -p "$ARCHIVE" s2 > "$INSTALL_ROOT/s2"
chmod 0755 "$INSTALL_ROOT/s2"
ln -sfn "$INSTALL_ROOT/s2" "$BIN_DIR/s2"
"$BIN_DIR/s2" --version
