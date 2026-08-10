//! agentctl — control panel for the Agent Web executor VM + Cloudflare tunnel.
//!
//! Arrow-key menu (no args) or direct subcommands (scriptable):
//!
//!   agentctl                 # interactive menu
//!   agentctl up              # start tunnel + executor VM
//!   agentctl down            # stop executor VM (tunnel left running)
//!   agentctl start|stop|reset|status
//!   agentctl tunnel start|stop|status|autostart
//!   agentctl vm <start|stop|reset|status|info <name>|selftest|delete <name>>
//!
//! The executor is a disposable VirtualBox VM (driven via VBoxManage): each
//! `start` restores the `clean` snapshot and boots headless. The Cloudflare
//! tunnel is a Windows service (Cloudflared); start/stop/autostart need an
//! Administrator terminal.

use std::process::Command;

use dialoguer::{theme::ColorfulTheme, Select};

const TUNNEL_SVC: &str = "Cloudflared";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("up") => {
            tunnel_start();
            executor_start();
        }
        Some("down") => executor_stop(),
        Some("start") => executor_start(),
        Some("stop") => executor_stop(),
        Some("reset") => executor_reset(),
        Some("status") => status(),
        Some("vm") => vm_cli(&args),
        Some("tunnel") => match args.get(1).map(|s| s.as_str()) {
            Some("start") => tunnel_start(),
            Some("stop") => tunnel_stop(),
            Some("status") => tunnel_status(),
            Some("autostart") => tunnel_autostart(),
            _ => eprintln!("usage: agentctl tunnel [start|stop|status|autostart]"),
        },
        Some(other) => {
            eprintln!("unknown command: {other}");
            usage();
        }
        None => interactive(),
    }
}

fn usage() {
    eprintln!(
        "usage: agentctl [up|down|start|stop|reset|status|tunnel <start|stop|status|autostart>|vm <start|stop|reset|status|info <name>|selftest|delete <name>>]"
    );
}

fn interactive() {
    let items = ["Executor", "Tunnel", "Status", "Quit"];
    loop {
        println!();
        let sel = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Agent Web — control panel")
            .items(&items)
            .default(0)
            .interact()
            .unwrap_or(items.len() - 1);
        match items[sel] {
            "Executor" => executor_menu(),
            "Tunnel" => tunnel_menu(),
            "Status" => status(),
            _ => break,
        }
    }
}

/// Executor submenu — start/stop/reset shown by context (disposable VM).
fn executor_menu() {
    loop {
        let running = vbox::running(EXECUTOR);
        let mut items: Vec<&str> = Vec::new();
        if running {
            items.push("Stop");
            items.push("Reset (discard → clean)");
        } else {
            items.push("Start (from clean snapshot)");
        }
        items.push("Status");
        items.push("(back)");
        println!();
        let sel = Select::with_theme(&ColorfulTheme::default())
            .with_prompt(if running { "Executor — running" } else { "Executor — stopped" })
            .items(&items)
            .default(0)
            .interact()
            .unwrap_or(items.len() - 1);
        match items[sel] {
            "Start (from clean snapshot)" => executor_start(),
            "Stop" => executor_stop(),
            "Reset (discard → clean)" => executor_reset(),
            "Status" => executor_status(),
            _ => return,
        }
    }
}

fn tunnel_menu() {
    let items = ["Status", "Start", "Stop", "Set autostart", "(back)"];
    loop {
        println!();
        let sel = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Tunnel")
            .items(&items)
            .default(0)
            .interact()
            .unwrap_or(items.len() - 1);
        match items[sel] {
            "Status" => tunnel_status(),
            "Start" => tunnel_start(),
            "Stop" => tunnel_stop(),
            "Set autostart" => tunnel_autostart(),
            _ => return,
        }
    }
}

// ---------------------------------------------------------------------------
// Cloudflare tunnel (Windows service)
// ---------------------------------------------------------------------------

fn tunnel_start() {
    println!("Requesting administrator rights (approve the UAC prompt)...");
    if ps_elevated(&format!("Start-Service {TUNNEL_SVC}")) {
        println!("Tunnel started.");
    } else {
        eprintln!("Could not start the tunnel (UAC declined or service error).");
    }
}

