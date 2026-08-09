//! agentctl — interactive control panel for the Agent Web guest sandbox.
//!
//! Aggregates the docker + tunnel commands and access-code minting behind an
//! arrow-key menu (run with no args) or direct subcommands (scriptable):
//!
//!   agentctl                 # interactive menu
//!   agentctl up              # start tunnel + guest container
//!   agentctl down            # stop guest container (tunnel left running)
//!   agentctl start|drain|stop|status|list|build
//!   agentctl code [label] [ttl]        # mint a magic link (default: guest 24h)
//!   agentctl tunnel start|stop|status|autostart
//!
//! The subscription token + public URL are read from .env; only those enter the
//! container (least privilege). The container runs with --restart unless-stopped
//! so it comes back after a reboot. The Cloudflare tunnel is a Windows service
//! (Cloudflared); start/stop/autostart need an Administrator terminal.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};

const IMAGE: &str = "agent-web:guest-sub";
const CONTAINER: &str = "agent-guest";
const HOST_PORT: &str = "127.0.0.1:8788";
const DEFAULT_URL: &str = "https://guest.astechlab.dev";
const TUNNEL_SVC: &str = "Cloudflared";
// Container resource caps — defaults; overridable per-run (interactive prompt or
// CWI_GUEST_CPUS / CWI_GUEST_MEMORY in .env).
const DEFAULT_CPUS: &str = "2";
const DEFAULT_MEMORY: &str = "2g";

fn main() {
    let base = base_dir();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("up") => {
            tunnel_start();
            let (cpus, memory) = resource_defaults(&base);
            start(&base, &cpus, &memory);
        }
        Some("down") => stop(),
        Some("start") => {
            let (cpus, memory) = resource_defaults(&base);
            start(&base, &cpus, &memory);
        }
        Some("drain") => drain(),
        Some("stop") => stop(),
        Some("status") => status(),
        Some("list") => list_codes(),
        Some("build") => build(&base),
        Some("code") => {
            let label = args.get(1).cloned().unwrap_or_else(|| "guest".into());
            let ttl = args.get(2).cloned().unwrap_or_else(|| "24h".into());
            new_code(&base, &label, &ttl);
        }
        Some("revoke") => {
            let label = args.get(1).cloned().unwrap_or_default();
            if label.is_empty() {
                eprintln!("usage: agentctl revoke <label>");
            } else {
                revoke(&label);
            }
        }
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
        None => interactive(&base),
    }
}

fn usage() {
    eprintln!(
        "usage: agentctl [up|down|start|drain|stop|status|list|build|code [label] [ttl]|revoke <label>|tunnel <start|stop|status|autostart>|vm <selftest|status <name>|delete <name>>]"
    );
}

fn interactive(base: &Path) {
    let items = ["Guest", "Tunnel", "Status", "Quit"];
    loop {
        println!();
        let sel = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Agent Web — control panel")
            .items(&items)
            .default(0)
            .interact()
            .unwrap_or(items.len() - 1);
        match items[sel] {
            "Guest" => guest_menu(base),
            "Tunnel" => tunnel_menu(),
            "Status" => status(),
            _ => break,
        }
    }
}

