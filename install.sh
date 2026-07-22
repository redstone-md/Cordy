#!/bin/sh
# Cordy installer for Linux and macOS.
#   curl -fsSL https://raw.githubusercontent.com/redstone-md/Cordy/main/install.sh | sh
# Override the install directory with CORDY_INSTALL_DIR (default: ~/.local/bin).
set -eu

REPO="redstone-md/Cordy"
BIN="cordy"

say() { printf '\033[1;34m::\033[0m %s\n' "$1"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$1" >&2; exit 1; }

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
      *) die "no prebuilt binary for linux/$arch — install with: cargo install $BIN" ;;
    esac ;;
  Darwin)
    case "$arch" in
      x86_64) target="x86_64-apple-darwin" ;;
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      *) die "no prebuilt binary for macos/$arch" ;;
    esac ;;
  *) die "unsupported OS: $os — install with: cargo install $BIN" ;;
esac

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar  >/dev/null 2>&1 || die "tar is required"

say "resolving the latest release"
tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
  | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name":[[:space:]]*"([^"]+)".*/\1/')"
[ -n "$tag" ] || die "could not resolve the latest release tag"

asset="${BIN}-${tag}-${target}.tar.gz"
base="https://github.com/$REPO/releases/download/$tag"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "downloading $asset"
curl -fsSL "$base/$asset" -o "$tmp/$asset" || die "download failed: $base/$asset"

# Verify the checksum when SHA256SUMS is present.
sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
           elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1; fi; }
if curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" 2>/dev/null; then
  want="$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1 || true)"
  got="$(sha256 "$tmp/$asset")"
  if [ -n "$want" ] && [ -n "$got" ] && [ "$want" != "$got" ]; then
    die "checksum mismatch for $asset"
  fi
  [ -n "$want" ] && say "checksum verified"
fi

tar -xzf "$tmp/$asset" -C "$tmp"
found="$(find "$tmp" -type f -name "$BIN" | head -n1)"
[ -n "$found" ] || die "binary not found in archive"

dir="${CORDY_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$dir"
install -m 0755 "$found" "$dir/$BIN" 2>/dev/null || { cp "$found" "$dir/$BIN"; chmod 0755 "$dir/$BIN"; }

say "installed $BIN $tag -> $dir/$BIN"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) printf '\n  add it to your PATH:\n    export PATH="%s:$PATH"\n\n' "$dir" ;;
esac
