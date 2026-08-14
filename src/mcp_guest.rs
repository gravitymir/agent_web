//! MCP stdio server exposing **sandboxed** tools that execute inside the executor
//! guest VM over SSH. Launched as `agent_web mcp-guest`.
//!
//! The host's `claude` CLI (subscription) is configured to use these tools
//! *instead of* its built-in Bash/Read/Write/Edit — so the model's reasoning runs
//! on the host (subscription token never leaves it) while its hands act only in
//! the disposable guest. This is the CLI counterpart to the API broker.
//!
//! Protocol: newline-delimited JSON-RPC 2.0 over stdio (the shape Claude Code's
//! MCP client speaks). STDOUT carries protocol frames ONLY — every diagnostic
//! goes to stderr, which the client drains to its log. Requests are handled
//! synchronously (one guest SSH call at a time), which is plenty for an agent
//! loop; a hung call only delays the next tool, and the guest is disposable.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::executor;

/// MCP protocol revision we speak. `2024-11-05` is widely supported by clients.
const PROTOCOL: &str = "2024-11-05";

/// Base guest workspace on the VM (`CWI_GUEST_WORKDIR`, default
/// `/home/insider/work`). Per-chat subdirs (`<base>/<session-id>`) live under it:
/// when spawning `claude`, the host sets the child mcp-guest's `CWI_GUEST_WORKDIR`
/// to the per-chat path — so this reads the *base* in the parent (host) process
/// and the per-chat dir inside the spawned mcp-guest.
pub fn base_workdir() -> String {
    std::env::var("CWI_GUEST_WORKDIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/home/insider/work".to_string())
}

/// Where the sandbox tools operate inside the guest. Relative tool paths resolve
/// against this — the per-chat dir in the spawned mcp-guest process.
fn workdir() -> String {
    base_workdir()
}

/// Single-quote a string for a POSIX shell (closes the quote, escapes any `'`,
/// reopens). Injection isn't a concern — the guest is the sandbox and the whole
/// point is running arbitrary commands there — but paths with spaces must survive.
pub fn sh(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Resolve a tool path: absolute paths as-is, relative against the workdir.
fn resolve(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", workdir().trim_end_matches('/'), path)
    }
}

fn text_result(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}
fn err_result(msg: &str) -> Value {
    text_result(msg, true)
}

/// One-line preview of a value for the audit log (newlines flattened, truncated).
fn preview(s: &str, n: usize) -> String {
    let flat = s.replace(['\n', '\r', '\t'], " ");
    if flat.chars().count() <= n {
        flat
    } else {
        format!("{}…", flat.chars().take(n).collect::<String>())
    }
}

/// Append an audit line for a guest tool call to a **host-side** log
/// (`<config_dir>/mcp_guest_audit.log`). Every sandbox action is recorded where
/// the guest can neither read nor tamper with it. Best-effort — never fails a call.
fn audit(tool: &str, detail: &str, exit: i32) {
    let path = crate::config::claude_config_dir().join("mcp_guest_audit.log");
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let line = format!("{ts}\t{tool}\texit={exit}\t{detail}\n");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Tool schemas advertised to the model. Descriptions stress that these act ONLY
/// in the sandbox, so the model reaches for them naturally.
fn tool_defs() -> Value {
    json!([
        {
            "name": "bash",
            "description": "Run a shell command inside the sandboxed guest VM (default working directory is the guest workspace). Returns combined stdout+stderr plus the exit code. Every shell/system action MUST go through this — it executes only in the disposable sandbox, never on the host.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to run in the guest." }
                },
                "required": ["command"]
            }
        },
        {
            "name": "read_file",
            "description": "Read a text file from the guest sandbox. Relative paths resolve against the guest workspace.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }
        },
        {
            "name": "write_file",
            "description": "Create or overwrite a text file in the guest sandbox (parent directories are created). Relative paths resolve against the guest workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }
        }
    ])
}

/// Execute one `tools/call` and return its MCP `result`.
fn handle_call(req: &Value) -> Value {
    let params = &req["params"];
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    if !executor::running() {
        return err_result("guest VM is not running — start the executor before using sandbox tools");
    }

    match name {
        "bash" => {
            let Some(command) = args.get("command").and_then(Value::as_str) else {
                return err_result("bash: 'command' is required");
            };
            let remote = format!("cd {} && ( {} )", sh(&workdir()), command);
            let (code, out, err) = executor::ssh_capture(&remote, None);
            audit("bash", &preview(command, 300), code);
            let mut body = out;
            if !err.is_empty() {
                if !body.is_empty() && !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(&err);
            }
            if code != 0 {
                if !body.is_empty() && !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(&format!("[exit code {code}]"));
            }
            if body.trim().is_empty() {
                body = "(no output)".to_string();
            }
            text_result(&body, code != 0)
        }
        "read_file" => {
            let Some(path) = args.get("path").and_then(Value::as_str) else {
                return err_result("read_file: 'path' is required");
            };
            let (code, out, err) = executor::ssh_capture(&format!("cat -- {}", sh(&resolve(path))), None);
            audit("read_file", &resolve(path), code);
            if code != 0 {
                return err_result(&format!("read_file failed: {}", err.trim()));
            }
            text_result(&out, false)
        }
        "write_file" => {
            let Some(path) = args.get("path").and_then(Value::as_str) else {
                return err_result("write_file: 'path' is required");
            };
            let content = args.get("content").and_then(Value::as_str).unwrap_or("");
            let p = resolve(path);
            let q = sh(&p);
            let remote = format!("mkdir -p \"$(dirname -- {q})\" && cat > {q}");
            let (code, _out, err) = executor::ssh_capture(&remote, Some(content.as_bytes()));
            audit("write_file", &format!("{p} ({} bytes)", content.len()), code);
            if code != 0 {
                return err_result(&format!("write_file failed: {}", err.trim()));
            }
            text_result(&format!("wrote {} bytes to {p}", content.len()), false)
        }
        other => err_result(&format!("unknown tool: {other}")),
    }
}

/// Run the stdio MCP server until stdin closes. Blocking; called from `main`'s
/// synchronous phase (no Tokio runtime needed).
pub fn run() {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // Best-effort: make sure the workspace exists (ignored if the VM is down).
    if executor::running() {
        let _ = executor::ssh_capture(&format!("mkdir -p {}", sh(&workdir())), None);
    }

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF — client closed the pipe
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(trimmed) else {
            continue; // ignore garbage lines
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");

        let result: Option<Value> = match method {
            "initialize" => Some(json!({
                "protocolVersion": PROTOCOL,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "guest-exec", "version": env!("CARGO_PKG_VERSION") }
            })),
            "tools/list" => Some(json!({ "tools": tool_defs() })),
            "tools/call" => Some(handle_call(&req)),
            "ping" => Some(json!({})),
            // Notifications (no id, e.g. notifications/initialized) get no reply;
            // an unknown *request* still gets an empty result so the client isn't
            // left waiting.
            _ => id.as_ref().map(|_| json!({})),
        };

        if let (Some(id), Some(result)) = (id, result) {
            let msg = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            if writeln!(out, "{msg}").is_err() || out.flush().is_err() {
                break;
            }
        }
    }
}
