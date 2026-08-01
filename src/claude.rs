//! Spawning a `claude` subprocess in streaming JSON mode.
//!
//! The process is driven by a [`crate::session::SessionKeeper`], which owns it
//! for the lifetime of a conversation (independent of any WebSocket): user turns
//! are written to stdin as `stream-json`, and Claude's events are read from
//! stdout line by line.

use std::process::Stdio;

use anyhow::{Context, Result};
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
        .arg(&config.permission_mode);

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
        .stderr(Stdio::null());

    let mut child = cmd.spawn().context("failed to spawn claude process")?;
    let stdin = child.stdin.take().context("missing child stdin")?;
    let stdout = child.stdout.take().context("missing child stdout")?;

    Ok(Spawned {
        child,
        stdin,
        stdout,
        session_id: id,
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
