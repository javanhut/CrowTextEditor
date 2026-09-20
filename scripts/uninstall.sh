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

# Desktop entry (Linux): leaving it behind would keep crow in every app picker,
# pointing at a binary that is gone. Removing it also unmasks any system-wide
# copy, so rebuild mimeinfo.cache to match.
if [ "$(uname)" = "Linux" ]; then
  APP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
  if [ -f "$APP_DIR/crow.desktop" ]; then
    rm "$APP_DIR/crow.desktop"
    echo "Removed $APP_DIR/crow.desktop"
    if command -v update-desktop-database >/dev/null 2>&1; then
      update-desktop-database "$APP_DIR"
    fi
  fi
fi

CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/crow"
if [ -d "$CONFIG_DIR" ]; then
  printf "Also remove your config at %s? [y/N] " "$CONFIG_DIR"
  read -r answer || answer="" # no stdin (imlazy): keep the config
  case "$answer" in
    y|Y) rm -r "$CONFIG_DIR"; echo "Removed $CONFIG_DIR" ;;
    *) echo "Kept $CONFIG_DIR" ;;
  esac
fi
