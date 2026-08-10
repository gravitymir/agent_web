//! Executor VM control — thin wrappers over `VBoxManage` + SSH into the guest.
//!
//! Pure operations; the caller adds the UX (live WebSocket progress streamed to
//! the "Гостевой сервер" tab). Blocking (shells out), so the async
//! server drives these via `spawn_blocking`. Conventions must match the Phase-0
//! base image and the NAT port-forwards baked into the `clean` snapshot.

use std::process::{Command, Stdio};
use std::time::Duration;

pub const EXECUTOR: &str = "executor";
pub const CLEAN_SNAPSHOT: &str = "clean";
pub const SSH_PORT: &str = "2222";
pub const SSH_USER: &str = "insider";
/// Host port forwarded to the guest's `agent_web` (NAT rule `aw`: 8788 → 8787).
pub const GUEST_APP_PORT: u16 = 8788;

/// The `VBoxManage` binary: PATH first, else the default Windows install path
/// (so it works even when the installer didn't update PATH for this process).
fn vbox_bin() -> String {
    #[cfg(windows)]
    {
        let default = r"C:\Program Files\Oracle\VirtualBox\VBoxManage.exe";
        if std::path::Path::new(default).exists() {
            return default.to_string();
        }
    }
    "VBoxManage".to_string()
}

fn vbox(args: &[&str]) -> bool {
    Command::new(vbox_bin())
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn vbox_out(args: &[&str]) -> Option<String> {
    let o = Command::new(vbox_bin()).args(args).output().ok()?;
    if !o.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&o.stdout).into_owned())
}

pub fn exists() -> bool {
    vbox_out(&["list", "vms"]).is_some_and(|s| s.contains(&format!("\"{EXECUTOR}\"")))
}
pub fn running() -> bool {
    vbox_out(&["list", "runningvms"]).is_some_and(|s| s.contains(&format!("\"{EXECUTOR}\"")))
}
pub fn restore_clean() -> bool {
    vbox(&["snapshot", EXECUTOR, "restore", CLEAN_SNAPSHOT])
}
pub fn start_headless() -> bool {
    vbox(&["startvm", EXECUTOR, "--type", "headless"])
}
/// Hard power-off (pulls the plug).
fn poweroff() -> bool {
    vbox(&["controlvm", EXECUTOR, "poweroff"])
}
/// Graceful stop: ACPI power button, then a hard power-off if the guest hasn't
/// shut down within a few seconds. Blocking (sleeps).
pub fn stop_graceful() -> bool {
    vbox(&["controlvm", EXECUTOR, "acpipowerbutton"]);
    for _ in 0..5 {
        std::thread::sleep(Duration::from_secs(2));
        if !running() {
            return true;
        }
    }
    poweroff() // force
}
fn snapshot_list() -> Option<String> {
    vbox_out(&["snapshot", EXECUTOR, "list", "--machinereadable"])
}

/// SSH key that authenticates into the executor (`~/.ssh/agent_vm_key`).
fn ssh_key() -> String {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    #[cfg(windows)]
    {
        format!("{home}\\.ssh\\agent_vm_key")
    }
    #[cfg(not(windows))]
    {
        format!("{home}/.ssh/agent_vm_key")
    }
}

fn ssh_base_args() -> [String; 9] {
    [
        "-i".into(),
        ssh_key(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "ConnectTimeout=5".into(),
        "-p".into(),
    ]
}

/// True once the executor accepts a key-based SSH connection (booted + sshd up).
pub fn ssh_ready() -> bool {
    let mut cmd = Command::new("ssh");
    cmd.args(ssh_base_args());
    cmd.arg(SSH_PORT)
        .arg(format!("{SSH_USER}@127.0.0.1"))
        .arg("true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a command inside the guest over SSH (key auth). Returns success.
pub fn ssh_run(remote_cmd: &str) -> bool {
    let mut cmd = Command::new("ssh");
    cmd.args(ssh_base_args());
    cmd.arg(SSH_PORT)
        .arg(format!("{SSH_USER}@127.0.0.1"))
        .arg(remote_cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Snapshot of the VM's state for the status UI.
pub struct Status {
    pub exists: bool,
    pub running: bool,
    pub ssh_ready: bool,
    pub has_clean_snapshot: bool,
}

pub fn status() -> Status {
    let running = running();
    Status {
        exists: exists(),
        running,
        ssh_ready: running && ssh_ready(),
        has_clean_snapshot: snapshot_list().is_some_and(|s| s.contains(CLEAN_SNAPSHOT)),
    }
}
