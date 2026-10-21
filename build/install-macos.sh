#!/bin/sh
# Put a deskbridge command on PATH. The program itself stays inside the
# stapled app, which is what macOS will allow to open.
set -eu
here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
src=$here/Deskbridge.app/Contents/MacOS/deskbridge
if [ ! -x "$src" ]; then
  echo "missing $src" >&2
  echo "copy Deskbridge.app and this script into the same directory" >&2
  exit 1
fi
if [ -w /usr/local/bin ] || [ -w /usr/local ]; then
  dest=/usr/local/bin/deskbridge
  mkdir -p /usr/local/bin
else
  dest=$HOME/.local/bin/deskbridge
  mkdir -p "$HOME/.local/bin"
fi
ln -sfn "$src" "$dest"
echo "deskbridge -> $src"
if command -v deskbridge >/dev/null 2>&1; then
  echo "run: deskbridge status"
else
  echo "add $(dirname "$dest") to PATH, then run: deskbridge status"
fi
