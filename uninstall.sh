#!/bin/sh
# Remove the crow binary (and optionally its config).
set -e

BIN_DIR="${1:-$HOME/.local/bin}"

if [ -f "$BIN_DIR/crow" ]; then
  rm "$BIN_DIR/crow"
  echo "Removed $BIN_DIR/crow"
else
  echo "No crow binary at $BIN_DIR/crow"
fi

CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/crow"
if [ -d "$CONFIG_DIR" ]; then
  printf "Also remove your config at %s? [y/N] " "$CONFIG_DIR"
  read -r answer
  case "$answer" in
    y|Y) rm -r "$CONFIG_DIR"; echo "Removed $CONFIG_DIR" ;;
    *) echo "Kept $CONFIG_DIR" ;;
  esac
fi
