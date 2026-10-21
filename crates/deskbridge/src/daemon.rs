//! Background host. The parent process returns after printing the pairing code.
//! The child leaves the terminal session, writes a pid file, and keeps running.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::PlatformError;

const READY_WAIT: Duration = Duration::from_secs(15);

pub struct Paths {
    pub dir: PathBuf,
    pub pid: PathBuf,
    pub state: PathBuf,
    pub stop: PathBuf,
    pub log: PathBuf,
}

impl Paths {
    pub fn in_dir(dir: PathBuf) -> Self {
        Self {
            pid: dir.join("host.pid"),
            state: dir.join("host.state"),
            stop: dir.join("host.stop"),
            log: dir.join("host.log"),
            dir,
        }
    }
}

pub struct HostState {
    pub pid: u32,
    pub code: String,
    pub fingerprint: String,
    pub listen: String,
}

pub struct SessionFiles {
    paths: Paths,
}

impl Drop for SessionFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.paths.pid);
        let _ = fs::remove_file(&self.paths.state);
        let _ = fs::remove_file(&self.paths.stop);
    }
}

impl SessionFiles {
    pub fn publish(&self, code: &str, fingerprint: &str, listen: &str) -> Result<(), PlatformError> {
        write_state(
            &self.paths,
            &HostState {
                pid: std::process::id(),
                code: code.to_string(),
                fingerprint: fingerprint.to_string(),
                listen: listen.to_string(),
            },
        )
    }
}

pub fn is_child() -> bool {
    std::env::var_os("DESKBRIDGE_DAEMON").is_some()
}

pub fn stop_requested() -> bool {
    paths()
        .map(|paths| paths.stop.is_file())
        .unwrap_or(false)
}

pub fn paths() -> Result<Paths, PlatformError> {
    Ok(Paths::in_dir(default_state_dir()?))
}

/// Called in the detached process before it listens.
pub fn prepare_child() -> Result<SessionFiles, PlatformError> {
    // A launchd GUI job must stay in that session. setsid would leave it.
    if std::env::var_os("DESKBRIDGE_LAUNCHD").is_none() {
        become_session_leader();
    }
    let paths = paths()?;
    fs::create_dir_all(&paths.dir).map_err(PlatformError::new)?;
    let _ = std::env::set_current_dir(&paths.dir);
    let _ = fs::remove_file(&paths.stop);
    fs::write(&paths.pid, format!("{}\n", std::process::id())).map_err(PlatformError::new)?;
    Ok(SessionFiles { paths })
}

