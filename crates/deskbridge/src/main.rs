#[cfg(all(
    any(target_os = "linux", target_os = "windows"),
    not(target_pointer_width = "64")
))]
compile_error!("Linux and Windows builds are 64-bit only");

#[cfg(target_os = "macos")]
mod view;
mod daemon;
mod error;

use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::process::ExitCode;
use std::time::Duration;

use crate::error::PlatformError;
use deskbridge_proto::{CodecId, Input};
use deskbridge_session::{mono_us, parse_public_key_line, AuthorizedKey};
#[cfg(target_os = "macos")]
use deskbridge_session::{parse_authorized_file, public_key_line};

#[cfg(target_os = "macos")]
use deskbridge_platform_macos::{ensure_host_permissions, require_encode_codec, Host, HostOptions, Outbound};
#[cfg(target_os = "macos")]
use deskbridge_proto::{negotiate_codec, Control};
#[cfg(target_os = "macos")]
use deskbridge_session::{accept_one, listen, Identity};

const DEFAULT_PORT: u16 = 9787;

fn main() -> ExitCode {
    match dispatch() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("deskbridge: {err}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch() -> Result<(), PlatformError> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("host") => host(args.collect()),
        Some("view") => view_command(args.collect()),
        Some("status") => daemon::status(),
        Some("stop") => daemon::stop(),
        Some("-h") | Some("--help") | None => {
            print_help();
            Ok(())
        }
        Some(other) => Err(PlatformError::new(format!(
            "unknown command {other}. Use `deskbridge host`, `deskbridge view`, `deskbridge status`, or `deskbridge stop`."
        ))),
    }
}

