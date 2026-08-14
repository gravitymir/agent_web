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

/// The host-side guest sandbox instance (`run-guest.ps1`): where guests actually
/// connect and their turns run (the executor VM is only their tool backend over
/// SSH). Drain-Stop drains THIS so it waits for real guest turns before powering
/// the VM off.
pub const GUEST_SANDBOX_PORT: u16 = 8790;

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

/// Run a guest command over SSH, capturing exit code + stdout + stderr. `stdin`
/// is fed to the remote command when `Some` (e.g. `cat > file`). Used by the
/// `mcp-guest` sandbox tools, which need the actual output, not just success.
pub fn ssh_capture(remote_cmd: &str, stdin: Option<&[u8]>) -> (i32, String, String) {
    use std::io::Write;
    let mut cmd = Command::new("ssh");
    cmd.args(ssh_base_args());
    cmd.arg(SSH_PORT)
        .arg(format!("{SSH_USER}@127.0.0.1"))
        .arg(remote_cmd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (-1, String::new(), format!("ssh spawn failed: {e}")),
    };
    if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
        let _ = si.write_all(data);
        // `si` drops here, closing stdin so the remote command sees EOF.
    }
    match child.wait_with_output() {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(e) => (-1, String::new(), format!("ssh wait failed: {e}")),
    }
}

/// Like [`ssh_capture`] but returns stdout as raw bytes — for binary payloads
/// such as a `tar` stream (a lossy-UTF-8 `String` would corrupt them). stderr
/// stays lossy UTF-8 (diagnostic/log only).
pub fn ssh_capture_raw(remote_cmd: &str, stdin: Option<&[u8]>) -> (i32, Vec<u8>, String) {
    use std::io::Write;
    let mut cmd = Command::new("ssh");
    cmd.args(ssh_base_args());
    cmd.arg(SSH_PORT)
        .arg(format!("{SSH_USER}@127.0.0.1"))
        .arg(remote_cmd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (-1, Vec::new(), format!("ssh spawn failed: {e}")),
    };
    if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
        let _ = si.write_all(data);
        // `si` drops here, closing stdin so the remote command sees EOF.
    }
    match child.wait_with_output() {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            o.stdout,
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(e) => (-1, Vec::new(), format!("ssh wait failed: {e}")),
    }
}

/// Run a guest command feeding `input` to its stdin (e.g. `cat > file`). Avoids
/// shell-escaping the payload. Returns success.
fn ssh_run_stdin(remote_cmd: &str, input: &[u8]) -> bool {
    use std::io::Write;
    let mut child = match Command::new("ssh")
        .args(ssh_base_args())
        .arg(SSH_PORT)
        .arg(format!("{SSH_USER}@127.0.0.1"))
        .arg(remote_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(input);
        // `si` drops here, closing the pipe so the remote `cat` sees EOF.
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

/// The guest's `agent_web` config dir (matches its systemd unit: exe under
/// `~/agent_web/target/release`, so the store is `…/chats/guest_tokens.json`).
const GUEST_CONFIG_DIR: &str = "/home/insider/agent_web/target/release/chats";

/// Push the owner's token-store JSON into the running executor's
/// `guest_tokens.json` so its access gate validates codes minted on the host.
/// `verify_code` reads the store live, so this takes effect without a restart.
/// Best-effort: false if the VM is down or SSH fails.
pub fn push_guest_tokens(json: &str) -> bool {
    if !running() {
        return false;
    }
    let remote = format!(
        "mkdir -p {d} && cat > {d}/guest_tokens.json && chmod 600 {d}/guest_tokens.json",
        d = GUEST_CONFIG_DIR
    );
    ssh_run_stdin(&remote, json.as_bytes())
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
