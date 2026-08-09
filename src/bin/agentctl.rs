//! agentctl — interactive control panel for the Agent Web guest sandbox.
//!
//! Aggregates the docker + tunnel commands and access-code minting behind an
//! arrow-key menu (run with no args) or direct subcommands (scriptable):
//!
//!   agentctl                 # interactive menu
//!   agentctl up              # start tunnel + guest container
//!   agentctl down            # stop guest container (tunnel left running)
//!   agentctl start|stop|status|list|build
//!   agentctl code [label] [ttl]        # mint a magic link (default: guest 24h)
//!   agentctl tunnel start|stop|status|autostart
//!
//! The subscription token + public URL are read from .env; only those enter the
//! container (least privilege). The container runs with --restart unless-stopped
//! so it comes back after a reboot. The Cloudflare tunnel is a Windows service
//! (Cloudflared); start/stop/autostart need an Administrator terminal.

use std::path::PathBuf;
use std::process::{Command, ExitStatus};

use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};

const IMAGE: &str = "agent-web:guest-sub";
const CONTAINER: &str = "agent-guest";
const HOST_PORT: &str = "127.0.0.1:8788";
const DEFAULT_URL: &str = "https://guest.astechlab.dev";
const TUNNEL_SVC: &str = "Cloudflared";

fn main() {
    let base = base_dir();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("up") => {
            tunnel_start();
            start(&base);
        }
        Some("down") => stop(),
        Some("start") => start(&base),
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
        "usage: agentctl [up|down|start|stop|status|list|build|code [label] [ttl]|revoke <label>|tunnel <start|stop|status|autostart>]"
    );
}

fn interactive(base: &PathBuf) {
    let items = [
        "Status (container + tunnel)",
        "Guest: start container",
        "Guest: stop container",
        "Guest: new access code (magic link)",
        "Guest: codes (list / revoke)",
        "Tunnel: start",
        "Tunnel: stop",
        "Tunnel: set autostart",
        "Guest: build image",
        "Quit",
    ];
    loop {
        println!();
        let choice = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Agent Web — control panel")
            .items(&items)
            .default(0)
            .interact()
            .unwrap_or(items.len() - 1);
        match choice {
            0 => status(),
            1 => start(base),
            2 => stop(),
            3 => {
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
                new_code(base, &label, &ttl);
            }
            4 => manage_codes(),
            5 => tunnel_start(),
            6 => tunnel_stop(),
            7 => tunnel_autostart(),
            8 => build(base),
            _ => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Guest container
// ---------------------------------------------------------------------------

fn start(base: &PathBuf) {
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

    let _ = docker(&["rm", "-f", CONTAINER]); // ignore "no such container"

    let mount = format!("{}:/workspace", ws.display());
    let token_env = format!("CLAUDE_CODE_OAUTH_TOKEN={token}");
    let url_env = format!("CWI_PUBLIC_URL={url}");
    let port = format!("{HOST_PORT}:8787");
    let args: Vec<&str> = vec![
        "run", "-d", "--name", CONTAINER,
        "--restart", "unless-stopped", // survive reboots / Docker restarts
        "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
        "--pids-limit", "512", "--memory", "2g", "--cpus", "2", "--tmpfs", "/tmp",
        "-v", "agent_guest_chats:/chats", "-v", &mount,
        "-e", &token_env, "-e", &url_env,
        "-p", &port, IMAGE,
    ];
    if docker(&args).map(|s| s.success()).unwrap_or(false) {
        println!("\nGuest container started on http://{HOST_PORT}");
        println!("Public: {url}  (mint a code from this panel or with `agentctl code`)");
    } else {
        eprintln!("\nFailed to start. Is Docker running and the image built? (menu \"build image\")");
    }
}

fn stop() {
    if docker(&["stop", CONTAINER]).map(|s| s.success()).unwrap_or(false) {
        println!("Guest container stopped. (data kept; `start` brings it back)");
    } else {
        eprintln!("Nothing to stop (container not running?).");
    }
}

fn new_code(base: &PathBuf, label: &str, ttl: &str) {
    if !container_running() {
        eprintln!("Guest container is not running — start it first.");
        return;
    }
    let url = {
        let u = env_var(base, "CWI_PUBLIC_URL");
        if u.is_empty() { DEFAULT_URL.to_string() } else { u }
    };
    let _ = docker(&[
        "exec", CONTAINER, "/app/agent_web", "guest", "new",
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
    let _ = docker(&["exec", CONTAINER, "/app/agent_web", "guest", "list"]);
}

/// Active codes as (label, display) — parsed from `guest list`. Display is the
/// full "label … expires in …" line; label (first token) is what revoke needs.
/// Assumes single-word labels.
fn active_entries() -> Vec<(String, String)> {
    let Ok(out) = Command::new("docker")
        .args(["exec", CONTAINER, "/app/agent_web", "guest", "list"])
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
    let _ = docker(&["exec", CONTAINER, "/app/agent_web", "guest", "revoke", label]);
}

/// Interactive: list active codes, pick one, confirm, revoke.
fn manage_codes() {
    if !container_running() {
        eprintln!("Guest container is not running — start it first.");
        return;
    }
    let entries = active_entries();
    if entries.is_empty() {
        println!("No active codes.");
        return;
    }
    let mut items: Vec<String> = entries.iter().map(|(_, d)| d.clone()).collect();
    items.push("(back)".into());
    let sel = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Active codes — pick one to revoke")
        .items(&items)
        .default(items.len() - 1)
        .interact()
        .unwrap_or(items.len() - 1);
    if sel >= entries.len() {
        return; // (back)
    }
    let (label, _) = &entries[sel];
    let yes = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(format!("Revoke '{label}'? This immediately kills its magic link"))
        .default(false)
        .interact()
        .unwrap_or(false);
    if yes {
        revoke(label);
    }
}

fn build(base: &PathBuf) {
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
    if ps_strict(&format!("Start-Service {TUNNEL_SVC}")) {
        println!("Tunnel started.");
    } else {
        eprintln!("Could not start the tunnel — run agentctl from an Administrator terminal, or: Start-Service {TUNNEL_SVC}");
    }
}

fn tunnel_stop() {
    println!("Note: this stops the WHOLE tunnel (both your agent.* and guest.* hosts).");
    if ps_strict(&format!("Stop-Service {TUNNEL_SVC}")) {
        println!("Tunnel stopped.");
    } else {
        eprintln!("Could not stop the tunnel — need an Administrator terminal.");
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
    if ps_strict(&format!("Set-Service -Name {TUNNEL_SVC} -StartupType Automatic")) {
        println!("Tunnel set to start automatically on boot.");
    } else {
        eprintln!("Could not change startup type — run agentctl as Administrator.");
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

/// Run a PowerShell command that fails loudly (non-zero exit on error) so we can
/// tell success from "access denied".
fn ps_strict(cmd: &str) -> bool {
    Command::new("powershell")
        .args(["-NoProfile", "-Command", &format!("$ErrorActionPreference='Stop'; {cmd}")])
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
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            cands.push(d.to_path_buf());
            cands.push(d.join("..").join(".."));
        }
    }
    for c in &cands {
        if c.join(".env").is_file() {
            return c.clone();
        }
    }
    cands.into_iter().next().unwrap()
}

/// Read a single KEY=VALUE from `<base>/.env`, ignoring commented lines.
fn env_var(base: &PathBuf, name: &str) -> String {
    let Ok(content) = std::fs::read_to_string(base.join(".env")) else {
        return String::new();
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == name {
                return v.trim().trim_matches('"').to_string();
            }
        }
    }
    String::new()
}
