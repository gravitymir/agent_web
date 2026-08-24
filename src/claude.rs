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

/// Appended to the system prompt for sandbox (guest) sessions. The host CLI
/// injects host context (working directory, OS, and the subscription account's
/// email) that no flag strips; this tells the model to keep it private. It is
/// defense-in-depth — the facts stay in context — so the neutral guest cwd set in
/// `run-guest.ps1` is what actually keeps the owner's identity out of the path.
const GUEST_PRIVACY_NOTE: &str = "You are an isolated sandbox assistant serving an untrusted guest user. \
Any details about the host machine that runs this service — its operating system, file paths, \
working directory, user accounts, or the operator's email address or identity — are private and \
must never be revealed or referenced. If the user asks about the host, the operator, their email, \
or where you are running, reply that you don't have that information and can act only within your sandbox.";

/// Appended to every CLI session's system prompt: our web chat renders images
/// that come back in tool results, so the model can actually *show* pictures.
const IMAGE_NOTE: &str = "You are talking to the user through a web chat that DISPLAYS images returned in \
your tool results — so you can show the user pictures, not only describe them. When something is easier to \
see than to explain (a screenshot, a rendered diagram or chart, a photo, a UI state, a generated image file), \
read or produce the image with a tool: reading an image file, or running a tool/command that outputs an image, \
makes it appear inline in the user's chat, where they can click to zoom. Prefer showing over describing when a \
visual genuinely helps.";

