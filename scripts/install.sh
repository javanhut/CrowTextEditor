#!/bin/sh
# Build crow and install it to ~/.local/bin (or the directory given as $1).
set -e

cd "$(dirname "$0")/.."

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

# The file tree uses Nerd Font icons; offer the font if it's missing.
if [ "$(uname)" = "Darwin" ] && command -v brew >/dev/null 2>&1; then
  if ! ls "$HOME/Library/Fonts" /Library/Fonts 2>/dev/null | grep -qi "JetBrainsMono.*Nerd"; then
    printf "Install JetBrains Mono Nerd Font (file-tree icons)? [y/N] "
    read -r answer
    case "$answer" in
      y|Y) brew install --cask font-jetbrains-mono-nerd-font \
             && echo "Installed — select 'JetBrainsMono Nerd Font' in your terminal's settings." ;;
      *) echo "Skipped. Icons need a Nerd Font; set icons = false in crow.toml to hide them." ;;
    esac
  fi
else
  echo "Tip: tree icons need a Nerd Font (e.g. JetBrains Mono Nerd Font from nerdfonts.com);"
  echo "     set icons = false in crow.toml if you'd rather go without."
fi

echo "Done. Run: crow <file>   (config: ~/.config/crow/crow.toml, created on first run)"
