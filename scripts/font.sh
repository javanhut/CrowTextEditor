#!/bin/sh
# Offer the Nerd Font the file tree's icons need (macOS, via Homebrew).
# Runs as the user: Homebrew refuses to run as root.
set -e

if ! command -v brew >/dev/null 2>&1; then
  echo "Tip: tree icons need a Nerd Font (e.g. JetBrains Mono Nerd Font from nerdfonts.com);"
  echo "     set icons = false in crow.toml if you'd rather go without."
  exit 0
fi

if ls "$HOME/Library/Fonts" /Library/Fonts 2>/dev/null | grep -qi "JetBrainsMono.*Nerd"; then
  exit 0
fi

printf "Install JetBrains Mono Nerd Font (file-tree icons)? [y/N] "
read -r answer || answer="" # no stdin: skip the font
case "$answer" in
  y|Y) brew install --cask font-jetbrains-mono-nerd-font \
         && echo "Installed — select 'JetBrainsMono Nerd Font' in your terminal's settings." ;;
  *) echo "Skipped. Icons need a Nerd Font; set icons = false in crow.toml to hide them." ;;
esac
