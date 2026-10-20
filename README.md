# Deskbridge

A small remote desktop. Phase 1 is a macOS host and a macOS viewer in one binary. The picture is hardware HEVC when this Mac has an encoder, and hardware H.264 otherwise. A still screen sends no video. The cursor is a sprite, not part of the picture.

Connections are direct: an IP address, a hostname, or a VPN you already have (Tailscale, WireGuard). There is no relay and no NAT traversal.

## Run

On the machine you want to watch:

```
deskbridge host --inject
```

The host listens on every interface, port 9787, and prints a pairing code. On the machine you are sitting at, use the host's LAN or Tailscale address:

```
deskbridge view 100.x.x.x --code CODE
```

Click the window to grab the pointer. Ctrl+Alt releases it. UDP port 9787 has to be open between the two machines. If the viewer prints `timed out`, the host is not receiving packets: restart it so the log says it is listening on every interface, and allow incoming connections for the terminal in System Settings if macOS asks.

```
deskbridge host [--display N] [--listen addr:port] [--bitrate 20M] [--resolution 1080p] [--inject] [--daemon] [--key "ssh-ed25519 AAAA..."] [--github USER]
deskbridge status
deskbridge stop
deskbridge view <addr[:port]> [--code CODE] [--key "ssh-ed25519 AAAA..."] [--fingerprint HEX]
```

`--bitrate` accepts `20M`, `500k`, or a raw bit count. The default is 20 Mbps. The host sends the next frame only as fast as the link and `--bitrate` can carry. The first picture is small, so a slow link shows the desktop quickly, and the full picture follows. A small change stays sharp and goes out often. When a large part of the screen is changing, the frame rate drops so that region keeps its detail and the pointer keeps moving. Input is applied as soon as it arrives.

`--resolution` accepts `720p`, `1080p`, `1440p`, `2160p`, `4k`, or `1280x720`. The host fits the picture inside that box and keeps the display's aspect ratio. A limit larger than the display leaves the picture at the display's own size. The pointer still covers the whole display.

The same pairing code admits every viewer until the host process exits. The host keeps listening after a viewer disconnects, in the foreground and as a daemon. Run `deskbridge view` again to reconnect. Several viewers can be connected at once. They share one capture, and each viewer's mouse and keyboard is injected. The cursor drawn on the picture is the cursor on the host Mac. When the last viewer leaves, capture stops until the next connection. Every viewer receives the host's hardware codec, HEVC when this Mac can encode it. A viewer that cannot decode that codec is closed, and the host keeps listening.

Quitting the host, Ctrl+C, or `deskbridge stop` closes every viewer. If the host process is killed, each viewer closes once the connection times out, about 10 seconds.

## Background host

```
deskbridge host --daemon --inject
```

The command prints the pairing code and returns. The host keeps running after the terminal closes, and after a viewer disconnects. `deskbridge status` prints the code again. `deskbridge stop` ends the host and closes every connected viewer.

On macOS the pid, state, and log are in `~/Library/Application Support/deskbridge/`. On Linux they are in `$XDG_STATE_HOME/deskbridge` or `~/.local/state/deskbridge`. On Windows they are in `%LOCALAPPDATA%\deskbridge`. `DESKBRIDGE_STATE_DIR` overrides that directory.

To try it on one Mac, leave `--inject` off and view `127.0.0.1`. Injection is off so the viewer's pointer cannot drive the host and feed back into itself. `--inject` posts mouse and keyboard into the host Mac. That is the setup for two machines.

`--listen 127.0.0.1` keeps the host on this Mac only. `--listen 0.0.0.0:9787` is the same reachability as the default, limited to IPv4.

`--fingerprint` is optional. It is the SHA-256 of the host certificate, printed next to the pairing code. Pass it when you want to pin a host you have seen before.

## SSH keys

The host can admit a viewer that signs a one-time challenge with an `ssh-ed25519` private key. The host keeps only the public key. The private key stays in the SSH agent.

Authorize a key by repeating `--key` with the public-key line, by naming a GitHub account, or by putting lines in `authorized_keys` in the state directory (`~/Library/Application Support/deskbridge/` on macOS):

```
deskbridge host --daemon --inject --key "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... comment"
deskbridge host --daemon --inject --github yourname
```

`--github` reads `https://github.com/yourname.keys` when the host starts and authorizes every `ssh-ed25519` key on that account. Other key types on the account are skipped. Repeat the flag for more than one account. Restart the host after adding a key on GitHub.

On the viewer, Deskbridge asks the agent in `SSH_AUTH_SOCK` and the 1Password agent socket, and signs with the one that holds the matching private key. 1Password prompts before it signs. The host sends the public keys it accepts.

```
deskbridge view 100.x.x.x
deskbridge view 100.x.x.x --key "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... comment"
```

With one matching key, the viewer uses it. With several, pass `--key` to choose. `--code` still works for a guest, including when the host also has authorized keys. A rejected proof does not stop the host; it waits for the next viewer.

## Permissions

The host checks permissions before it listens. It needs Screen Recording. `--inject` also needs Accessibility, so the host can post mouse and keyboard events. Loopback on one Mac does not need Accessibility.

If a permission is off, the host opens the system prompt and exits. macOS applies the grant on the next launch, so start `deskbridge host` again after turning it on for the terminal that runs it.

## What this build does not do

Audio, clipboard, file transfer, multiple monitors, mobile viewers, discovery, NAT traversal, and a software encoder. Those stay off this binary so it can stay small. A later build can add OpenH264 for machines with no hardware encoder. This one will not open a software HEVC or H.264 encoder.

Linux and Windows 64-bit binaries build and run the protocol tools. Screen capture and the viewer window are still the macOS backend. `crates/platform-linux` is the home for PipeWire or X11 capture with VAAPI or OpenH264. `crates/platform-windows` is the home for Windows.Graphics.Capture and Media Foundation.

## Build

```
cargo test
cargo build --release
```

The macOS release binary is `target/release/deskbridge`. It is built with thin LTO and stripped symbols.

64-bit Linux and Windows binaries, from a machine with Docker:

```
build/package-64.sh
```

That writes `dist/deskbridge-linux-x86_64` and `dist/deskbridge-windows-x86_64.exe`.

Release binary size: 3.7 MB (3,901,488 bytes) on Apple Silicon.

Deskbridge is licensed under the MIT License. See `LICENSE`.
