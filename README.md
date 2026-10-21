# Deskbridge

Deskbridge is a small remote desktop. One binary is both the host and the viewer. The host captures a Mac screen, encodes it with hardware HEVC when the Mac has that encoder and hardware H.264 otherwise, and sends the picture over a direct UDP connection. You connect with an IP address, a hostname, or a VPN you already run, such as Tailscale or WireGuard.

A still screen sends nothing. When the picture is moving, the host spends the bitrate on the part of the screen that is changing and slows the frame rate when a large area is in motion, so the pointer stays responsive. The cursor is drawn as a sprite on top of the picture.

## Features

- macOS host and macOS viewer in one binary, about 4 MB
- Hardware HEVC, with hardware H.264 when HEVC is unavailable
- Direct connection on UDP port 9787. No account server and no relay
- Pairing code, or SSH keys from an agent (including 1Password) and from a GitHub account
- Several viewers at once, sharing one capture
- The host keeps listening after a viewer disconnects
- Optional background host (`--daemon`)
- Viewer-selected picture size (`--resolution`)

Audio, clipboard sync, file transfer, and multiple monitors are not in this version.

## Requirements

- macOS on both machines. The host is the Mac whose screen you want to see.
- A Rust toolchain (`rustup`, stable) to build from source.
- Screen Recording permission on the host. `--inject` also needs Accessibility, so the viewer’s mouse and keyboard can drive the host.
- UDP port 9787 reachable from the viewer to the host.

Linux and Windows builds of the binary exist for the command-line tools. Capture and the viewer window are macOS-only.

## Build

```sh
cargo build --release
```

The binary is `target/release/deskbridge`. Release builds use thin LTO and strip symbols.

```sh
cargo test
```

To sign and notarize a copy you will move to another Mac, install a Developer ID Application certificate, store notary credentials once, and run the sign script:

```sh
xcrun notarytool store-credentials deskbridge
build/sign-macos.sh
```

That signs `target/release/deskbridge` as `com.deskbridge`. Screen Recording stays attached to that identity across later builds. Copy the signed file with `scp`. A browser or AirDrop download into `~/Downloads` is quarantined by macOS; `scp` onto a normal path is not.

64-bit Linux and Windows binaries, on a machine with Docker:

```sh
build/package-64.sh
```

Outputs:

- `dist/deskbridge-linux-x86_64`
- `dist/deskbridge-windows-x86_64.exe`

## Install

A push to `main` publishes a GitHub release. The tag is `v0.0.N`, where `N` increases with each release, so Homebrew and Scoop can tell which build is newer. A short commit hash does not sort that way. `deskbridge version` prints both, for example `0.0.12 (abc1234)`.

