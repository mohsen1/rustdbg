#!/bin/sh
# Install rdbg (rust-debugger-skill). Usage:
#   curl -fsSL https://azimi.me/rust-debugger-skill/install.sh | sh
set -e

repo="mohsen1/rust-debugger-skill"
bin="rdbg"
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64)        target="x86_64-apple-darwin" ;;
      *) echo "rdbg: unsupported macOS arch: $arch" >&2; exit 1 ;;
    esac ;;
  Linux)
    case "$arch" in
      x86_64)        target="x86_64-unknown-linux-musl" ;;
      aarch64|arm64) target="aarch64-unknown-linux-musl" ;;
      *) echo "rdbg: unsupported Linux arch: $arch" >&2; exit 1 ;;
    esac ;;
  *) echo "rdbg: unsupported OS: $os" >&2; exit 1 ;;
esac

url="https://github.com/$repo/releases/latest/download/${bin}-${target}.tar.gz"
dir="${RDBG_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$dir"

echo "rdbg: downloading ${bin}-${target}" >&2
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "$url" | tar -xz -C "$tmp"
install -m 755 "$tmp/$bin" "$dir/$bin"

echo "rdbg: installed to $dir/$bin" >&2
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "rdbg: add $dir to your PATH" >&2 ;;
esac
echo "rdbg: also needs rust-analyzer (rustup component add rust-analyzer) and lldb-dap" >&2