fn view_command(args: Vec<String>) -> Result<(), PlatformError> {
    #[cfg(target_os = "macos")]
    {
        view::view(args)
    }
    #[cfg(target_os = "linux")]
    {
        let _ = args;
        Err(PlatformError::new(deskbridge_platform_linux::VIEWER_UNAVAILABLE))
    }
    #[cfg(target_os = "windows")]
    {
        let _ = args;
        Err(PlatformError::new(
            deskbridge_platform_windows::VIEWER_UNAVAILABLE,
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = args;
        Err(PlatformError::new(
            "the viewer is not built for this operating system",
        ))
    }
}

fn print_help() {
    eprintln!(
        "\
deskbridge host [--display N] [--listen addr:port] [--bitrate 20M] [--resolution 1080p] [--inject] [--daemon] [--key \"ssh-ed25519 AAAA...\"]
deskbridge status
deskbridge stop
deskbridge view <addr[:port]> [--code CODE] [--key \"ssh-ed25519 AAAA...\"] [--fingerprint HEX]

Direct connection only. Default port is {DEFAULT_PORT}. The host listens on every interface.
The host prints a pairing code. Ctrl+Alt releases the viewer's pointer grab.
--daemon keeps the host running after the terminal closes.
`deskbridge status` prints the pairing code. `deskbridge stop` ends that host.
--key on the host authorizes an ssh-ed25519 public key. Repeat it for more than one.
The host also reads authorized_keys in its state directory.
The viewer signs with the SSH agent, including 1Password. --key there picks which agent key.
--code still admits a guest. --resolution is 720p, 1080p, 1440p, 2160p, 4k, or WxH.
Quitting the host ends the viewer. A killed host ends the viewer after the connection times out."
    );
}

struct HostArgs {
    display: usize,
    listen: SocketAddr,
    bitrate: u32,
    resolution: Option<(u32, u32)>,
    inject: bool,
    daemon: bool,
    keys: Vec<AuthorizedKey>,
}

fn host(args: Vec<String>) -> Result<(), PlatformError> {
    let args = parse_host(&args)?;
    #[cfg(target_os = "macos")]
    {
        if args.daemon && !daemon::is_child() {
            ensure_host_permissions(args.inject).map_err(PlatformError::new)?;
            return daemon::spawn();
        }
        return run_host(args);
    }
    #[cfg(target_os = "linux")]
    {
        let _ = args;
        Err(PlatformError::new(deskbridge_platform_linux::HOST_UNAVAILABLE))
    }
    #[cfg(target_os = "windows")]
    {
        let _ = args;
        Err(PlatformError::new(deskbridge_platform_windows::HOST_UNAVAILABLE))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = args;
        Err(PlatformError::new(
            "the host is not built for this operating system",
        ))
    }
}

#[cfg(target_os = "macos")]
fn run_host(args: HostArgs) -> Result<(), PlatformError> {
    let session_files = if daemon::is_child() {
        Some(daemon::prepare_child()?)
    } else {
        None
    };
    ensure_host_permissions(args.inject).map_err(PlatformError::new)?;
    let codecs = require_encode_codec().map_err(PlatformError::new)?;
    let identity = Identity::generate().map_err(PlatformError::new)?;
    let authorized = authorized_keys(args.keys)?;
    println!("pairing code: {}", identity.code);
    println!("fingerprint:  {}", identity.fingerprint_hex());
    for key in &authorized {
        let line = public_key_line(&key.blob, &key.comment).map_err(PlatformError::new)?;
        println!("authorized ssh key: {line}");
    }
    print_listen(&args.listen);
    if !args.inject {
        println!("input injection is off. Pass --inject to drive this Mac.");
    }
    if let Some((width, height)) = args.resolution {
        println!("resolution limit {width}x{height} (keeps aspect, never larger than the display)");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(PlatformError::new)?;
    let endpoint = {
        let _guard = runtime.enter();
        match listen(args.listen, &identity) {
            Ok(endpoint) => endpoint,
            Err(err) if args.listen.is_ipv6() && args.listen.ip().is_unspecified() => {
                let v4 = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), args.listen.port());
                eprintln!("IPv6 listen failed ({err}). Listening on {v4} instead.");
                listen(v4, &identity).map_err(PlatformError::new)?
            }
            Err(err) => return Err(PlatformError::new(err)),
        }
    };
    if let Some(files) = &session_files {
        let listen = endpoint
            .local_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|_| args.listen.to_string());
        files.publish(&identity.code, &identity.fingerprint_hex(), &listen)?;
    }
    runtime.block_on(async move {
        let shutdown = shutdown_requested();
        tokio::pin!(shutdown);
        let mut paired = loop {
            tokio::select! {
                _ = &mut shutdown => {
                    println!("stopping");
                    return Ok(());
                }
                result = accept_one(&endpoint, &identity, codecs.clone(), &authorized) => {
                    match result {
                        Ok(paired) => break paired,
                        Err(err) if err.to_string().contains("endpoint closed") => {
                            return Err(PlatformError::new(err));
                        }
                        Err(err) => eprintln!("deskbridge: {err}"),
                    }
                }
            }
        };
        let codec = negotiate_codec(&codecs, &paired.peer_codecs).ok_or_else(|| {
            PlatformError::new("the viewer cannot decode H.264 or HEVC from this host")
        })?;
        match &paired.viewer {
            deskbridge_session::ViewerProof::PairingCode => {
                println!("paired by pairing code, encoding {}", codec.name());
            }
            deskbridge_session::ViewerProof::SshKey { comment } if comment.is_empty() => {
                println!("paired by ssh key, encoding {}", codec.name());
            }
            deskbridge_session::ViewerProof::SshKey { comment } => {
                println!("paired by ssh key ({comment}), encoding {}", codec.name());
            }
        }
        let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let capture = Host::spawn(
            HostOptions {
                display_index: args.display,
                bitrate: args.bitrate,
                inject: args.inject,
                codec,
                resolution: args.resolution,
            },
            outbound_tx,
        )
        .map_err(PlatformError::new)?;
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    println!("stopping");
                    break;
                }
                incoming = paired.from_peer.recv() => {
                    let Some(message) = incoming else { break };
                    match message {
                        Control::Input(input) => capture.inject(input),
                        Control::KeyframeRequest { .. } => capture.request_keyframe(),
                        _ => {}
                    }
                }
                outgoing = outbound_rx.recv() => {
                    let Some(outgoing) = outgoing else { break };
                    match outgoing {
                        Outbound::Control(message) => paired.send_control(message),
                        Outbound::Frame(frame) => paired.send_frame(frame),
                    }
                }
            }
        }
        drop(capture);
        paired.shutdown().await;
        let _ = tokio::time::timeout(Duration::from_millis(500), endpoint.wait_idle()).await;
        Ok(())
    })?;
    drop(session_files);
    Ok(())
}

