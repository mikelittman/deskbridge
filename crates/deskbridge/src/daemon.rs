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
    become_session_leader();
    let paths = paths()?;
    fs::create_dir_all(&paths.dir).map_err(PlatformError::new)?;
    let _ = std::env::set_current_dir(&paths.dir);
    let _ = fs::remove_file(&paths.stop);
    fs::write(&paths.pid, format!("{}\n", std::process::id())).map_err(PlatformError::new)?;
    Ok(SessionFiles { paths })
}

/// Start a detached host and wait until it publishes a pairing code.
pub fn spawn() -> Result<(), PlatformError> {
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
    let started = Instant::now();
    loop {
        if let Some(state) = read_state(&paths)? {
            println!("host running in the background");
            println!("pid {}", state.pid);
            println!("pairing code: {}", state.code);
            println!("fingerprint:  {}", state.fingerprint);
            println!("listening on {}", state.listen);
            println!("log: {}", paths.log.display());
            println!("`deskbridge status` prints this again. `deskbridge stop` ends the host.");
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(PlatformError::new)? {
            let _ = fs::remove_file(&paths.pid);
            let log_text = fs::read_to_string(&paths.log).unwrap_or_default();
            let detail = log_text.trim();
            let message = if detail.is_empty() {
                format!("background host exited ({status})")
            } else {
                format!("background host exited ({status})\n{detail}")
            };
            return Err(PlatformError::new(message));
        }
        if started.elapsed() > READY_WAIT {
            let _ = signal_term(child.id());
            let _ = signal_kill(child.id());
            return Err(PlatformError::new(format!(
                "background host did not become ready. See {}",
                paths.log.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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
    let Some(pid) = live_pid(&paths) else {
        let _ = fs::remove_file(&paths.pid);
        let _ = fs::remove_file(&paths.state);
        return Err(PlatformError::new("host is not running"));
    };
    fs::write(&paths.stop, b"stop\n").map_err(PlatformError::new)?;
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
