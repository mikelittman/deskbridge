//! Inbound UDP for the host. Windows Firewall drops it unless a rule allows it,
//! and the viewer then times out.

use std::process::Command;

/// Allow this program to receive UDP on `port`. A missing rule is reported.
/// Adding the rule needs an elevated process.
pub fn allow_incoming(port: u16) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let program = exe.display().to_string();
    if rule_covers(&program, port) {
        return;
    }
    let quoted = format!("\"{program}\"");
    let port_text = port.to_string();
    let _ = netsh(&["advfirewall", "firewall", "delete", "rule", "name=Deskbridge"]);
    let added = netsh(&[
        "advfirewall",
        "firewall",
        "add",
        "rule",
        "name=Deskbridge",
        "dir=in",
        "action=allow",
        "protocol=UDP",
        &format!("localport={port_text}"),
        &format!("program={quoted}"),
        "enable=yes",
        "profile=any",
    ]);
    if added {
        eprintln!("Windows Firewall allows incoming UDP {port} for deskbridge");
        return;
    }
    eprintln!(
        "Windows Firewall is blocking incoming UDP {port}. The viewer will time out until this program is allowed. Start the host once as Administrator, or allow {program} in Windows Defender Firewall."
    );
}

fn rule_covers(program: &str, port: u16) -> bool {
    let Ok(output) = Command::new("netsh")
        .args(["advfirewall", "firewall", "show", "rule", "name=Deskbridge", "verbose"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let lower = text.to_ascii_lowercase();
    lower.contains(&program.to_ascii_lowercase()) && lower.contains(&port.to_string())
}

fn netsh(args: &[&str]) -> bool {
    Command::new("netsh")
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}
