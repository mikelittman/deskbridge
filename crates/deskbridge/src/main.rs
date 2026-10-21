#[cfg(all(
    any(target_os = "linux", target_os = "windows"),
    not(target_pointer_width = "64")
))]
compile_error!("Linux and Windows builds are 64-bit only");

#[cfg(target_os = "macos")]
mod sessions;
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
use deskbridge_platform_macos::{ensure_host_permissions, require_encode_codec, PermissionSubject};
#[cfg(target_os = "macos")]
use deskbridge_session::{listen, Identity};

const DEFAULT_PORT: u16 = 9787;

fn version_line() -> String {
    let version = option_env!("DESKBRIDGE_VERSION").unwrap_or("dev");
    let commit = option_env!("DESKBRIDGE_COMMIT").unwrap_or("unknown");
    format!("{version} ({commit})")
}

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
        Some("version") | Some("--version") | Some("-V") => {
            println!("{}", version_line());
            Ok(())
        }
        Some("-h") | Some("--help") | None => {
            print_help();
            Ok(())
        }
        Some(other) => Err(PlatformError::new(format!(
            "unknown command {other}. Use `deskbridge host`, `deskbridge view`, `deskbridge status`, `deskbridge stop`, or `deskbridge version`."
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
deskbridge host [--display N] [--listen addr:port] [--bitrate 20M] [--resolution 1080p] [--inject] [--daemon] [--key \"ssh-ed25519 AAAA...\"] [--github USER]
deskbridge view <addr[:port]> [--code CODE] [--key \"ssh-ed25519 AAAA...\"] [--fingerprint HEX] [--resolution 1080p]
deskbridge status
deskbridge stop
deskbridge version

Direct connection only. Default port is {DEFAULT_PORT}. The host listens on every interface.
The host prints a pairing code. Ctrl+Alt releases the viewer's pointer grab.
--daemon keeps the host running after the terminal closes.
From SSH, --daemon starts the host in the desktop session so Screen Recording applies to deskbridge. Log in on the Mac's screen first.
`deskbridge status` prints the pairing code. `deskbridge stop` ends that host.
--key on the host authorizes an ssh-ed25519 public key. Repeat it for more than one.
--github USER authorizes the ssh-ed25519 keys published for that GitHub account.
The host also reads authorized_keys in its state directory.
The viewer signs with the SSH agent, including 1Password. --key there picks which agent key.
--code still admits a guest. --resolution is 720p, 1080p, 1440p, 2160p, 4k, or WxH.
On the viewer, --resolution selects that viewer's picture size. On the host, it is the largest picture the host will send.
With several viewers, the picture fits the smallest request.
The host keeps listening after a viewer disconnects. Several viewers can connect at once.
The first picture is small so it shows up quickly, then the full picture follows.
The frame rate drops when a large part of the screen is changing, so input stays responsive.
Quitting the host, or deskbridge stop, closes every viewer."
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
    github: Vec<String>,
}

fn host(args: Vec<String>) -> Result<(), PlatformError> {
    let args = parse_host(&args)?;
    #[cfg(target_os = "macos")]
    {
        if args.daemon && !daemon::is_child() {
            // An SSH parent must not prompt. macOS would attach the grant to sshd.
            if !daemon::needs_gui_launch() {
                ensure_host_permissions(args.inject, PermissionSubject::Terminal)
                    .map_err(PlatformError::new)?;
            }
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
    let subject = if std::env::var_os("DESKBRIDGE_LAUNCHD").is_some() {
        PermissionSubject::ThisBinary
    } else {
        PermissionSubject::Terminal
    };
    ensure_host_permissions(args.inject, subject).map_err(PlatformError::new)?;
    let codecs = require_encode_codec().map_err(PlatformError::new)?;
    let identity = Identity::generate().map_err(PlatformError::new)?;
    let authorized = authorized_keys(args.keys, &args.github)?;
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
    let codec = codecs[0];
    runtime.block_on(sessions::serve(
        endpoint,
        identity,
        codec,
        authorized,
        args.display,
        args.bitrate,
        args.inject,
        args.resolution,
    ));
    drop(session_files);
    Ok(())
}

fn parse_host(args: &[String]) -> Result<HostArgs, PlatformError> {
    let mut display = 0usize;
    let mut listen = format!("[::]:{DEFAULT_PORT}");
    let mut bitrate = 20_000_000u32;
    let mut resolution = None;
    let mut inject = false;
    let mut daemon = false;
    let mut keys = Vec::new();
    let mut github = Vec::new();
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
            "--github" => {
                let user = take_value(args, &mut index, "--github")?;
                github.push(github_username(&user)?.to_string());
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
        github,
    })
}

fn github_username(text: &str) -> Result<&str, PlatformError> {
    let name = text.trim();
    let bytes = name.as_bytes();
    let ok = (1..=39).contains(&bytes.len())
        && bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        && bytes.first().is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.last().is_some_and(|byte| byte.is_ascii_alphanumeric())
        && !name.contains("--");
    if !ok {
        return Err(PlatformError::new(format!(
            "{text} is not a GitHub username"
        )));
    }
    Ok(name)
}

#[derive(Debug)]
struct GithubKeySet {
    keys: Vec<AuthorizedKey>,
    skipped: u32,
}

fn keys_from_github_text(user: &str, text: &str) -> Result<GithubKeySet, PlatformError> {
    let mut keys: Vec<AuthorizedKey> = Vec::new();
    let mut skipped = 0u32;
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if is_other_ssh_key(line) {
            skipped += 1;
            continue;
        }
        let mut key = parse_public_key_line(line).map_err(|err| {
            PlatformError::new(format!("github user {user} key {}: {err}", index + 1))
        })?;
        if key.comment.is_empty() {
            key.comment = format!("github:{user}");
        }
        if keys.iter().any(|have| have.blob == key.blob) {
            continue;
        }
        keys.push(key);
    }
    if keys.is_empty() {
        let message = if skipped == 0 {
            format!("github user {user} has no public SSH keys")
        } else {
            format!("github user {user} has no ssh-ed25519 public keys ({skipped} other keys were skipped)")
        };
        return Err(PlatformError::new(message));
    }
    Ok(GithubKeySet { keys, skipped })
}

fn is_other_ssh_key(line: &str) -> bool {
    line.split_whitespace().any(|token| {
        (token.starts_with("ssh-") || token.starts_with("ecdsa-") || token.starts_with("sk-"))
            && token != "ssh-ed25519"
    })
}

#[cfg(target_os = "macos")]
fn authorized_keys(
    from_flags: Vec<AuthorizedKey>,
    github_users: &[String],
) -> Result<Vec<AuthorizedKey>, PlatformError> {
    let mut keys = read_authorized_keys_file()?;
    for key in from_flags {
        push_unique(&mut keys, key);
    }
    let mut seen_users = Vec::new();
    for user in github_users {
        if seen_users.iter().any(|have: &String| have.eq_ignore_ascii_case(user)) {
            continue;
        }
        seen_users.push(user.clone());
        let fetched = fetch_github_keys(user)?;
        if fetched.skipped > 0 {
            eprintln!(
                "deskbridge: github user {user} has {} keys that are not ssh-ed25519. Those were skipped.",
                fetched.skipped
            );
        }
        for key in fetched.keys {
            push_unique(&mut keys, key);
        }
    }
    if keys.len() > 32 {
        return Err(PlatformError::new("at most 32 authorized SSH keys"));
    }
    Ok(keys)
}

#[cfg(target_os = "macos")]
fn push_unique(keys: &mut Vec<AuthorizedKey>, key: AuthorizedKey) {
    if keys.iter().any(|have| have.blob == key.blob) {
        return;
    }
    keys.push(key);
}

#[cfg(target_os = "macos")]
fn fetch_github_keys(user: &str) -> Result<GithubKeySet, PlatformError> {
    use std::io::Read;

    let url = format!("https://github.com/{user}.keys");
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    let response = agent.get(&url).set("User-Agent", "deskbridge").call();
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => {
            return Err(PlatformError::new(format!("github user {user} was not found")));
        }
        Err(ureq::Error::Status(status, _)) => {
            return Err(PlatformError::new(format!(
                "github keys for {user} returned HTTP {status}"
            )));
        }
        Err(ureq::Error::Transport(err)) => {
            return Err(PlatformError::new(format!(
                "could not reach github for user {user}: {err}"
            )));
        }
    };
    let mut body = Vec::new();
    response
        .into_reader()
        .take(256 * 1024)
        .read_to_end(&mut body)
        .map_err(|err| PlatformError::new(format!("could not read github keys for {user}: {err}")))?;
    let text = String::from_utf8(body)
        .map_err(|_| PlatformError::new(format!("github keys for {user} were not utf-8")))?;
    keys_from_github_text(user, &text)
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
        assert!(args.github.is_empty());
    }

    #[test]
    fn github_usernames_and_account_keys() {
        let args = parse_host(&["--github".into(), "octocat".into(), "--github".into(), "FiloSottile".into()])
            .unwrap();
        assert_eq!(args.github, vec!["octocat".to_string(), "FiloSottile".to_string()]);
        assert!(parse_host(&["--github".into(), "../etc".into()]).is_err());
        assert!(parse_host(&["--github".into(), "-bad".into()]).is_err());
        let text = "\nssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQC comment\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBERERERERERERERERERERERERERERERERERERERERER\n";
        let parsed = keys_from_github_text("octocat", text).unwrap();
        assert_eq!(parsed.skipped, 1);
        assert_eq!(parsed.keys.len(), 1);
        assert_eq!(parsed.keys[0].comment, "github:octocat");
        match keys_from_github_text("octocat", "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQC comment\n") {
            Err(err) => assert!(err.to_string().contains("no ssh-ed25519"), "{err}"),
            Ok(_) => panic!("an rsa-only account was accepted"),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn github_fetch_reads_a_real_account() {
        match fetch_github_keys("torvalds") {
            Err(err) => assert!(err.to_string().contains("no ssh-ed25519"), "{err}"),
            Ok(_) => panic!("expected torvalds to have no ssh-ed25519 key"),
        }
        match fetch_github_keys("this-user-does-not-exist-deskbridge-zz") {
            Err(err) => assert!(err.to_string().contains("not found"), "{err}"),
            Ok(_) => panic!("missing github user was accepted"),
        }
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