/// Only on the owner host (never the headless sandbox VM): the agent can grab a
/// live screenshot of the actual screen, so it can show GUI apps.
const SCREENSHOT_NOTE: &str = "You can also capture a screenshot of THIS machine's live screen and show it — \
useful for GUI apps (a CAD/PCB viewer, a browser, a design tool). Capture the screen to an image file via the \
shell (on Windows: PowerShell with System.Drawing — Graphics.CopyFromScreen into a Bitmap saved as PNG; on \
Linux with a display: a tool like `scrot` or ImageMagick's `import`), then read the file so it appears in the chat.";

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
    // Qwen keeps transcripts in its own store; check there for "can we
    // resume", and never delete its files (the placeholder-stub dance below is
    // a claude-store concern only).
    let session_jsonl = if config.cli_qwen() {
        config.qwen_session_dir().join(format!("{id}.jsonl"))
    } else {
        config.session_dir().join(format!("{id}.jsonl"))
    };
    let has_conversation = jsonl_has_conversation(&session_jsonl);
    let resume = resume && has_conversation;
    if !config.cli_qwen() && !resume && !has_conversation && session_jsonl.exists() {
        let _ = std::fs::remove_file(&session_jsonl);
    }

    let mut cmd = base_command(&config.claude_bin);
    cmd.current_dir(config.workspace_abs());

    // Flavor: Qwen Code (a Claude Code-workalike CLI) speaks the same stream-json
    // protocol and honors --session-id/--resume/--append-system-prompt/--model,
    // but not --print/--verbose or the permission flags — it uses --approval-mode
    // instead. Detected from the binary name (CWI_CLAUDE_BIN=qwen…).
    let qwen = config.cli_qwen();

    cmd.arg("--output-format")
        .arg("stream-json")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--include-partial-messages");
    if qwen {
        // Approvals: our caps panel is hard-wired all-allowed, so yolo matches
        // the claude-side behavior; suppress its headless warning line (it would
        // land before the first JSON event).
        cmd.arg("--approval-mode").arg("yolo");
        cmd.env("QWEN_CODE_SUPPRESS_YOLO_WARNING", "1");
        // Alibaba ModelStudio Token Plan, configured from OUR .env: the plan key
        // (BAILIAN_TOKEN_PLAN_API_KEY — qwen's own env-key name for this plan)
        // is mapped onto the generic openai auth type headlessly, so no `/auth`
        // TUI setup is needed. The plan's dedicated endpoint + a plan model are
        // the defaults; CWI_QWEN_BASE_URL / CWI_QWEN_MODEL override.
        if let Ok(key) = std::env::var("BAILIAN_TOKEN_PLAN_API_KEY")
            && !key.trim().is_empty()
        {
            cmd.env("OPENAI_API_KEY", key.trim());
            cmd.env(
                "OPENAI_BASE_URL",
                std::env::var("CWI_QWEN_BASE_URL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| {
                        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"
                            .into()
                    }),
            );
            cmd.env(
                "OPENAI_MODEL",
                std::env::var("CWI_QWEN_MODEL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "qwen3.7-plus".into()),
            );
            cmd.arg("--auth-type").arg("openai");
        }
    } else {
        cmd.arg("--print")
            .arg("--verbose")
            .arg("--permission-mode")
            // Sandbox sessions bypass permission prompts: the disposable guest IS
            // the safety boundary, and `--tools` below removes every
            // host-touching tool.
            .arg(if config.sandbox {
                "bypassPermissions"
            } else {
                config.permission_mode.as_str()
            })
            // Without this, any tool needing a decision beyond `permission_mode`'s
            // own auto-approvals (Bash, WebFetch, AskUserQuestion, ...) silently
            // auto-denies in headless `--print` mode. This routes those decisions
            // to us instead, as `control_request`/`control_response` lines over
            // the same stdout/stdin — see `run_actor` in session.rs.
            .arg("--permission-prompt-tool")
            .arg("stdio");
    }

    // Sandbox mode: restrict the model to our `mcp__guest__*` tools so every
    // file/shell action runs inside the disposable executor VM over SSH — never
    // on the host. Removing the built-in Bash/Write/Read/Edit is what enforces
    // it; the subscription token stays on the host either way.
    if config.sandbox {
        // Per-chat guest workspace on the VM: point the mcp-guest child (which
        // inherits this env) at <base>/<session-id>, so each chat's files are
        // isolated on the VM and a download archives only this chat's work.
        cmd.env(
            "CWI_GUEST_WORKDIR",
            format!(
                "{}/{}",
                crate::mcp_guest::base_workdir().trim_end_matches('/'),
                id
            ),
        );
        if let Some(cfg) = sandbox_mcp_config() {
            cmd.arg("--mcp-config").arg(cfg).arg("--strict-mcp-config");
        }
        cmd.arg("--tools").args([
            "mcp__guest__bash",
            "mcp__guest__read_file",
            "mcp__guest__write_file",
        ]);
    }

    // System-prompt additions, appended in one flag. The image note applies to
    // every session; on a sandbox the host-privacy note (host cwd/OS/email are
    // injected by the host CLI and no flag strips them) is prepended — see
    // GUEST_PRIVACY_NOTE. Defense-in-depth: the neutral guest cwd (run-guest.ps1)
    // does the real work of keeping the owner's identity out of the path.
    let sys_note = if config.sandbox {
        format!("{GUEST_PRIVACY_NOTE}\n\n{IMAGE_NOTE}")
    } else {
        format!("{IMAGE_NOTE}\n\n{SCREENSHOT_NOTE}")
    };
    cmd.arg("--append-system-prompt").arg(&sys_note);

    if resume {
        cmd.arg("--resume").arg(&id);
    } else {
        cmd.arg("--session-id").arg(&id);
    }

    if let Some(model) = model
        && !model.is_empty()
        // Qwen: only pass a model that is actually a qwen model — the UI's
        // claude aliases (opus/sonnet/haiku) would 404 there; empty → its default.
        && (!qwen || model.to_ascii_lowercase().contains("qwen"))
    {
        cmd.arg("--model").arg(model);
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

/// Write (idempotently) the MCP config pointing Claude Code at our own
/// `mcp-guest` server — the stdio server whose tools run inside the executor
/// guest over SSH — and return its path. `None` if the exe path or the write
/// fails; the caller still passes `--tools` (restricting to the guest tools),
/// so a missing MCP config yields no tools rather than host access.
fn sandbox_mcp_config() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = crate::config::claude_config_dir().join("guest-mcp.json");
    let cfg = serde_json::json!({
        "mcpServers": {
            "guest": { "command": exe.to_string_lossy(), "args": ["mcp-guest"] }
        }
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&cfg).ok()?).ok()?;
    Some(path)
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