fn tunnel_stop() {
    println!("Note: this stops the WHOLE tunnel (both your agent.* and guest.* hosts).");
    println!("Requesting administrator rights (approve the UAC prompt)...");
    if ps_elevated(&format!("Stop-Service {TUNNEL_SVC}")) {
        println!("Tunnel stopped.");
    } else {
        eprintln!("Could not stop the tunnel (UAC declined or service error).");
    }
}

fn tunnel_status() {
    // Build the lines by hand — Format-List pads output with blank lines.
    let _ = Command::new("powershell")
        .args([
            "-NoProfile", "-Command",
            &format!(
                "$s = Get-Service {TUNNEL_SVC} -ErrorAction SilentlyContinue; \
                 if ($s) {{ 'Name      : ' + $s.Name; 'Status    : ' + $s.Status; 'StartType : ' + $s.StartType }} \
                 else {{ 'not installed ({TUNNEL_SVC})' }}"
            ),
        ])
        .status();
}

fn tunnel_autostart() {
    println!("Requesting administrator rights (approve the UAC prompt)...");
    if ps_elevated(&format!("Set-Service -Name {TUNNEL_SVC} -StartupType Automatic")) {
        println!("Tunnel set to start automatically on boot.");
    } else {
        eprintln!("Could not change startup type (UAC declined).");
    }
}

// ---------------------------------------------------------------------------
// Combined status
// ---------------------------------------------------------------------------

fn status() {
    println!("== Executor VM ==");
    executor_status();
    println!("\n== Tunnel (service {TUNNEL_SVC}) ==");
    tunnel_status();
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run a PowerShell command elevated (via a UAC prompt) and wait for it — used
/// for the Cloudflared Windows-service ops, which require Administrator. Returns
/// true only if the elevated command succeeded. `inner` must not contain single
/// quotes (our tunnel commands don't).
fn ps_elevated(inner: &str) -> bool {
    let script = format!(
        "try {{ $p = Start-Process powershell -Verb RunAs -Wait -PassThru -ArgumentList \
         '-NoProfile','-Command','$ErrorActionPreference=''Stop''; {inner}'; \
         if ($p.ExitCode -ne 0) {{ exit 1 }} }} catch {{ exit 1 }}"
    );
    Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// VirtualBox executor VMs (Phase 2). Thin wrappers over VBoxManage — the same
// shape as the docker() helper, so agentctl can drive VM lifecycle (create /
// start headless / snapshot / port-forward / stop / delete) from the console.
// ---------------------------------------------------------------------------

mod vbox {
    use std::process::Command;

    /// The VBoxManage binary. Prefer PATH; on Windows fall back to the default
    /// install location so it works even if PATH wasn't updated.
    fn bin() -> String {
        #[cfg(windows)]
        {
            let default = r"C:\Program Files\Oracle\VirtualBox\VBoxManage.exe";
            if std::path::Path::new(default).exists() {
                return default.to_string();
            }
        }
        "VBoxManage".to_string()
    }

    /// Run VBoxManage, inheriting stdio; return whether it succeeded.
    pub fn run(args: &[&str]) -> bool {
        Command::new(bin()).args(args).status().map(|s| s.success()).unwrap_or(false)
    }

    /// Run VBoxManage capturing stdout (None on failure / no output).
    pub fn out(args: &[&str]) -> Option<String> {
        let o = Command::new(bin()).args(args).output().ok()?;
        if !o.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&o.stdout).into_owned())
    }

    pub fn version() -> Option<String> {
        out(&["--version"]).map(|s| s.trim().to_string())
    }

    /// A VM is registered / running if its quoted name appears in the listing.
    pub fn exists(name: &str) -> bool {
        out(&["list", "vms"]).is_some_and(|s| s.contains(&format!("\"{name}\"")))
    }
    pub fn running(name: &str) -> bool {
        out(&["list", "runningvms"]).is_some_and(|s| s.contains(&format!("\"{name}\"")))
    }

    /// Register a fresh VM with basic resources and a NAT NIC.
    pub fn create(name: &str, cpus: u32, mem_mb: u32) -> bool {
        run(&["createvm", "--name", name, "--ostype", "Ubuntu_64", "--register"])
            && run(&[
                "modifyvm", name,
                "--memory", &mem_mb.to_string(),
                "--cpus", &cpus.to_string(),
                "--nic1", "nat",
            ])
    }

    /// Forward host_port -> guest_port over the NAT NIC (rule name must be unique).
    pub fn port_forward(name: &str, rule: &str, host_port: u16, guest_port: u16) -> bool {
        run(&[
            "modifyvm", name,
            "--natpf1", &format!("{rule},tcp,,{host_port},,{guest_port}"),
        ])
    }

    pub fn start_headless(name: &str) -> bool {
        run(&["startvm", name, "--type", "headless"])
    }
    pub fn poweroff(name: &str) -> bool {
        run(&["controlvm", name, "poweroff"])
    }
    pub fn snapshot_take(name: &str, snap: &str) -> bool {
        run(&["snapshot", name, "take", snap])
    }
    pub fn snapshot_restore(name: &str, snap: &str) -> bool {
        run(&["snapshot", name, "restore", snap])
    }
    pub fn snapshot_list(name: &str) -> Option<String> {
        out(&["snapshot", name, "list", "--machinereadable"])
    }
    pub fn info(name: &str) -> Option<String> {
        out(&["showvminfo", name, "--machinereadable"])
    }
    /// Unregister and delete all files.
    pub fn delete(name: &str) -> bool {
        run(&["unregistervm", name, "--delete"])
    }
}

