# Deskbridge

Deskbridge is a small remote desktop. One binary is both the host and the viewer. The host captures the screen of a Mac, a Linux desktop, or a Windows desktop, encodes it with hardware HEVC when that machine has the encoder and hardware H.264 otherwise, and sends the picture over a direct UDP connection. You connect with an IP address, a hostname, or a VPN you already run, such as Tailscale or WireGuard.

A still screen sends nothing. On a fast link the picture stays at the full frame rate and the bitrate you set. On a slow link the frame rate and bitrate drop, and the picture gets smaller if that is what keeps the view current. The cursor is drawn as a sprite on top of the picture. The viewer plays the host's system audio.

## Features

- Host and viewer in one binary on macOS, Linux, and Windows
- Hardware HEVC, with hardware H.264 when HEVC is unavailable
- Direct connection on UDP port 9787. No account server and no relay
- Pairing code, or SSH keys from an agent (including 1Password) and from a GitHub account
- Several viewers at once, sharing one capture
- The host keeps listening after a viewer disconnects
- Optional background host (`--daemon`)
- Viewer-selected picture size (`--resolution`)
- Host system audio on the viewer

Clipboard sync, file transfer, and multiple monitors are not in this version.

## Requirements

- The host and the viewer can each be macOS, Linux, or Windows. The host is the machine whose screen you want to see.
- A Rust toolchain (`rustup`, stable) to build from source.
- On macOS, Screen Recording permission on the host. `--inject` also needs Accessibility.
- On Linux, a logged-in desktop (Wayland or X11) with PipeWire. The portal asks before capture. Hardware H.264 or HEVC through VA-API. `libpipewire` is required to start the binary.
- On Windows, a signed-in desktop session. Hardware H.264 or HEVC through Media Foundation. Capture uses Windows.Graphics.Capture.
- UDP port 9787 reachable from the viewer to the host.

A machine without a hardware H.264 or HEVC encoder cannot host. The viewer plays the host's system audio on each of these systems.

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

That signs `target/release/deskbridge` as `com.deskbridge`. Screen Recording stays attached to that identity across later builds. The release workflow signs and notarizes the macOS binary the same way. Add these repository secrets before the first release:

| Secret | Contents |
| --- | --- |
| `APPLE_CERTIFICATE_BASE64` | Developer ID Application certificate and private key, exported as a `.p12` and base64-encoded |
| `APPLE_CERTIFICATE_PASSWORD` | Password for that `.p12` |
| `APPLE_API_KEY` | App Store Connect API private key (`.p8` file contents) |
| `APPLE_API_KEY_ID` | Key ID for that API key |
| `APPLE_API_ISSUER` | Issuer ID for that API key |

Export the certificate from Keychain Access as a `.p12`, then:

```sh
base64 -i Certificates.p12 | gh secret set APPLE_CERTIFICATE_BASE64
gh secret set APPLE_CERTIFICATE_PASSWORD
gh secret set APPLE_API_KEY < AuthKey_XXXX.p8
gh secret set APPLE_API_KEY_ID
gh secret set APPLE_API_ISSUER
```

Do not commit the `.p12` or the `.p8`. Copy a signed build with `scp`. A browser or AirDrop download into `~/Downloads` is quarantined by macOS; `scp` onto a normal path is not.

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

`--bitrate` accepts `20M`, `500k`, or a raw bit count. The default is 20 Mbps, and that is the quality a fast link aims for. On a fast link the picture runs at the display frame rate, including while a large area of the screen is changing, and the host keeps enough frames in flight to cover the round trip. On a slow link it keeps a single frame in flight, lowers the bitrate and the frame rate, and sends a smaller picture if the link stays slow, so the desktop you see stays as current as the path allows. The first picture is small so a slow link shows the desktop quickly. The full picture follows. A small change stays sharp.

The viewer plays the host's system audio. Sound is AAC at about 96 kbps and is sent ahead of the picture, so a slow link shrinks the video before it crowds out the audio. A quiet host still sends that stream while a viewer is connected.

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

The pairing code stays the same until the host process exits. The host keeps listening after a viewer disconnects. Run `deskbridge view` again to reconnect. Viewers share one capture. Each viewer’s mouse and keyboard is injected when `--inject` is on. The cursor on the picture is the host’s cursor. When the last viewer leaves, capture stops until the next connection.

Quitting the host, Ctrl+C, or `deskbridge stop` closes every viewer. If the host process is killed, each viewer closes after the connection times out, about 10 seconds.

### Background host

```sh
deskbridge host --daemon --inject
```

The command prints the pairing code and returns. The host keeps running after the terminal closes. `deskbridge status` prints the code again. `deskbridge stop` ends the host and closes every viewer.

From a shell that is not the desktop session, `--daemon` starts the host in that session. On macOS it uses a launchd GUI job, so Screen Recording applies to the `deskbridge` program and the permission prompt appears on the Mac’s screen. On Linux it starts a systemd user service. On Windows it runs a task in the signed-in user session. Log in on the screen first.

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
| `crates/platform-linux` | PipeWire, VA-API, portal input, viewer window |
| `crates/platform-windows` | Windows.Graphics.Capture, Media Foundation, WASAPI, viewer window |

## License

[MIT](LICENSE). Copyright 2026 Mike Littman.
