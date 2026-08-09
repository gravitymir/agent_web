//! agentctl — interactive control panel for the Agent Web guest sandbox.
//!
//! Aggregates the docker commands and access-code minting behind an arrow-key
//! menu (run with no args) or direct subcommands (scriptable), so you don't have
//! to remember docker invocations.
//!
//!   agentctl                 # interactive menu
//!   agentctl start           # (re)start the locked guest container
//!   agentctl stop            # stop it
//!   agentctl status          # is it up?
//!   agentctl code [label] [ttl]   # mint a magic link (default: guest 24h)
//!   agentctl list            # list active codes
//!   agentctl build           # (re)build the guest image
//!
//! The subscription token and public URL are read from .env; only those enter
//! the container (least privilege — see run-guest.ps1).

use std::path::PathBuf;
use std::process::Command;

use dialoguer::{theme::ColorfulTheme, Input, Select};

const IMAGE: &str = "agent-web:guest-sub";
const CONTAINER: &str = "agent-guest";
const HOST_PORT: &str = "127.0.0.1:8788";
const DEFAULT_URL: &str = "https://guest.astechlab.dev";

fn main() {
    let base = base_dir();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
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
        Some(other) => {
            eprintln!("unknown command: {other}");
            usage();
        }
        None => interactive(&base),
    }
}

fn usage() {
    eprintln!("usage: agentctl [start|stop|status|list|build|code [label] [ttl]]");
}

fn interactive(base: &PathBuf) {
    let items = [
        "Guest: start container",
        "Guest: stop container",
        "Guest: new access code (magic link)",
        "Guest: list codes",
        "Guest: status",
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
            0 => start(base),
            1 => stop(),
            2 => {
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
            3 => list_codes(),
            4 => status(),
            5 => build(base),
            _ => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Actions
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
        eprintln!("\nFailed to start. Is Docker running and the image built? (menu option \"build image\")");
    }
}

fn stop() {
    if docker(&["stop", CONTAINER]).map(|s| s.success()).unwrap_or(false) {
        println!("Guest container stopped. (data kept; `start` brings it back)");
    } else {
        eprintln!("Nothing to stop (container not running?).");
    }
}

fn status() {
    let _ = docker(&[
        "ps", "-a", "--filter", &format!("name={CONTAINER}"),
        "--format", "table {{.Names}}\t{{.Status}}\t{{.Ports}}",
    ]);
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
    let _ = docker(&["exec", CONTAINER, "/app/agent_web", "guest", "list"]);
}

fn build(base: &PathBuf) {
    println!("Building {IMAGE} (this takes a few minutes)...");
    let mut cmd = Command::new("docker");
    cmd.args(["build", "-f", "Dockerfile.guest", "-t", IMAGE, "."])
        .current_dir(base);
    match cmd.status() {
        Ok(s) if s.success() => println!("Image built."),
        _ => eprintln!("Build failed. Is Docker running?"),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn docker(args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    Command::new("docker").args(args).status()
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

/// Read a single KEY=VALUE from `<base>/.env`, ignoring commented lines. Returns
/// "" if absent.
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
