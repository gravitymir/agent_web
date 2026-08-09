//! Subscription usage/limits for the sidebar/settings, sourced from the Claude
//! Code CLI's own `/usage` screen.
//!
//! There is no documented API for subscription quota, but `claude -p "/usage"
//! --output-format json` prints the same data the interactive `/usage` command
//! shows — the 5-hour ("session") window and the weekly limit, as percentages
//! plus reset times — inside the envelope's `result` string. Crucially it costs
//! nothing (`num_turns: 0`, zero tokens), so it's safe to poll behind a cache.
//!
//! We spawn the CLI exactly like the keeper does (`cmd /C claude …` on Windows),
//! so it inherits `CLAUDE_CONFIG_DIR` / `CLAUDE_CODE_OAUTH_TOKEN` from the env.

use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::Config;

/// Cache the (relatively slow ~300ms) CLI spawn so repeated settings-panel opens
/// don't re-run it. The data changes slowly, so a short TTL is plenty.
static CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);
const TTL: Duration = Duration::from_secs(20);

/// Return the usage payload for `/api/usage`, served from cache when fresh.
pub async fn usage_json(config: &Config) -> Value {
    // The subscription limits only reflect what the CLI engine consumes; in
    // native mode the app talks to a different provider, so they'd be misleading.
    if config.native_engine {
        return json!({ "available": false, "reason": "native" });
    }
    {
        // Recover from a poisoned lock instead of propagating a panic — a stale
        // cache is harmless and better than taking down every /api/usage request.
        let guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, v)) = guard.as_ref()
            && t.elapsed() < TTL {
                return v.clone();
            }
    }
    let v = fetch(config).await;
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v.clone()));
    v
}

async fn fetch(config: &Config) -> Value {
    let raw = match run_claude(config, &["-p", "/usage", "--output-format", "json"]).await {
        Some(s) => s,
        None => return json!({ "available": false, "reason": "spawn_failed" }),
    };
    // The `--output-format json` envelope carries the human-readable screen in
    // `result`; older/other output may be the screen text directly.
    let result_text = serde_json::from_str::<Value>(raw.trim())
        .ok()
        .and_then(|v| v.get("result").and_then(Value::as_str).map(str::to_string))
        .unwrap_or(raw);

    if !result_text.contains("Current session") && !result_text.contains("Current week") {
        return json!({ "available": false, "reason": "unrecognized" });
    }

    let session = parse_line(&result_text, "Current session:");
    let week = parse_line(&result_text, "Current week (all models):");
    let fable = parse_line(&result_text, "Current week (Fable):");

    let account = run_claude(config, &["auth", "status"])
        .await
        .and_then(|s| serde_json::from_str::<Value>(s.trim()).ok());
    let plan = account
        .as_ref()
        .and_then(|a| a.get("subscriptionType").and_then(Value::as_str))
        .map(str::to_string);
    let email = account
        .as_ref()
        .and_then(|a| a.get("email").and_then(Value::as_str))
        .map(str::to_string);

    json!({
        "available": session.is_some() || week.is_some(),
        "session": session.map(|(p, r)| json!({ "percent": p, "resets": r })),
        "week": week.map(|(p, r)| json!({ "percent": p, "resets": r })),
        "fable": fable.map(|(p, r)| json!({ "percent": p, "resets": r })),
        "plan": plan,
        "email": email,
    })
}

/// Parse a line like `Current session: 25% used · resets Aug 3, 1:19am (…)` into
/// `(percent, Some("Aug 3, 1:19am (…)"))`.
fn parse_line(text: &str, prefix: &str) -> Option<(u32, Option<String>)> {
    let line = text.lines().map(str::trim).find(|l| l.starts_with(prefix))?;
    let rest = line[prefix.len()..].trim();
    let percent: u32 = rest.split('%').next()?.trim().parse().ok()?;
    let resets = rest
        .split("resets")
        .nth(1)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some((percent, resets))
}

/// Spawn the `claude` CLI with `args`, capture stdout. `stdin` is closed so an
/// unexpected interactive prompt can't hang the request.
///
/// The isolated `CLAUDE_CONFIG_DIR` + `setup-token` auth only returns the header
/// line of `/usage` (no percentages); the primary desktop login returns the full
/// breakdown. So for these read-only meta queries we strip the isolation env and
/// let the CLI use the default `~/.claude` login. This creates no chats there
/// (`/usage` and `auth status` don't run a turn), so chat isolation is preserved.
async fn run_claude(config: &Config, args: &[&str]) -> Option<String> {
    use tokio::process::Command;
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(&config.claude_bin);
        c
    } else {
        Command::new(&config.claude_bin)
    };
    cmd.args(args)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let out = cmd.output().await.ok()?;
    if out.stdout.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}
