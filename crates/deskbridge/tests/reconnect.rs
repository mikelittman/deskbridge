//! The host process stays up after a viewer leaves, and two viewers can be connected together.
#![cfg(target_os = "macos")]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use deskbridge_platform_macos::hardware_decode_codecs;
use deskbridge_proto::Control;
use deskbridge_session::{connect, ClientAuth, KeyChoice, PairedClient};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_accepts_another_viewer_and_keeps_two_at_once() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_deskbridge"))
        .args(["host", "--listen", "127.0.0.1:19787"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn host");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let lines = spawn_lines(stdout);
    let errors = spawn_lines(stderr);
    let mut host = HostProcess { child, lines, errors };

    let ready = host.wait_for("pairing code:", Duration::from_secs(8));
    let code = ready.split(':').nth(1).expect("code").trim().to_string();
    assert!(!code.is_empty(), "{ready}");
    host.wait_for("waiting for a viewer", Duration::from_secs(2));

    let mut first = connect_viewer(&code).await;
    wait_display(&mut first).await;
    host.wait_for("1 viewer", Duration::from_secs(2));

    let mut second = connect_viewer(&code).await;
    wait_display(&mut second).await;
    host.wait_for("2 viewers", Duration::from_secs(2));

    drop(first);
    host.wait_for("1 still connected", Duration::from_secs(5));
    assert!(host.running(), "host exited while a viewer was still connected");

    drop(second);
    host.wait_for("waiting for a viewer", Duration::from_secs(5));
    assert!(host.running(), "host exited after the last viewer left");

    let mut third = connect_viewer(&code).await;
    wait_display(&mut third).await;
    host.wait_for("1 viewer", Duration::from_secs(8));
    drop(third);

    host.stop();
}

struct HostProcess {
    child: Child,
    lines: Receiver<String>,
    errors: Receiver<String>,
}

impl HostProcess {
    fn wait_for(&self, needle: &str, timeout: Duration) -> String {
        let start = Instant::now();
        let mut seen = Vec::new();
        while start.elapsed() < timeout {
            match self.lines.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => {
                    if line.contains(needle) {
                        return line;
                    }
                    seen.push(line);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        panic!(
            "did not see {needle} within {}s.\nstdout:\n{}\nstderr:\n{}",
            timeout.as_secs(),
            seen.join("\n"),
            self.drain_errors()
        );
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn stop(&mut self) {
        let _ = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn drain_errors(&self) -> String {
        let mut lines = Vec::new();
        while let Ok(line) = self.errors.try_recv() {
            lines.push(line);
        }
        lines.join("\n")
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        if self.running() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn spawn_lines(pipe: impl std::io::Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

async fn connect_viewer(code: &str) -> PairedClient {
    connect(
        "127.0.0.1:19787".parse().unwrap(),
        &ClientAuth {
            code: Some(code.to_string()),
            keys: KeyChoice::PairingCodeOnly,
        },
        None,
        hardware_decode_codecs(),
    )
    .await
    .unwrap_or_else(|err| panic!("connect failed: {err}"))
}

async fn wait_display(client: &mut PairedClient) {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match client.from_peer.recv().await {
                Some(Control::DisplayInfo(info)) => {
                    assert!(info.width >= 2 && info.height >= 2);
                    return;
                }
                Some(_) => continue,
                None => panic!("viewer disconnected before DisplayInfo"),
            }
        }
    })
    .await;
    result.expect("timed out waiting for display info");
}
