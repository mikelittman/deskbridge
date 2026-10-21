#!/bin/sh
# Install the latest Deskbridge release for this Linux machine.
# Usage: curl -fsSL https://raw.githubusercontent.com/mikelittman/deskbridge/main/packaging/install-linux.sh | sh
set -eu

repo=mikelittman/deskbridge
dest_dir=${PREFIX:-$HOME/.local/bin}
arch=$(uname -m)
os=$(uname -s)

if [ "$os" != "Linux" ]; then
  echo "this installer is for Linux. On macOS use Homebrew. On Windows use Scoop." >&2
  exit 1
fi

case "$arch" in
  x86_64) asset=deskbridge-linux-x86_64 ;;
  aarch64 | arm64) asset=deskbridge-linux-aarch64 ;;
  *)
    echo "unsupported architecture: $arch" >&2
    exit 1
    ;;
esac

api="https://api.github.com/repos/${repo}/releases/latest"
tag=$(curl -fsSL "$api" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
if [ -z "$tag" ]; then
  echo "could not find the latest release" >&2
  exit 1
fi

base="https://github.com/${repo}/releases/download/${tag}"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -fsSL -o "$tmp/$asset" "${base}/${asset}"
curl -fsSL -o "$tmp/SHA256SUMS" "${base}/SHA256SUMS"
expected=$(awk -v name="$asset" '$2 == name { print $1 }' "$tmp/SHA256SUMS")
actual=$(sha256sum "$tmp/$asset" | awk '{ print $1 }')
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
  echo "checksum did not match for $asset" >&2
  exit 1
fi

mkdir -p "$dest_dir"
install -m 755 "$tmp/$asset" "$dest_dir/deskbridge"
echo "installed ${dest_dir}/deskbridge ${tag}"
if ! command -v deskbridge >/dev/null 2>&1; then
  echo "add ${dest_dir} to PATH, then run: deskbridge version"
fi