// The executor VM conventions (matches the base image built in Phase 0).
const EXECUTOR: &str = "executor";
const CLEAN_SNAPSHOT: &str = "clean";
const EXECUTOR_SSH_PORT: &str = "2222";
const EXECUTOR_SSH_USER: &str = "insider";

/// Path to the SSH key that authenticates into the executor VM (`%USERPROFILE%\.ssh\agent_vm_key`).
fn executor_ssh_key() -> String {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    format!("{home}\\.ssh\\agent_vm_key")
}

/// True once the executor accepts a key-based SSH connection (i.e. it has booted
/// far enough to be usable). Fast-fails via BatchMode + a short ConnectTimeout.
fn executor_ssh_ready() -> bool {
    Command::new("ssh")
        .args([
            "-i", &executor_ssh_key(),
            "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=accept-new",
            "-o", "ConnectTimeout=3",
            "-p", EXECUTOR_SSH_PORT,
            &format!("{EXECUTOR_SSH_USER}@127.0.0.1"),
            "true",
        ])
        // Quiet the expected "connection refused/timed out" churn while booting.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn vm_cli(args: &[String]) {
    match args.get(1).map(|s| s.as_str()) {
        Some("start") => executor_start(),
        Some("stop") => executor_stop(),
        Some("reset") => executor_reset(),
        Some("status") => executor_status(),
        Some("selftest") => vm_selftest(),
        Some("info") => match args.get(2) {
            Some(name) => match vbox::info(name) {
                Some(i) => print!("{i}"),
                None => eprintln!("VM '{name}' not found (or VBoxManage failed)."),
            },
            None => eprintln!("usage: agentctl vm info <name>"),
        },
        Some("delete") => match args.get(2) {
            Some(name) => {
                if vbox::running(name) {
                    let _ = vbox::poweroff(name);
                }
                if vbox::delete(name) {
                    println!("Deleted VM '{name}'.");
                } else {
                    eprintln!("Failed to delete '{name}' (does it exist?).");
                }
            }
            None => eprintln!("usage: agentctl vm delete <name>"),
        },
        _ => eprintln!("usage: agentctl vm [start|stop|reset|status|info <name>|selftest|delete <name>]"),
    }
}

