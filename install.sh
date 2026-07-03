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
command -v rust-analyzer >/dev/null 2>&1 || \
  echo "rdbg: also install rust-analyzer for navigation:  rustup component add rust-analyzer" >&2

# codelldb — the debug adapter, auto-installed so `eval` gets full Rust expression
# eval (comparisons `a == b`, tuple `x.0`, method calls), not just variable paths.
# Kept in its own dir so it finds its bundled liblldb; rdbg finds it automatically.
cl_home="$HOME/.local/share/rdbg/codelldb"
if [ -n "${RDBG_NO_CODELLDB:-}" ]; then
  echo "rdbg: skipping codelldb (RDBG_NO_CODELLDB set) — rdbg will use lldb-dap if present" >&2
elif [ -x "$cl_home/extension/adapter/codelldb" ] || command -v codelldb >/dev/null 2>&1; then
  echo "rdbg: codelldb already present (full Rust expression eval available)" >&2
else
  case "$os" in Darwin) cl_os="darwin" ;; Linux) cl_os="linux" ;; *) cl_os="" ;; esac
  case "$arch" in arm64|aarch64) cl_arch="arm64" ;; x86_64) cl_arch="x64" ;; *) cl_arch="" ;; esac
  if [ -n "$cl_os" ] && [ -n "$cl_arch" ] && command -v unzip >/dev/null 2>&1; then
    cl_url="https://github.com/vadimcn/codelldb/releases/latest/download/codelldb-${cl_os}-${cl_arch}.vsix"
    echo "rdbg: installing codelldb (${cl_arch}-${cl_os}) for full Rust expression eval …" >&2
    if curl -fsSL "$cl_url" -o "$tmp/codelldb.vsix"; then
      rm -rf "$cl_home"; mkdir -p "$cl_home"
      unzip -oq "$tmp/codelldb.vsix" -d "$cl_home"
      chmod +x "$cl_home/extension/adapter/codelldb" 2>/dev/null || true
      if [ -x "$cl_home/extension/adapter/codelldb" ]; then
        echo "rdbg: codelldb installed to $cl_home" >&2
      else
        echo "rdbg: codelldb extract failed — install lldb-dap (Xcode CLT / 'apt install lldb') as a fallback" >&2
      fi
    else
      echo "rdbg: codelldb download failed — install lldb-dap for variable-path eval, or re-run to retry" >&2
    fi
  else
    echo "rdbg: needs unzip + a supported platform to auto-install codelldb; install codelldb or lldb-dap manually" >&2
  fi
fi
