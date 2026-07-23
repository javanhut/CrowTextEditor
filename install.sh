#!/bin/sh
# Build crow and install it to ~/.local/bin (or the directory given as $1).
set -e

cd "$(dirname "$0")"

if ! command -v cargo >/dev/null 2>&1; then
  echo "crow needs the Rust toolchain to build. Install it from https://rustup.rs, then rerun."
  exit 1
fi

BIN_DIR="${1:-$HOME/.local/bin}"

echo "Building crow (release)…"
cargo build --release

mkdir -p "$BIN_DIR"
cp target/release/crow "$BIN_DIR/crow"
echo "Installed $BIN_DIR/crow"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "Note: $BIN_DIR is not on your PATH — add: export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

if ! command -v rust-analyzer >/dev/null 2>&1; then
  echo "Optional: install rust-analyzer for LSP support:  rustup component add rust-analyzer"
fi

echo "Done. Run: crow <file>   (config: ~/.config/crow/crow.toml, created on first run)"