/// True when this process is not in the desktop session, for example an SSH shell.
///
/// Screen Recording follows the desktop session. Starting the host from SSH makes
/// macOS ask for sshd instead of deskbridge.
pub fn needs_gui_launch() -> bool {
    #[cfg(target_os = "macos")]
    {
        manager_name().as_deref() != Some("Aqua")
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Start a detached host and wait until it publishes a pairing code.
pub fn spawn() -> Result<(), PlatformError> {
    let paths = reserve_slot()?;
    #[cfg(target_os = "macos")]
    if needs_gui_launch() {
        return spawn_via_launchd(&paths);
    }
    spawn_detached(&paths)
}

fn reserve_slot() -> Result<Paths, PlatformError> {
    let paths = paths()?;
    if let Some(pid) = live_pid(&paths) {
        return Err(PlatformError::new(format!(
            "host is already running (pid {pid}). Use `deskbridge stop`"
        )));
    }
    let _ = fs::remove_file(&paths.pid);
    let _ = fs::remove_file(&paths.state);
    let _ = fs::remove_file(&paths.stop);
    fs::create_dir_all(&paths.dir).map_err(PlatformError::new)?;
    Ok(paths)
}

fn spawn_detached(paths: &Paths) -> Result<(), PlatformError> {
    let log = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&paths.log)
        .map_err(PlatformError::new)?;
    let log_err = log.try_clone().map_err(PlatformError::new)?;
    let exe = std::env::current_exe().map_err(PlatformError::new)?;
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args().skip(1))
        .env("DESKBRIDGE_DAEMON", "1")
        .current_dir(&paths.dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    configure_detached(&mut cmd);
    let mut child = cmd.spawn().map_err(PlatformError::new)?;
    wait_until_ready(paths, |paths| {
        match child.try_wait() {
            Ok(None) => Ok(true),
            Ok(Some(status)) => {
                let _ = fs::remove_file(&paths.pid);
                Err(exited_message(paths, &format!("background host exited ({status})")))
            }
            Err(err) => Err(PlatformError::new(err)),
        }
    })
}

fn announce(paths: &Paths, state: &HostState) {
    println!("host running in the background");
    println!("pid {}", state.pid);
    println!("pairing code: {}", state.code);
    println!("fingerprint:  {}", state.fingerprint);
    println!("listening on {}", state.listen);
    println!("log: {}", paths.log.display());
    println!("`deskbridge status` prints this again. `deskbridge stop` ends the host.");
}

fn exited_message(paths: &Paths, prefix: &str) -> PlatformError {
    let log_text = fs::read_to_string(&paths.log).unwrap_or_default();
    let detail = log_text.trim();
    if detail.is_empty() {
        PlatformError::new(prefix)
    } else {
        PlatformError::new(format!("{prefix}\n{detail}"))
    }
}

fn wait_until_ready(
    paths: &Paths,
    mut still_running: impl FnMut(&Paths) -> Result<bool, PlatformError>,
) -> Result<(), PlatformError> {
    let started = Instant::now();
    loop {
        if let Some(state) = read_state(paths)? {
            announce(paths, &state);
            return Ok(());
        }
        if !still_running(paths)? {
            let log = fs::read_to_string(&paths.log).unwrap_or_default();
            let grace = if log.trim().is_empty() {
                Duration::from_secs(3)
            } else {
                Duration::from_millis(800)
            };
            if started.elapsed() > grace {
                return Err(exited_message(paths, "background host exited"));
            }
        }
        if started.elapsed() > READY_WAIT {
            return Err(exited_message(
                paths,
                &format!(
                    "background host did not become ready. See {}",
                    paths.log.display()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "com.deskbridge.host";

#[cfg(target_os = "macos")]
fn spawn_via_launchd(paths: &Paths) -> Result<(), PlatformError> {
    let domain = gui_domain();
    if !launchctl_ok(&["print", &domain]) {
        return Err(PlatformError::new(
            "the Mac is not logged in at the desktop. Log in on the screen, then start the host again. Screen Recording belongs to that desktop session.",
        ));
    }
    println!("starting the host in the desktop session so macOS asks for deskbridge, not the SSH session");
    let exe = std::env::current_exe().map_err(PlatformError::new)?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let plist_path = paths.dir.join("host.plist");
    let mut env = vec![
        ("DESKBRIDGE_DAEMON".to_string(), "1".to_string()),
        ("DESKBRIDGE_LAUNCHD".to_string(), "1".to_string()),
    ];
    // The desktop job must use this same directory. Its HOME can differ from the SSH shell.
    env.push(("DESKBRIDGE_STATE_DIR".to_string(), paths.dir.display().to_string()));
    let plist = launchd_plist(&exe, &args, &paths.log, &paths.dir, &env);
    fs::write(&plist_path, plist).map_err(PlatformError::new)?;
    let _ = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&paths.log);
    clear_quarantine(&exe);
    let service = format!("{domain}/{LAUNCHD_LABEL}");
    let _ = launchctl(&["bootout", &service]);
    launchctl(&[
        "bootstrap",
        &domain,
        &plist_path.display().to_string(),
    ])?;
    // bootstrap only loads the job. kickstart is what runs it.
    let launched = match kickstart_pid(&service) {
        Ok(pid) => pid,
        Err(err) => {
            if !job_alive(paths, None, &service) {
                let status = service_status(&service);
                let _ = launchctl(&["bootout", &service]);
                return Err(PlatformError::new(format!("{err}\n{status}")));
            }
            None
        }
    };
    if let Some(pid) = launched {
        // status and stop can see the process before it writes its own pid file.
        let _ = fs::write(&paths.pid, format!("{pid}\n"));
    }
    let ready = wait_until_ready(paths, {
        let service = service.clone();
        move |paths| Ok(job_alive(paths, launched, &service))
    });
    if let Err(err) = ready {
        if job_alive(paths, launched, &service) {
            let pid = launched
                .or_else(|| live_pid(paths))
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            println!("host is still starting in the desktop session (pid {pid})");
            println!("If macOS is showing a prompt on the screen, approve deskbridge.");
            println!("`deskbridge status` prints the pairing code when the host is listening.");
            println!("log: {}", paths.log.display());
            return Ok(());
        }
        let status = service_status(&service);
        let _ = launchctl(&["bootout", &service]);
        return Err(PlatformError::new(format!("{err}\n{status}")));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn job_alive(paths: &Paths, launched: Option<u32>, service: &str) -> bool {
    live_pid(paths).is_some()
        || launched.is_some_and(process_alive)
        || service_pid(service).is_some_and(process_alive)
}

#[cfg(target_os = "macos")]
fn kickstart_pid(service: &str) -> Result<Option<u32>, PlatformError> {
    let output = Command::new("/bin/launchctl")
        .args(["kickstart", "-p", service])
        .output()
        .map_err(PlatformError::new)?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let extra = String::from_utf8_lossy(&output.stdout);
        let detail = detail.trim();
        let extra = extra.trim();
        let message = if detail.is_empty() { extra } else if extra.is_empty() { detail } else { detail };
        return Err(PlatformError::new(format!(
            "launchctl kickstart -p {service} failed: {message}"
        )));
    }
    Ok(first_pid(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(target_os = "macos")]
fn clear_quarantine(exe: &std::path::Path) {
    // A copy in Downloads is quarantined. launchd then leaves the process paused
    // until the screen prompt is answered, and the SSH command used to stop it.
    let _ = Command::new("/usr/bin/xattr")
        .args(["-d", "com.apple.quarantine"])
        .arg(exe)
        .output();
}

fn first_pid(text: &str) -> Option<u32> {
    text.split_whitespace().find_map(|word| word.parse().ok())
}

fn top_level_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key} = ");
    for line in text.lines() {
        if !(line.starts_with('\t') && !line.starts_with("\t\t")) {
            continue;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            return Some(rest.trim());
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn service_pid(service: &str) -> Option<u32> {
    top_level_field(&launchctl_text(&["print", service]), "pid").and_then(|value| value.parse().ok())
}

#[cfg(target_os = "macos")]
fn service_status(service: &str) -> String {
    let text = launchctl_text(&["print", service]);
    let mut kept = Vec::new();
    for key in ["path", "state", "program", "pid", "runs", "last exit code"] {
        if let Some(value) = top_level_field(&text, key) {
            kept.push(format!("{key} = {value}"));
        }
    }
    if kept.is_empty() {
        text.trim().to_string()
    } else {
        kept.join("\n")
    }
}

#[cfg(target_os = "macos")]
fn launchctl_text(args: &[&str]) -> String {
    let Ok(output) = Command::new("/bin/launchctl").args(args).output() else {
        return String::new();
    };
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let err = String::from_utf8_lossy(&output.stderr);
    if !err.trim().is_empty() {
        text.push('\n');
        text.push_str(err.trim());
    }
    text
}

#[cfg(target_os = "macos")]
fn bootout_gui_job() {
    let domain = gui_domain();
    let _ = launchctl(&["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]);
}

#[cfg(target_os = "macos")]
fn gui_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

#[cfg(target_os = "macos")]
fn manager_name() -> Option<String> {
    let output = Command::new("/bin/launchctl").arg("managername").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8(output.stdout).ok()?;
    let name = name.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> Result<(), PlatformError> {
    let output = Command::new("/bin/launchctl")
        .args(args)
        .output()
        .map_err(PlatformError::new)?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    let extra = String::from_utf8_lossy(&output.stdout);
    let extra = extra.trim();
    let message = match (detail.is_empty(), extra.is_empty()) {
        (false, false) => format!("launchctl {} failed: {detail} {extra}", args.join(" ")),
        (false, true) => format!("launchctl {} failed: {detail}", args.join(" ")),
        (true, false) => format!("launchctl {} failed: {extra}", args.join(" ")),
        (true, true) => format!("launchctl {} failed", args.join(" ")),
    };
    Err(PlatformError::new(message))
}

#[cfg(target_os = "macos")]
fn launchctl_ok(args: &[&str]) -> bool {
    launchctl(args).is_ok()
}

fn launchd_plist(
    exe: &std::path::Path,
    args: &[String],
    log: &std::path::Path,
    workdir: &std::path::Path,
    env: &[(String, String)],
) -> String {
    let mut arguments = String::new();
    arguments.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(&exe.display().to_string())));
    for arg in args {
        arguments.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(arg)));
    }
    let mut variables = String::new();
    for (key, value) in env {
        variables.push_str(&format!(
            "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
            xml_escape(key),
            xml_escape(value)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.deskbridge.host</string>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>ProgramArguments</key>
	<array>
{arguments}	</array>
	<key>EnvironmentVariables</key>
	<dict>
{variables}	</dict>
	<key>WorkingDirectory</key>
	<string>{workdir}</string>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        arguments = arguments,
        variables = variables,
        workdir = xml_escape(&workdir.display().to_string()),
        log = xml_escape(&log.display().to_string()),
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn status() -> Result<(), PlatformError> {
    let paths = paths()?;
    let Some(pid) = live_pid(&paths) else {
        let _ = fs::remove_file(&paths.pid);
        return Err(PlatformError::new("host is not running"));
    };
    println!("host running");
    println!("pid {pid}");
    if let Some(state) = read_state(&paths)? {
        println!("pairing code: {}", state.code);
        println!("fingerprint:  {}", state.fingerprint);
        println!("listening on {}", state.listen);
    } else {
        println!("the host is still starting");
    }
    println!("log: {}", paths.log.display());
    Ok(())
}

pub fn stop() -> Result<(), PlatformError> {
    let paths = paths()?;
    let pid = live_pid(&paths);
    if pid.is_some() {
        fs::write(&paths.stop, b"stop\n").map_err(PlatformError::new)?;
    }
    #[cfg(target_os = "macos")]
    {
        bootout_gui_job();
        let _ = fs::remove_file(paths.dir.join("host.plist"));
    }
    let Some(pid) = pid else {
        let _ = fs::remove_file(&paths.pid);
        let _ = fs::remove_file(&paths.state);
        return Err(PlatformError::new("host is not running"));
    };
    let _ = signal_term(pid);
    let started = Instant::now();
    while process_alive(pid) && started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    if process_alive(pid) {
        let _ = signal_kill(pid);
        let started = Instant::now();
        while process_alive(pid) && started.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if process_alive(pid) {
        return Err(PlatformError::new(format!("host pid {pid} did not exit")));
    }
    let _ = fs::remove_file(&paths.pid);
    let _ = fs::remove_file(&paths.state);
    let _ = fs::remove_file(&paths.stop);
    println!("host stopped");
    Ok(())
}

pub fn write_state(paths: &Paths, state: &HostState) -> Result<(), PlatformError> {
    if let Some(parent) = paths.state.parent() {
        fs::create_dir_all(parent).map_err(PlatformError::new)?;
    }
    let body = format!(
        "pid={}\ncode={}\nfingerprint={}\nlisten={}\n",
        state.pid, state.code, state.fingerprint, state.listen
    );
    let tmp = paths.dir.join("host.state.tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .map_err(PlatformError::new)?;
        file.write_all(body.as_bytes()).map_err(PlatformError::new)?;
        file.sync_all().map_err(PlatformError::new)?;
    }
    fs::rename(&tmp, &paths.state).map_err(PlatformError::new)?;
    Ok(())
}

pub fn read_state(paths: &Paths) -> Result<Option<HostState>, PlatformError> {
    if !paths.state.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&paths.state).map_err(PlatformError::new)?;
    Ok(parse_state(&text))
}

fn parse_state(text: &str) -> Option<HostState> {
    let mut pid = None;
    let mut code = None;
    let mut fingerprint = None;
    let mut listen = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "pid" => pid = value.trim().parse().ok(),
            "code" => code = Some(value.trim().to_string()),
            "fingerprint" => fingerprint = Some(value.trim().to_string()),
            "listen" => listen = Some(value.trim().to_string()),
            _ => {}
        }
    }
    Some(HostState {
        pid: pid?,
        code: code.filter(|code| !code.is_empty())?,
        fingerprint: fingerprint.filter(|value| !value.is_empty())?,
        listen: listen.filter(|value| !value.is_empty())?,
    })
}

fn default_state_dir() -> Result<PathBuf, PlatformError> {
    if let Some(dir) = std::env::var_os("DESKBRIDGE_STATE_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(home_dir()?.join("Library/Application Support/deskbridge"));
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
            if !xdg.is_empty() {
                return Ok(PathBuf::from(xdg).join("deskbridge"));
            }
        }
        return Ok(home_dir()?.join(".local/state/deskbridge"));
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            if !local.is_empty() {
                return Ok(PathBuf::from(local).join("deskbridge"));
            }
        }
        return Ok(home_dir()?
            .join("AppData")
            .join("Local")
            .join("deskbridge"));
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Ok(home_dir()?.join(".deskbridge"))
    }
}

fn home_dir() -> Result<PathBuf, PlatformError> {
    #[cfg(windows)]
    let key = "USERPROFILE";
    #[cfg(not(windows))]
    let key = "HOME";
    match std::env::var_os(key) {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Err(PlatformError::new(format!("{key} is not set"))),
    }
}

fn live_pid(paths: &Paths) -> Option<u32> {
    let text = fs::read_to_string(&paths.pid).ok()?;
    let pid = text.trim().parse::<u32>().ok()?;
    if process_alive(pid) {
        Some(pid)
    } else {
        None
    }
}

fn process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe {
            if libc::kill(pid, 0) == 0 {
                return true;
            }
            std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }
    #[cfg(windows)]
    {
        unsafe {
            const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
            const STILL_ACTIVE: u32 = 259;
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if handle.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

fn signal_term(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        unsafe { libc::kill(pid, libc::SIGTERM) == 0 }
    }
    #[cfg(windows)]
    {
        // The stop file is the graceful path. TerminateProcess is the fallback.
        let _ = pid;
        true
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

fn signal_kill(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        unsafe { libc::kill(pid, libc::SIGKILL) == 0 }
    }
    #[cfg(windows)]
    {
        unsafe {
            const PROCESS_TERMINATE: u32 = 0x0001;
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                return false;
            }
            let ok = TerminateProcess(handle, 1);
            CloseHandle(handle);
            ok != 0
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

#[cfg(unix)]
fn become_session_leader() {
    unsafe {
        if libc::setsid() == -1 {
            eprintln!(
                "deskbridge: could not leave the terminal session ({})",
                std::io::Error::last_os_error()
            );
        }
    }
}

#[cfg(not(unix))]
fn become_session_leader() {}

fn configure_detached(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

#[cfg(windows)]
extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    fn GetExitCodeProcess(handle: *mut std::ffi::c_void, code: *mut u32) -> i32;
    fn TerminateProcess(handle: *mut std::ffi::c_void, code: u32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrip_and_a_partial_file_is_ignored() {
        let dir = std::env::temp_dir().join(format!("deskbridge-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let paths = Paths::in_dir(dir.clone());
        assert!(read_state(&paths).unwrap().is_none());
        fs::write(&paths.state, "pid=4\n").unwrap();
        assert!(read_state(&paths).unwrap().is_none());
        write_state(
            &paths,
            &HostState {
                pid: 42,
                code: "ABCD2345".into(),
                fingerprint: "ab".into(),
                listen: "127.0.0.1:9787".into(),
            },
        )
        .unwrap();
        let state = read_state(&paths).unwrap().unwrap();
        assert_eq!(state.pid, 42);
        assert_eq!(state.code, "ABCD2345");
        assert_eq!(state.listen, "127.0.0.1:9787");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_process_is_alive_and_pid_zero_is_not() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX));
    }

    #[test]
    fn the_default_directory_matches_the_platform() {
        if std::env::var_os("DESKBRIDGE_STATE_DIR").is_some() {
            return;
        }
        let dir = default_state_dir().unwrap();
        let text = dir.to_string_lossy();
        #[cfg(target_os = "macos")]
        assert!(
            text.contains("Application Support/deskbridge"),
            "{text}"
        );
        #[cfg(target_os = "linux")]
        assert!(text.contains("deskbridge"), "{text}");
        #[cfg(target_os = "windows")]
        assert!(text.contains("deskbridge"), "{text}");
        assert!(dir.is_absolute());
    }

    #[test]
    fn the_launchd_plist_names_deskbridge_and_escapes_arguments() {
        let plist = launchd_plist(
            std::path::Path::new("/usr/local/bin/deskbridge"),
            &["host".into(), "--key".into(), "a & b <c>".into()],
            std::path::Path::new("/tmp/deskbridge/host.log"),
            std::path::Path::new("/tmp/deskbridge"),
            &[
                ("DESKBRIDGE_DAEMON".into(), "1".into()),
                ("DESKBRIDGE_LAUNCHD".into(), "1".into()),
            ],
        );
        assert!(plist.contains("<string>/usr/local/bin/deskbridge</string>"));
        assert!(plist.contains("<string>a &amp; b &lt;c&gt;</string>"));
        assert!(plist.contains("<string>Aqua</string>"));
        assert!(plist.contains("<key>DESKBRIDGE_LAUNCHD</key>"));
        assert!(plist.contains("<string>com.deskbridge.host</string>"));
        assert_eq!(first_pid("32353\n"), Some(32353));
        assert_eq!(first_pid("not a pid"), None);
        let printed = "\tstate = running\n\tpid = 42\n\t\tstate = active\n";
        assert_eq!(top_level_field(printed, "state"), Some("running"));
        assert_eq!(top_level_field(printed, "pid"), Some("42"));
        #[cfg(target_os = "macos")]
        {
            let path = std::env::temp_dir().join(format!("deskbridge-plist-{}.plist", std::process::id()));
            std::fs::write(&path, &plist).unwrap();
            let status = std::process::Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(&path)
                .status()
                .unwrap();
            let _ = std::fs::remove_file(&path);
            assert!(status.success(), "plist did not parse");
        }
    }

    #[test]
    fn a_dead_pid_file_is_not_running() {
        let dir = std::env::temp_dir().join(format!("deskbridge-pid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let paths = Paths::in_dir(dir.clone());
        fs::write(&paths.pid, "0\n").unwrap();
        assert!(live_pid(&paths).is_none());
        fs::write(&paths.pid, format!("{}\n", std::process::id())).unwrap();
        assert_eq!(live_pid(&paths), Some(std::process::id()));
        let _ = fs::remove_dir_all(&dir);
    }
}