#[cfg(target_os = "macos")]
async fn shutdown_requested() {
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    let mut ctrl_c_ok = true;
    #[cfg(unix)]
    let mut terminate = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(signal) => Some(signal),
        Err(err) => {
            eprintln!("deskbridge: stop signal is unavailable ({err})");
            None
        }
    };
    loop {
        tokio::select! {
            result = &mut ctrl_c, if ctrl_c_ok => {
                match result {
                    Ok(()) => return,
                    Err(err) => {
                        eprintln!("deskbridge: ctrl-c is unavailable ({err})");
                        ctrl_c_ok = false;
                    }
                }
            }
            _ = async {
                #[cfg(unix)]
                {
                    if let Some(signal) = terminate.as_mut() {
                        signal.recv().await;
                        return;
                    }
                }
                std::future::pending::<()>().await
            } => return,
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                if daemon::is_child() && daemon::stop_requested() {
                    return;
                }
            }
        }
    }
}

fn parse_host(args: &[String]) -> Result<HostArgs, PlatformError> {
    let mut display = 0usize;
    let mut listen = format!("[::]:{DEFAULT_PORT}");
    let mut bitrate = 20_000_000u32;
    let mut resolution = None;
    let mut inject = false;
    let mut daemon = false;
    let mut keys = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--inject" => inject = true,
            "--daemon" => daemon = true,
            "--key" => {
                let line = take_value(args, &mut index, "--key")?;
                keys.push(parse_public_key_line(&line).map_err(PlatformError::new)?);
            }
            "--display" => {
                display = take_value(args, &mut index, "--display")?
                    .parse()
                    .map_err(|_| PlatformError::new("--display needs a number"))?;
            }
            "--listen" => listen = take_value(args, &mut index, "--listen")?,
            "--bitrate" => {
                bitrate = parse_bitrate(&take_value(args, &mut index, "--bitrate")?)?;
            }
            "--resolution" => {
                resolution = Some(parse_resolution(&take_value(
                    args,
                    &mut index,
                    "--resolution",
                )?)?);
            }
            other => {
                return Err(PlatformError::new(format!("unknown host option {other}")));
            }
        }
        index += 1;
    }
    Ok(HostArgs {
        display,
        listen: resolve_socket(&listen)?,
        bitrate,
        resolution,
        inject,
        daemon,
        keys,
    })
}

#[cfg(target_os = "macos")]
fn authorized_keys(from_flags: Vec<AuthorizedKey>) -> Result<Vec<AuthorizedKey>, PlatformError> {
    let mut keys = read_authorized_keys_file()?;
    for key in from_flags {
        if keys.iter().any(|have| have.blob == key.blob) {
            continue;
        }
        keys.push(key);
    }
    if keys.len() > 32 {
        return Err(PlatformError::new("at most 32 authorized SSH keys"));
    }
    Ok(keys)
}

#[cfg(target_os = "macos")]
fn read_authorized_keys_file() -> Result<Vec<AuthorizedKey>, PlatformError> {
    let paths = daemon::paths()?;
    let file = paths.dir.join("authorized_keys");
    if !file.is_file() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&file).map_err(PlatformError::new)?;
    parse_authorized_file(&text)
        .map_err(|err| PlatformError::new(format!("{}: {err}", file.display())))
}

pub fn parse_resolution(text: &str) -> Result<(u32, u32), PlatformError> {
    let text = text.trim();
    let preset = match text.to_ascii_lowercase().as_str() {
        "720p" => Some((1280, 720)),
        "1080p" => Some((1920, 1080)),
        "1440p" => Some((2560, 1440)),
        "2160p" | "4k" => Some((3840, 2160)),
        _ => None,
    };
    if let Some(size) = preset {
        return Ok(size);
    }
    let Some((width, height)) = text.split_once(['x', 'X']) else {
        return Err(PlatformError::new(
            "--resolution needs WxH, for example 1280x720, or 720p, 1080p, 1440p, 2160p, 4k",
        ));
    };
    let width: u32 = width
        .trim()
        .parse()
        .map_err(|_| PlatformError::new("--resolution width is not a number"))?;
    let height: u32 = height
        .trim()
        .parse()
        .map_err(|_| PlatformError::new("--resolution height is not a number"))?;
    if width < 2 || height < 2 {
        return Err(PlatformError::new("--resolution must be at least 2x2"));
    }
    Ok((width, height))
}