macOS, Apple Silicon, from a [Homebrew tap](https://github.com/mikelittman/deskbridge) of this repository:

```sh
brew tap mikelittman/deskbridge https://github.com/mikelittman/deskbridge
brew install mikelittman/deskbridge/deskbridge
```

Windows, with [Scoop](https://scoop.sh):

```sh
scoop bucket add deskbridge https://github.com/mikelittman/deskbridge
scoop install deskbridge
```

Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/mikelittman/deskbridge/main/packaging/install-linux.sh | sh
```

That installs `~/.local/bin/deskbridge`. Add that directory to `PATH` if the shell does not already search it.

## Quick start

On the Mac you want to watch:

```sh
deskbridge host --inject
```

The host listens on every interface, port 9787, and prints a pairing code. On the machine you are sitting at, use the host’s LAN or Tailscale address:

```sh
deskbridge view 100.x.x.x --code CODE
```

Click the window to grab the pointer. Ctrl+Alt releases it.

To try both sides on one Mac, leave `--inject` off and view `127.0.0.1`, so the viewer’s pointer cannot drive the same machine.

## Usage

```text
deskbridge host [--display N] [--listen addr:port] [--bitrate 20M] [--resolution 1080p] [--inject] [--daemon] [--key "ssh-ed25519 AAAA..."] [--github USER]
deskbridge view <addr[:port]> [--code CODE] [--key "ssh-ed25519 AAAA..."] [--fingerprint HEX] [--resolution 1080p]
deskbridge status
deskbridge stop
```

### Picture size and bitrate

`--bitrate` accepts `20M`, `500k`, or a raw bit count. The default is 20 Mbps. The host sends the next frame only as fast as the link and that budget can carry. The first picture is small so a slow link shows the desktop quickly. The full picture follows. A small change stays sharp. A large change lowers the frame rate so that region keeps its detail.

`--resolution` accepts `720p`, `1080p`, `1440p`, `2160p`, `4k`, or `1280x720`. On the viewer it is the picture size that viewer wants, often the client screen:

```sh
deskbridge view 100.x.x.x --resolution 1728x1117
```

On the host it is the largest picture the host will send. The host fits the display inside that box and keeps the aspect ratio. A request larger than the host display stays at the display’s own size. The pointer still covers the whole host display. With several viewers, the picture fits the smallest request and grows again when that viewer leaves.

The pixel size of the client Mac is the `Resolution:` line from:

```sh
system_profiler SPDisplaysDataType | awk '/Resolution:/ {print}'
```

### Several viewers

The pairing code stays the same until the host process exits. The host keeps listening after a viewer disconnects. Run `deskbridge view` again to reconnect. Viewers share one capture. Each viewer’s mouse and keyboard is injected when `--inject` is on. The cursor on the picture is the host Mac’s cursor. When the last viewer leaves, capture stops until the next connection.

Quitting the host, Ctrl+C, or `deskbridge stop` closes every viewer. If the host process is killed, each viewer closes after the connection times out, about 10 seconds.

### Background host

```sh
deskbridge host --daemon --inject
```

The command prints the pairing code and returns. The host keeps running after the terminal closes. `deskbridge status` prints the code again. `deskbridge stop` ends the host and closes every viewer.

From an SSH session, `--daemon` starts the host in the Mac’s desktop session, so Screen Recording applies to the `deskbridge` program. Log in on the Mac’s screen first. The permission prompt appears there.

State files (pid, pairing code, log) live in:

| System | Directory |
| --- | --- |
| macOS | `~/Library/Application Support/deskbridge/` |
| Linux | `$XDG_STATE_HOME/deskbridge` or `~/.local/state/deskbridge` |
| Windows | `%LOCALAPPDATA%\deskbridge` |

`DESKBRIDGE_STATE_DIR` overrides that directory.

`--listen 127.0.0.1` keeps the host on this Mac only. The default listens on every interface. `--listen 0.0.0.0:9787` is the same reachability, limited to IPv4.

### SSH keys

The host can admit a viewer that signs a one-time challenge with an `ssh-ed25519` private key. The host stores the public key. The private key stays in the SSH agent.

Authorize keys with `--key`, with `--github`, or with lines in `authorized_keys` in the state directory:

```sh
deskbridge host --daemon --inject --key "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... comment"
deskbridge host --daemon --inject --github yourname
```

`--github` reads `https://github.com/yourname.keys` when the host starts and authorizes every `ssh-ed25519` key on that account. Other key types are skipped. Repeat the flag for more than one account. Restart the host after adding a key on GitHub.

The viewer asks the agent in `SSH_AUTH_SOCK` and the 1Password agent socket, and signs with the key the host accepts. 1Password prompts before it signs.

```sh
deskbridge view 100.x.x.x
deskbridge view 100.x.x.x --key "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... comment"
```

One matching key is used automatically. With several matches, pass `--key` to choose. `--code` still admits a guest. A rejected login does not stop the host.

`--fingerprint` is optional. It is the SHA-256 of the host certificate, printed next to the pairing code. Pass it to pin a host you have seen before.

## Permissions

The host checks permissions before it listens.

- Screen Recording is required.
- Accessibility is required with `--inject`.

If a permission is off, the host opens the system prompt and exits. macOS applies the grant on the next launch, so start the host again after turning it on. From Terminal, the grant belongs to that terminal app. From SSH with `--daemon`, the grant belongs to the `deskbridge` program, and the prompt is on the Mac’s screen.

If the viewer prints `timed out`, the host is not receiving packets. Confirm the host log says it is listening on every interface, and allow incoming UDP 9787.

## Project layout

| Path | Role |
| --- | --- |
| `crates/deskbridge` | Command-line program |
| `crates/proto` | Wire messages |
| `crates/session` | QUIC session, pairing, SSH authentication |
| `crates/platform-macos` | ScreenCaptureKit, VideoToolbox, window, input |
| `crates/platform-linux` | Linux command-line stubs |
| `crates/platform-windows` | Windows command-line stubs |

## License

[MIT](LICENSE). Copyright 2026 Mike Littman.