/// Start a FRESH executor: restore the `clean` snapshot, boot headless, wait for
/// SSH. Each session begins from the pristine base (disposable executor).
fn executor_start() {
    if !vbox::exists(EXECUTOR) {
        eprintln!("VM '{EXECUTOR}' not found — build the base image (Phase 0) first.");
        return;
    }
    if vbox::running(EXECUTOR) {
        println!("Executor already running — powering off to start clean.");
        let _ = vbox::poweroff(EXECUTOR);
    }
    if !vbox::snapshot_restore(EXECUTOR, CLEAN_SNAPSHOT) {
        eprintln!("Failed to restore snapshot '{CLEAN_SNAPSHOT}'.");
        return;
    }
    if !vbox::start_headless(EXECUTOR) {
        eprintln!("Failed to start the executor VM.");
        return;
    }
    print!("Executor booting from '{CLEAN_SNAPSHOT}', waiting for SSH");
    for _ in 0..25 {
        if executor_ssh_ready() {
            println!("\nExecutor ready — SSH: 127.0.0.1:{EXECUTOR_SSH_PORT}, app: 127.0.0.1:18787");
            return;
        }
        print!(".");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    println!("\nStarted, but SSH not confirmed within ~75s (VM may still be booting).");
}

/// Stop the executor (hard power-off — state is disposable, discarded on next start).
fn executor_stop() {
    if vbox::running(EXECUTOR) && vbox::poweroff(EXECUTOR) {
        println!("Executor stopped.");
    } else {
        eprintln!("Executor is not running.");
    }
}

/// Discard the current session: power off (if running) and restore `clean`,
/// leaving the VM powered off and pristine for the next start.
fn executor_reset() {
    if vbox::running(EXECUTOR) {
        let _ = vbox::poweroff(EXECUTOR);
    }
    if vbox::snapshot_restore(EXECUTOR, CLEAN_SNAPSHOT) {
        println!("Executor reset to '{CLEAN_SNAPSHOT}' (powered off).");
    } else {
        eprintln!("Failed to restore snapshot '{CLEAN_SNAPSHOT}'.");
    }
}

fn executor_status() {
    if !vbox::exists(EXECUTOR) {
        println!("Executor VM '{EXECUTOR}': not created.");
        return;
    }
    println!(
        "Executor VM '{EXECUTOR}': {}",
        if vbox::running(EXECUTOR) { "RUNNING" } else { "stopped" }
    );
    if let Some(snaps) = vbox::snapshot_list(EXECUTOR) {
        print!("{snaps}");
    }
}

/// Exercise the whole VBoxManage lifecycle on a throwaway VM (no OS needed — we
/// only verify our wrappers drive create/modify/snapshot/delete correctly).
fn vm_selftest() {
    const NAME: &str = "agentctl-selftest";
    // Report a step and return its pass flag (caller folds into `ok`) — a closure
    // that captured `ok` would keep it mutably borrowed past the final read.
    let step = |label: &str, pass: bool| -> bool {
        println!("  [{}] {label}", if pass { "OK" } else { "!!" });
        pass
    };
    let mut ok = true;

    match vbox::version() {
        Some(v) => {
            step(&format!("VBoxManage {v}"), true);
        }
        None => {
            eprintln!("VBoxManage not found. Install VirtualBox or add it to PATH.");
            return;
        }
    }

    // Clean slate if a previous run left it behind.
    if vbox::exists(NAME) {
        if vbox::running(NAME) {
            let _ = vbox::poweroff(NAME);
        }
        let _ = vbox::delete(NAME);
    }

    ok &= step("create VM", vbox::create(NAME, 2, 2048));
    ok &= step("NAT port-forward 18787->8787", vbox::port_forward(NAME, "aw", 18787, 8787));

    // Verify the settings actually applied.
    let info = vbox::info(NAME).unwrap_or_default();
    ok &= step("memory=2048 applied", info.contains("memory=2048"));
    ok &= step("cpus=2 applied", info.contains("cpus=2"));
    ok &= step("port-forward applied", info.contains("18787") && info.contains("8787"));

    // Offline snapshot (no running VM needed).
    ok &= step("snapshot take 'clean'", vbox::snapshot_take(NAME, "clean"));
    let snaps = vbox::snapshot_list(NAME).unwrap_or_default();
    ok &= step("snapshot 'clean' listed", snaps.contains("clean"));
    ok &= step("snapshot restore 'clean'", vbox::snapshot_restore(NAME, "clean"));

    // Headless start is best-effort (a diskless VM can't boot; on the Hyper-V
    // backend it may refuse) — report, don't fail the suite on it.
    if vbox::start_headless(NAME) {
        println!("  [OK] start headless (diskless — will idle at no-boot)");
        std::thread::sleep(std::time::Duration::from_secs(2));
        println!("  [{}] running detected", if vbox::running(NAME) { "OK" } else { "--" });
        let _ = vbox::poweroff(NAME);
    } else {
        println!("  [--] start headless skipped (expected without a bootable disk)");
    }

    ok &= step("delete VM", vbox::delete(NAME));
    ok &= step("VM gone", !vbox::exists(NAME));

    println!("\n{}", if ok { "vm selftest: PASS" } else { "vm selftest: FAILED (see !! above)" });
}