fn take_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, PlatformError> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| PlatformError::new(format!("{flag} needs a value")))
}

pub fn parse_bitrate(text: &str) -> Result<u32, PlatformError> {
    let text = text.trim();
    let (number, scale) = if let Some(rest) = text.strip_suffix(['M', 'm']) {
        (rest, 1_000_000u64)
    } else if let Some(rest) = text.strip_suffix(['K', 'k']) {
        (rest, 1_000u64)
    } else {
        (text, 1u64)
    };
    let value: u64 = number
        .parse()
        .map_err(|_| PlatformError::new(format!("bitrate {text} is not a number")))?;
    u32::try_from(value.saturating_mul(scale))
        .map_err(|_| PlatformError::new("bitrate does not fit in 32 bits"))
}

fn print_listen(addr: &SocketAddr) {
    if addr.ip().is_unspecified() {
        println!(
            "listening on port {} on every interface",
            addr.port()
        );
        println!("from another Mac, use this machine's LAN or Tailscale address");
    } else if addr.ip().is_loopback() {
        println!("listening on {addr}");
        println!(
            "only this Mac can connect. For LAN or Tailscale, restart with --listen 0.0.0.0:{}",
            addr.port()
        );
    } else {
        println!("listening on {addr}");
    }
}

/// Every address a viewer should try. IPv4 comes first. A bare IP gets the default port.
pub fn resolve_targets(text: &str) -> Result<Vec<SocketAddr>, PlatformError> {
    let text = text.trim();
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Ok(vec![addr]);
    }
    if let Ok(ip) = text.parse::<std::net::IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, DEFAULT_PORT)]);
    }
    let query = if let Some((host, port)) = text.rsplit_once(':') {
        if !host.is_empty() && !host.contains(':') && port.parse::<u16>().is_ok() {
            text.to_string()
        } else if text.starts_with('[') && text.ends_with(']') {
            format!("{text}:{DEFAULT_PORT}")
        } else {
            return Err(PlatformError::new(format!(
                "could not understand address {text}. An IPv6 address needs brackets, for example [::1]:{DEFAULT_PORT}"
            )));
        }
    } else {
        format!("{text}:{DEFAULT_PORT}")
    };
    let mut addrs: Vec<SocketAddr> = query
        .to_socket_addrs()
        .map_err(PlatformError::new)?
        .collect();
    if addrs.is_empty() {
        return Err(PlatformError::new(format!("could not resolve {text}")));
    }
    addrs.sort_by_key(|addr| addr.is_ipv6());
    addrs.dedup();
    Ok(addrs)
}

pub fn resolve_socket(text: &str) -> Result<SocketAddr, PlatformError> {
    resolve_targets(text)?
        .into_iter()
        .next()
        .ok_or_else(|| PlatformError::new(format!("could not resolve {text}")))
}

#[cfg(target_os = "macos")]
pub fn modifiers_from_winit(state: winit::keyboard::ModifiersState) -> u8 {
    use deskbridge_proto::mods;
    let mut bits = 0;
    if state.shift_key() {
        bits |= mods::SHIFT;
    }
    if state.control_key() {
        bits |= mods::CTRL;
    }
    if state.alt_key() {
        bits |= mods::ALT;
    }
    if state.super_key() {
        bits |= mods::META;
    }
    bits
}

pub fn key_input(code: u16, down: bool, modifiers: u8) -> Input {
    Input::Key { code, down, modifiers }
}

pub struct Latency {
    samples: Vec<u64>,
    offset_us: i64,
    offset_ready: bool,
    bytes: Vec<(u64, u32)>,
    dropped: u64,
    last_id: u64,
    ping_at: Option<std::time::Instant>,
}

impl Latency {
    pub fn new() -> Self {
        Self {
            samples: Vec::new(),
            offset_us: 0,
            offset_ready: false,
            bytes: Vec::new(),
            dropped: 0,
            last_id: 0,
            ping_at: None,
        }
    }

