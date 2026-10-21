#!/bin/sh
# Build the 64-bit Linux and Windows binaries in a Linux container.
# Outputs:
#   dist/deskbridge-linux-x86_64
#   dist/deskbridge-windows-x86_64.exe
set -eu
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
mkdir -p "$root/dist"
docker run --platform linux/amd64 --rm \
  -v "$root":/src \
  -w /src \
  -e HOST_UID="$(id -u)" \
  -e HOST_GID="$(id -g)" \
  -e CARGO_TARGET_DIR=/tmp/deskbridge-target \
  rust:bookworm \
  bash -c '
    set -eu
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y --no-install-recommends gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64 ca-certificates pkg-config libpipewire-0.3-dev libclang-dev
    rustup target add x86_64-unknown-linux-gnu x86_64-pc-windows-gnu
    export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
    cargo build --locked --release --target x86_64-unknown-linux-gnu -p deskbridge
    cargo build --locked --release --target x86_64-pc-windows-gnu -p deskbridge
    cp /tmp/deskbridge-target/x86_64-unknown-linux-gnu/release/deskbridge /src/dist/deskbridge-linux-x86_64
    cp /tmp/deskbridge-target/x86_64-pc-windows-gnu/release/deskbridge.exe /src/dist/deskbridge-windows-x86_64.exe
    chown "$HOST_UID:$HOST_GID" /src/dist/deskbridge-linux-x86_64 /src/dist/deskbridge-windows-x86_64.exe
  '
echo "dist/deskbridge-linux-x86_64"
echo "dist/deskbridge-windows-x86_64.exe"