/// Guest submenu — start/stop shown by context; Codes opens the code manager.
fn guest_menu(base: &Path) {
    loop {
        let running = container_running();
        let mut items: Vec<&str> = Vec::new();
        if running {
            items.push("Drain (finish active turns, then safe to stop)");
            items.push("Stop container");
            items.push("Codes (list / new / revoke)");
        } else {
            items.push("Start container");
        }
        items.push("Build image");
        items.push("(back)");
        println!();
        let sel = Select::with_theme(&ColorfulTheme::default())
            .with_prompt(if running { "Guest — running" } else { "Guest — stopped" })
            .items(&items)
            .default(0)
            .interact()
            .unwrap_or(items.len() - 1);
        match items[sel] {
            "Start container" => {
                let (cpus, memory) = prompt_resources(base);
                start(base, &cpus, &memory);
            }
            "Drain (finish active turns, then safe to stop)" => drain(),
            "Stop container" => stop(),
            "Codes (list / new / revoke)" => codes_menu(base),
            "Build image" => build(base),
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
// Guest container
// ---------------------------------------------------------------------------

fn start(base: &Path, cpus: &str, memory: &str) {
    let token = env_var(base, "CLAUDE_CODE_OAUTH_TOKEN");
    if token.is_empty() {
        eprintln!(
            "CLAUDE_CODE_OAUTH_TOKEN is not set in {}. Run `claude setup-token` and put it in .env.",
            base.join(".env").display()
        );
        return;
    }
    let url = {
        let u = env_var(base, "CWI_PUBLIC_URL");
        if u.is_empty() { DEFAULT_URL.to_string() } else { u }
    };
    let ws = base.join("guest-workspace");
    let _ = std::fs::create_dir_all(&ws);

    // Owner usage snapshot (plan + limits) — mounted read-only so the guest badge
    // can show them. Ensure the FILE exists (else Docker mounts a directory).
    let snap = base.join("target").join("release").join("chats").join("usage_snapshot.json");
    if let Some(dir) = snap.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if !snap.exists() {
        let _ = std::fs::write(&snap, "{}");
    }

    let _ = docker(&["rm", "-f", CONTAINER]); // ignore "no such container"

    let mount = format!("{}:/workspace", ws.display());
    let snap_mount = format!("{}:/owner_usage.json:ro", snap.display());
    let token_env = format!("CLAUDE_CODE_OAUTH_TOKEN={token}");
    let url_env = format!("CWI_PUBLIC_URL={url}");
    let port = format!("{HOST_PORT}:8787");
    let args: Vec<&str> = vec![
        "run", "-d", "--name", CONTAINER,
        "--restart", "unless-stopped", // survive reboots / Docker restarts
        "--read-only", "--cap-drop", "ALL",
        // entrypoint (root) needs NET_ADMIN to install egress rules and
        // SETUID/SETGID so gosu can drop to the unprivileged 'guest' user.
        "--cap-add", "NET_ADMIN", "--cap-add", "SETUID", "--cap-add", "SETGID",
        "--security-opt", "no-new-privileges",
        "--pids-limit", "512", "--memory", memory, "--cpus", cpus, "--tmpfs", "/tmp",
        "-v", "agent_guest_chats:/chats", "-v", &mount,
        "-v", &snap_mount,
        "-e", &token_env, "-e", &url_env,
        "-e", "CWI_USAGE_FILE=/owner_usage.json",
        "-p", &port, IMAGE,
    ];
    if docker(&args).map(|s| s.success()).unwrap_or(false) {
        println!("\nGuest container started on http://{HOST_PORT}  (cpus={cpus}, memory={memory})");
        println!("Public: {url}  (mint a code from this panel or with `agentctl code`)");
    } else {
        eprintln!("\nFailed to start. Is Docker running and the image built? (menu \"build image\")");
    }
}

/// Ask Docker how much it can actually give a container: (NCPU, MemTotal bytes).
/// This reflects the Docker/WSL2 VM's allocation, which is the real ceiling for
/// `--cpus` / `--memory` (it can be less than the host's physical resources).
fn docker_capacity() -> (Option<u64>, Option<u64>) {
    let ncpu = docker_info("{{.NCPU}}").and_then(|s| s.parse().ok());
    let mem = docker_info("{{.MemTotal}}").and_then(|s| s.parse().ok());
    (ncpu, mem)
}

fn docker_info(fmt: &str) -> Option<String> {
    let out = Command::new("docker").args(["info", "--format", fmt]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Non-interactive resource caps: `.env` overrides (CWI_GUEST_CPUS /
/// CWI_GUEST_MEMORY), else the built-in defaults. Used by the `start`/`up`
/// subcommands, which must not block on a prompt.
fn resource_defaults(base: &Path) -> (String, String) {
    let pick = |key: &str, default: &str| {
        let v = env_var(base, key);
        if v.is_empty() { default.to_string() } else { v }
    };
    (pick("CWI_GUEST_CPUS", DEFAULT_CPUS), pick("CWI_GUEST_MEMORY", DEFAULT_MEMORY))
}

/// Interactive resource picker: show what Docker can give, then prompt (Enter
/// keeps the current default). Handy when the machine is otherwise idle and you
/// want to hand the container more.
fn prompt_resources(base: &Path) -> (String, String) {
    let (max_cpu, max_mem) = docker_capacity();
    let (def_cpu, def_mem) = resource_defaults(base);

    let cpu_hint = max_cpu
        .map(|c| format!(" (Docker sees {c} CPUs)"))
        .unwrap_or_default();
    let cpus: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt(format!("CPUs{cpu_hint}"))
        .default(def_cpu.clone())
        .interact_text()
        .unwrap_or(def_cpu);

    let mem_hint = max_mem
        .map(|b| format!(" (Docker has ~{} GB)", b / 1_073_741_824))
        .unwrap_or_default();
    let memory: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt(format!("Memory — e.g. 2g, 512m{mem_hint}"))
        .default(def_mem.clone())
        .interact_text()
        .unwrap_or(def_mem);

    (cpus.trim().to_string(), memory.trim().to_string())
}

fn stop() {
    if docker(&["stop", CONTAINER]).map(|s| s.success()).unwrap_or(false) {
        println!("Guest container stopped. (data kept; `start` brings it back)");
    } else {
        eprintln!("Nothing to stop (container not running?).");
    }
}

/// Graceful drain: tell the running app to stop accepting NEW turns (via SIGUSR1)
/// and wait until every in-flight agent turn finishes, so `stop` won't cut a guest
/// off mid-answer. Polls /api/health (exempt from the access gate) on the host
/// loopback port; when `active_turns` hits zero it's safe to stop.
fn drain() {
    if !container_running() {
        eprintln!("Guest container is not running — nothing to drain.");
        return;
    }
    // Flip the app into draining mode. SIGUSR1 is caught by the app (unix only);
    // `docker kill --signal` just delivers the signal, it does not stop the box.
    println!("Draining: telling the app to refuse new turns and finish active ones...");
    if !docker(&["kill", "--signal=SIGUSR1", CONTAINER])
        .map(|s| s.success())
        .unwrap_or(false)
    {
        eprintln!("Could not send the drain signal (SIGUSR1) to the container.");
        return;
    }

    let url = format!("http://{HOST_PORT}/api/health");
    // Poll for up to ~10 minutes; a single turn rarely runs longer, and the
    // operator can Ctrl-C and re-run at any time (drain state is sticky).
    const MAX_POLLS: u32 = 200;
    const EVERY: std::time::Duration = std::time::Duration::from_secs(3);
    let mut last = u64::MAX;
    for i in 0..MAX_POLLS {
        std::thread::sleep(EVERY);
        let Some(body) = http_get(&url) else {
            eprintln!("  (health check unreachable — retrying)");
            continue;
        };
        let json: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let draining = json.get("draining").and_then(serde_json::Value::as_bool).unwrap_or(false);
        let active = json.get("active_turns").and_then(serde_json::Value::as_u64).unwrap_or(0);
        if !draining {
            // The app didn't pick up the signal (older build without the handler?).
            eprintln!("  App is not in draining mode — is this build drain-aware? Aborting.");
            return;
        }
        if active != last {
            println!("  active turns: {active}");
            last = active;
        }
        if active == 0 {
            println!("\nDrained — no active turns. It's now safe to stop:");
            println!("  agentctl stop");
            return;
        }
        if i + 1 == MAX_POLLS {
            eprintln!("\nStill {active} active turn(s) after waiting. Left in draining mode.");
            eprintln!("Re-run `agentctl drain` to keep waiting, or `agentctl stop` to force.");
        }
    }
}

/// Minimal HTTP GET via the system `curl` (built in on Windows 11 and Linux).
/// Returns the response body, or None on any failure.
fn http_get(url: &str) -> Option<String> {
    let out = Command::new("curl")
        .args(["-s", "--max-time", "5", url])
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn new_code(base: &Path, label: &str, ttl: &str) {
    if !container_running() {
        eprintln!("Guest container is not running — start it first.");
        return;
    }
    let url = {
        let u = env_var(base, "CWI_PUBLIC_URL");
        if u.is_empty() { DEFAULT_URL.to_string() } else { u }
    };
    let _ = docker(&[
        "exec", "-u", "10001", CONTAINER, "/app/agent_web", "guest", "new",
        "--ttl", ttl, "--label", label, "--url", &url,
    ]);
}

fn list_codes() {
    if !container_running() {
        eprintln!("Guest container is not running — start it first.");
        return;
    }
    // Only labels + expiry can be listed — codes are stored hashed and shown
    // once at mint time (a leaked store yields no usable links).
    let _ = docker(&["exec", "-u", "10001", CONTAINER, "/app/agent_web", "guest", "list"]);
}

/// Active codes as (label, display) — parsed from `guest list`. Display is the
/// full "label … expires in …" line; label (first token) is what revoke needs.
/// Assumes single-word labels.
fn active_entries() -> Vec<(String, String)> {
    let Ok(out) = Command::new("docker")
        .args(["exec", "-u", "10001", CONTAINER, "/app/agent_web", "guest", "list"])
        .output()
    else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with("no active"))
        .filter_map(|l| l.split_whitespace().next().map(|lbl| (lbl.to_string(), l.to_string())))
        .collect()
}

fn revoke(label: &str) {
    if !container_running() {
        eprintln!("Guest container is not running — start it first.");
        return;
    }
    let _ = docker(&["exec", "-u", "10001", CONTAINER, "/app/agent_web", "guest", "revoke", label]);
}

/// Interactive codes submenu: `[ new code ]` plus each active code. Picking a
/// code confirms and revokes it; picking new mints one. Loops until "(back)".
fn codes_menu(base: &Path) {
    if !container_running() {
        eprintln!("Guest container is not running — start it first.");
        return;
    }
    loop {
        let entries = active_entries();
        let mut items: Vec<String> = vec!["[ new code ]".into()];
        for (_, d) in &entries {
            items.push(d.clone());
        }
        items.push("(back)".into());
        println!();
        let sel = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Codes — pick a code to revoke, or create a new one")
            .items(&items)
            .default(0)
            .interact()
            .unwrap_or(items.len() - 1);
        if sel == 0 {
            let (label, ttl) = prompt_new();
            new_code(base, &label, &ttl);
        } else if sel == items.len() - 1 {
            return; // (back)
        } else {
            let (label, _) = &entries[sel - 1];
            let yes = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!("Revoke '{label}'? This immediately kills its magic link"))
                .default(false)
                .interact()
                .unwrap_or(false);
            if yes {
                revoke(label);
            }
        }
    }
}

/// Prompt for a label + TTL when minting a code interactively.
fn prompt_new() -> (String, String) {
    let label: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Label (who is this for)")
        .default("guest".into())
        .interact_text()
        .unwrap_or_else(|_| "guest".into());
    let ttl: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Valid for (e.g. 24h, 7d, 30m)")
        .default("24h".into())
        .interact_text()
        .unwrap_or_else(|_| "24h".into());
    (label, ttl)
}

fn build(base: &Path) {
    println!("Building {IMAGE} (this takes a few minutes)...");
    let ok = Command::new("docker")
        .args(["build", "-f", "Dockerfile.guest", "-t", IMAGE, "."])
        .current_dir(base)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok { println!("Image built."); } else { eprintln!("Build failed. Is Docker running?"); }
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
    println!("== Guest container ==");
    let _ = docker(&[
        "ps", "-a", "--filter", &format!("name={CONTAINER}"),
        "--format", "table {{.Names}}\t{{.Status}}\t{{.Ports}}",
    ]);
    println!("\n== Tunnel (service {TUNNEL_SVC}) ==");
    tunnel_status();
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn docker(args: &[&str]) -> std::io::Result<ExitStatus> {
    Command::new("docker").args(args).status()
}

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

fn container_running() -> bool {
    Command::new("docker")
        .args(["ps", "--filter", &format!("name={CONTAINER}"), "--format", "{{.Names}}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(CONTAINER))
        .unwrap_or(false)
}

/// Directory that holds `.env` — checks cwd, then next to the exe, then the dev
/// `target/<profile>/` layout (exe/../..). Falls back to cwd.
fn base_dir() -> PathBuf {
    let mut cands = vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))];
    if let Ok(exe) = std::env::current_exe()
        && let Some(d) = exe.parent() {
            cands.push(d.to_path_buf());
            cands.push(d.join("..").join(".."));
        }
    for c in &cands {
        if c.join(".env").is_file() {
            return c.clone();
        }
    }
    cands.into_iter().next().unwrap()
}

/// Read a single KEY=VALUE from `<base>/.env`, ignoring commented lines.
fn env_var(base: &Path, name: &str) -> String {
    let Ok(content) = std::fs::read_to_string(base.join(".env")) else {
        return String::new();
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == name {
                return v.trim().trim_matches('"').to_string();
            }
    }
    String::new()
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

/// `agentctl vm <selftest|status|delete> [name]` — Phase-2 VM controls. The full
/// interactive integration (menus, base image, broker wiring) comes next; for now
/// this exercises and exposes the VBoxManage lifecycle directly.
fn vm_cli(args: &[String]) {
    match args.get(1).map(|s| s.as_str()) {
        Some("selftest") => vm_selftest(),
        Some("status") => match args.get(2) {
            Some(name) => match vbox::info(name) {
                Some(i) => print!("{i}"),
                None => eprintln!("VM '{name}' not found (or VBoxManage failed)."),
            },
            None => eprintln!("usage: agentctl vm status <name>"),
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
        _ => eprintln!("usage: agentctl vm [selftest|status <name>|delete <name>]"),
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