    pub fn note_frame(&mut self, id: u64, capture_us: u64, payload: u32) {
        if self.last_id != 0 && id > self.last_id + 1 {
            self.dropped += id - self.last_id - 1;
        }
        self.last_id = id;
        let now = mono_us();
        self.bytes.push((now, payload));
        self.bytes.retain(|(time, _)| now.saturating_sub(*time) < 1_000_000);
        if self.offset_ready {
            let host_now = now as i64 + self.offset_us;
            let delta = host_now - capture_us as i64;
            if (0..2_000_000).contains(&delta) {
                self.samples.push(delta as u64);
                if self.samples.len() > 240 {
                    self.samples.remove(0);
                }
            }
        }
    }

    pub fn note_pong(&mut self, sent_us: u64, host_us: u64) {
        let now = mono_us();
        let rtt = now.saturating_sub(sent_us);
        let mid = sent_us + rtt / 2;
        self.offset_us = host_us as i64 - mid as i64;
        self.offset_ready = true;
    }

    pub fn due_ping(&mut self) -> bool {
        let due = self
            .ping_at
            .map(|at| at.elapsed() >= Duration::from_millis(500))
            .unwrap_or(true);
        if due {
            self.ping_at = Some(std::time::Instant::now());
        }
        due
    }

    pub fn line(&self, codec: Option<CodecId>) -> String {
        let name = codec.map(|codec| codec.name()).unwrap_or("...");
        let mbps = self.bytes.iter().map(|(_, n)| u64::from(*n)).sum::<u64>() as f64 * 8.0 / 1_000_000.0;
        let (p50, p95) = percentiles(&self.samples);
        format!(
            "{name} HW {mbps:.1} MBPS P50 {p50} MS P95 {p95} MS DROP {}",
            self.dropped
        )
    }
}

fn percentiles(samples: &[u64]) -> (u64, u64) {
    if samples.is_empty() {
        return (0, 0);
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let p50 = sorted[sorted.len() / 2];
    let p95 = sorted[sorted.len().saturating_mul(95) / 100];
    (p50 / 1000, p95 / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_suffixes() {
        assert_eq!(parse_bitrate("20M").unwrap(), 20_000_000);
        assert_eq!(parse_bitrate("500k").unwrap(), 500_000);
        assert_eq!(parse_bitrate("1000").unwrap(), 1000);
    }

    #[test]
    fn localhost_gets_the_default_port() {
        let addr = resolve_socket("127.0.0.1").unwrap();
        assert_eq!(addr.port(), DEFAULT_PORT);
    }

    #[test]
    fn ipv6_literal_gets_the_default_port() {
        let addr = resolve_socket("::1").unwrap();
        assert!(addr.is_ipv6());
        assert_eq!(addr.port(), DEFAULT_PORT);
    }

    #[test]
    fn an_explicit_port_is_kept() {
        let addr = resolve_socket("192.0.2.10:9").unwrap();
        assert_eq!(addr.port(), 9);
    }

    #[test]
    fn daemon_flag_is_accepted() {
        let args = parse_host(&["--daemon".into(), "--listen".into(), "127.0.0.1:9".into()]).unwrap();
        assert!(args.daemon);
        assert!(!args.inject);
        assert_eq!(args.listen.port(), 9);
        assert!(args.keys.is_empty());
    }

    #[test]
    fn host_key_flag_is_parsed() {
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBERERERERERERERERERERERERERERERERERERERERER laptop";
        let args = parse_host(&["--key".into(), line.into(), "--key".into(), line.into()]).unwrap();
        assert_eq!(args.keys.len(), 2);
        assert_eq!(args.keys[0].comment, "laptop");
        match parse_host(&["--key".into(), "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQC comment".into()]) {
            Err(err) => assert!(err.to_string().contains("ssh-ed25519"), "{err}"),
            Ok(_) => panic!("ssh-rsa was accepted"),
        }
    }

    #[test]
    fn resolution_presets_and_explicit_size() {
        assert_eq!(parse_resolution("1080p").unwrap(), (1920, 1080));
        assert_eq!(parse_resolution("4K").unwrap(), (3840, 2160));
        assert_eq!(parse_resolution("1280x720").unwrap(), (1280, 720));
        assert_eq!(parse_resolution("800X600").unwrap(), (800, 600));
        assert!(parse_resolution("nope").is_err());
        assert!(parse_resolution("1x720").is_err());
    }
}
