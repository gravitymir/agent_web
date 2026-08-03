//! Spawning a `claude` subprocess in streaming JSON mode.
//!
//! The process is driven by a [`crate::session::SessionKeeper`], which owns it
//! for the lifetime of a conversation (independent of any WebSocket): user turns
//! are written to stdin as `stream-json`, and Claude's events are read from
//! stdout line by line.

use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::config::Config;

/// A freshly spawned Claude Code process: its handle, piped stdin/stdout, and
/// the resolved session id.
pub struct Spawned {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub session_id: String,
}

/// Spawn a Claude Code session.
///
/// - `session_id`: `Some(id)` uses that id; `None` mints a fresh UUID.
/// - `resume`: when `true` and an id is given, resumes the existing chat
///   (`--resume`); otherwise starts a new one with that id (`--session-id`).
///   Lets the client pre-assign an id to a brand-new, named chat.
/// - `model`: optional model alias/name (e.g. `opus`, `sonnet`).
pub fn spawn_claude(
    config: &Config,
    session_id: Option<String>,
    resume: bool,
    model: Option<String>,
) -> Result<Spawned> {
    let (resume, id) = match session_id {
        Some(id) => (resume, id),
        None => (false, uuid::Uuid::new_v4().to_string()),
    };

    // A chat can exist only as the placeholder `.jsonl` that `ensure_chat_exists`
    // writes so a named new chat shows in the sidebar before its first turn — with
    // no real conversation yet. That stub satisfies NEITHER launch path: `--resume`
    // fails ("No conversation found with session ID") and `--session-id` fails
    // ("Session ID is already in use"). So: only `--resume` a chat that has a real
    // conversation, and for the start path delete a placeholder-only stub first so
    // `--session-id` gets a clean slate. A real transcript is never deleted.
    let session_jsonl = config.session_dir().join(format!("{id}.jsonl"));
    let has_conversation = jsonl_has_conversation(&session_jsonl);
    let resume = resume && has_conversation;
    if !resume && !has_conversation && session_jsonl.exists() {
        let _ = std::fs::remove_file(&session_jsonl);
    }

    let mut cmd = base_command(&config.claude_bin);
    cmd.current_dir(config.workspace_abs());

    cmd.arg("--print")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--verbose")
        .arg("--permission-mode")
        .arg(&config.permission_mode)
        // Without this, any tool needing a decision beyond `permission_mode`'s
        // own auto-approvals (Bash, WebFetch, AskUserQuestion, ...) silently
        // auto-denies in headless `--print` mode. This routes those decisions
        // to us instead, as `control_request`/`control_response` lines over
        // the same stdout/stdin — see `run_actor` in session.rs.
        .arg("--permission-prompt-tool")
        .arg("stdio");

    if resume {
        cmd.arg("--resume").arg(&id);
    } else {
        cmd.arg("--session-id").arg(&id);
    }

    if let Some(model) = model {
        if !model.is_empty() {
            cmd.arg("--model").arg(model);
        }
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("failed to spawn claude process")?;
    let stdin = child.stdin.take().context("missing child stdin")?;
    let stdout = child.stdout.take().context("missing child stdout")?;

    // Drain stderr to the log so diagnostics aren't lost.
    if let Some(stderr) = child.stderr.take() {
        let sid = id.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    tracing::warn!(session = %sid, "claude stderr: {line}");
                }
            }
        });
    }

    Ok(Spawned {
        child,
        stdin,
        stdout,
        session_id: id,
    })
}

/// True if the session `.jsonl` holds a real conversation (a `user`/`assistant`
/// turn), as opposed to just the placeholder line `ensure_chat_exists` writes.
/// A missing/unreadable file counts as "no conversation" (→ start fresh).
fn jsonl_has_conversation(path: &std::path::Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
            .is_some_and(|t| t == "user" || t == "assistant")
    })
}

/// Build the base [`Command`]. On Windows the CLI ships as `claude.cmd`, which
/// must be launched via `cmd /C`; elsewhere we invoke it directly.
fn base_command(bin: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(bin);
        cmd
    }
    #[cfg(not(windows))]
    {
        Command::new(bin)
    }
}
