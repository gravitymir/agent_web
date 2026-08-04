//! Minimal MCP (Model Context Protocol) client over stdio.
//!
//! Reads a config (`CWI_MCP_CONFIG` or `~/.claude/cwi_mcp.json`) of the form
//! `{ "servers": { "name": { "command": "...", "args": [...], "env": {...} } } }`,
//! spawns each server, performs the `initialize` handshake, lists its tools, and
//! exposes them to the agent as `mcp__<server>__<tool>`. Tool calls are routed
//! back over JSON-RPC (`tools/call`). Missing/broken config → no servers (no-op).
//!
//! Concurrency: after the handshake, a per-server background task reads every
//! line from stdout and dispatches responses to the waiting call by JSON-RPC
//! `id` (a `pending` map of `id -> oneshot`). Calls therefore run **pipelined** —
//! a slow/hung call only times out its own future and never head-of-line blocks
//! other calls to the same server (the previous `Mutex<Io>` serialized them).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{oneshot, Mutex};

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
struct ServerCfg {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default)]
    servers: HashMap<String, ServerCfg>,
}

/// Response is `Ok(full json-rpc message)` or `Err(error text)`.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct Server {
    name: String,
    stdin: Mutex<ChildStdin>, // locked only briefly to write one request line
    pending: Pending,
    next_id: AtomicU64,
    tools: Vec<Value>, // raw MCP tool defs: {name, description, inputSchema}
    _child: Child,
}

pub struct McpClient {
    servers: Vec<Server>,
}

fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("CWI_MCP_CONFIG") {
        return PathBuf::from(p);
    }
    crate::config::claude_config_dir().join("cwi_mcp.json")
}

impl McpClient {
    /// Spawn and initialize every configured server. Never fails — servers that
    /// can't start are logged and skipped.
    pub async fn init() -> Self {
        let cfg: Config = std::fs::read_to_string(config_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Config { servers: HashMap::new() });

        let mut servers = Vec::new();
        for (name, sc) in cfg.servers {
            match spawn_server(&name, &sc).await {
                Ok(s) => {
                    tracing::info!("mcp: server '{}' up with {} tools", s.name, s.tools.len());
                    servers.push(s);
                }
                Err(e) => tracing::warn!("mcp: server '{name}' failed: {e}"),
            }
        }
        McpClient { servers }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Tool schemas for the model, names prefixed `mcp__<server>__<tool>`.
    pub fn tool_schemas(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for s in &self.servers {
            for t in &s.tools {
                let tname = t.get("name").and_then(Value::as_str).unwrap_or("");
                out.push(json!({
                    "name": format!("mcp__{}__{}", s.name, tname),
                    "description": t.get("description").cloned().unwrap_or_else(|| json!("")),
                    "input_schema": t.get("inputSchema").cloned().unwrap_or_else(|| json!({"type":"object"})),
                }));
            }
        }
        out
    }

    /// Execute an `mcp__server__tool` call. Returns (content, is_error). Runs
    /// pipelined: registers a `pending` waiter, writes the request, and awaits
    /// only its own response — never blocked by another in-flight call.
    pub async fn call(&self, name: &str, args: &Value) -> (String, bool) {
        let rest = name.strip_prefix("mcp__").unwrap_or(name);
        let Some((server_name, tool_name)) = rest.split_once("__") else {
            return (format!("bad MCP tool name: {name}"), true);
        };
        let Some(server) = self.servers.iter().find(|s| s.name == server_name) else {
            return (format!("MCP server not found: {server_name}"), true);
        };

        let id = server.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        server.pending.lock().await.insert(id, tx);

        let req = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": tool_name, "arguments": args }
        });
        if let Err(e) = write_line(&server.stdin, &req).await {
            server.pending.lock().await.remove(&id);
            return (format!("MCP call failed: {e}"), true);
        }

        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(Ok(resp))) => {
                let result = &resp["result"];
                let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
                (extract_content(result), is_error)
            }
            Ok(Ok(Err(e))) => (format!("MCP call failed: {e}"), true),
            Ok(Err(_)) => ("MCP call failed: response channel closed".to_string(), true),
            Err(_) => {
                server.pending.lock().await.remove(&id); // don't leak the waiter
                ("MCP call timed out after 60s".to_string(), true)
            }
        }
    }
}

async fn spawn_server(name: &str, sc: &ServerCfg) -> Result<Server> {
    let mut cmd = tokio::process::Command::new(&sc.command);
    cmd.args(&sc.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &sc.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

    // Drain stderr to the log so server crashes / config complaints aren't lost.
    if let Some(stderr) = child.stderr.take() {
        let server = name.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    tracing::warn!(mcp_server = %server, "mcp stderr: {line}");
                }
            }
        });
    }

    let mut reader = BufReader::new(stdout).lines();

    // Handshake synchronously while we still own the reader exclusively (ids 1, 2).
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "agent_web", "version": "0.1" }
        }
    });
    init_request(&mut stdin, &mut reader, 1, init).await?;
    write_line_raw(&mut stdin, &json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await?;
    let list = init_request(
        &mut stdin,
        &mut reader,
        2,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await?;
    let tools = list["result"]["tools"].as_array().cloned().unwrap_or_default();

    // Hand the reader to a background demux task so subsequent calls pipeline.
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    tokio::spawn(reader_loop(name.to_string(), reader, pending.clone()));

    Ok(Server {
        name: name.to_string(),
        stdin: Mutex::new(stdin),
        pending,
        next_id: AtomicU64::new(2), // ids 1 and 2 were used by the handshake
        tools,
        _child: child,
    })
}

/// Background task: read every line from the server and route each response to
/// the call waiting on its `id`. When the stream closes, fail all pending calls
/// so none hang.
async fn reader_loop(name: String, mut reader: Lines<BufReader<ChildStdout>>, pending: Pending) {
    while let Ok(Some(l)) = reader.next_line().await {
        let l = l.trim();
        if l.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        let Some(id) = v.get("id").and_then(Value::as_u64) else {
            continue; // notification (no id) — nothing to route
        };
        let waiter = pending.lock().await.remove(&id);
        if let Some(tx) = waiter {
            let msg = if v.get("error").is_some() {
                Err(v["error"].to_string())
            } else {
                Ok(v)
            };
            let _ = tx.send(msg);
        }
    }
    let mut p = pending.lock().await;
    for (_, tx) in p.drain() {
        let _ = tx.send(Err("MCP server closed the stream".to_string()));
    }
    tracing::warn!(mcp_server = %name, "mcp: reader stopped");
}

/// A request/response used only during the handshake, where we own the reader.
async fn init_request(
    stdin: &mut ChildStdin,
    reader: &mut Lines<BufReader<ChildStdout>>,
    id: u64,
    req: Value,
) -> Result<Value> {
    write_line_raw(stdin, &req).await?;
    loop {
        let next = tokio::time::timeout(CALL_TIMEOUT, reader.next_line()).await??;
        let Some(l) = next else {
            return Err(anyhow!("server closed the stream"));
        };
        if l.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&l) else {
            continue;
        };
        if v.get("id").and_then(Value::as_u64) == Some(id) {
            if let Some(err) = v.get("error") {
                return Err(anyhow!("{}", err));
            }
            return Ok(v);
        }
    }
}

async fn write_line_raw(stdin: &mut ChildStdin, v: &Value) -> Result<()> {
    let mut line = v.to_string();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn write_line(stdin: &Mutex<ChildStdin>, v: &Value) -> Result<()> {
    let mut line = v.to_string();
    line.push('\n');
    let mut w = stdin.lock().await;
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

/// Flatten an MCP `result.content` array (text blocks) into a string.
fn extract_content(result: &Value) -> String {
    if let Some(arr) = result.get("content").and_then(Value::as_array) {
        let mut out = String::new();
        for block in arr {
            if let Some(t) = block.get("text").and_then(Value::as_str) {
                out.push_str(t);
                out.push('\n');
            }
        }
        if !out.is_empty() {
            return out.trim_end().to_string();
        }
    }
    result.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // Tiny stdio MCP servers: `echo` replies instantly; `sleep` replies after
    // `ms` (async, so the server can answer `echo` while `sleep` is pending).
    // Used to prove a slow call doesn't head-of-line block a fast one.
    const MOCK_JS: &str = r#"
const rl = require('readline').createInterface({ input: process.stdin });
rl.on('line', (line) => {
  line = (line || '').trim();
  if (!line) return;
  let m; try { m = JSON.parse(line); } catch { return; }
  if (m.id === undefined || m.id === null) return;
  const send = (result) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, result }) + "\n");
  if (m.method === 'initialize') return send({ protocolVersion: "2024-11-05", capabilities: {} });
  if (m.method === 'tools/list') return send({ tools: [
    { name: "echo", inputSchema: { type: "object" } },
    { name: "sleep", inputSchema: { type: "object" } }
  ]});
  if (m.method === 'tools/call') {
    const p = m.params || {}; const a = p.arguments || {};
    if (p.name === 'sleep') { setTimeout(() => send({ content: [{ type: "text", text: "slept" }] }), a.ms || 500); return; }
    return send({ content: [{ type: "text", text: "echo:" + (a.text || "") }] });
  }
  send({});
});
"#;

    const MOCK_PY: &str = r#"
import sys, json, threading
lock = threading.Lock()
def send(i, result):
    with lock:
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":i,"result":result}) + "\n")
        sys.stdout.flush()
while True:
    line = sys.stdin.readline()
    if not line: break
    line = line.strip()
    if not line: continue
    try: m = json.loads(line)
    except Exception: continue
    if m.get("id") is None: continue
    meth = m.get("method")
    if meth == "initialize":
        send(m["id"], {"protocolVersion":"2024-11-05","capabilities":{}})
    elif meth == "tools/list":
        send(m["id"], {"tools":[{"name":"echo","inputSchema":{"type":"object"}},{"name":"sleep","inputSchema":{"type":"object"}}]})
    elif meth == "tools/call":
        p = m.get("params",{}); a = p.get("arguments",{}); n = p.get("name")
        if n == "sleep":
            ms = a.get("ms",500)
            threading.Timer(ms/1000.0, send, args=(m["id"], {"content":[{"type":"text","text":"slept"}]})).start()
        else:
            send(m["id"], {"content":[{"type":"text","text":"echo:"+str(a.get("text",""))}]})
    else:
        send(m["id"], {})
"#;

    #[tokio::test]
    async fn concurrent_calls_are_not_head_of_line_blocked() {
        // Use whichever runtime is available (MCP servers are usually node; python
        // is a common fallback). Skip cleanly if neither is present.
        let candidates: Vec<(&str, Vec<String>)> = vec![
            ("node", vec!["-e".into(), MOCK_JS.into()]),
            ("python", vec!["-c".into(), MOCK_PY.into()]),
            ("python3", vec!["-c".into(), MOCK_PY.into()]),
        ];
        let mut server = None;
        for (cmd, args) in candidates {
            let sc = ServerCfg { command: cmd.into(), args, env: HashMap::new() };
            if let Ok(s) = spawn_server("mock", &sc).await {
                server = Some(s);
                break;
            }
        }
        let Some(server) = server else {
            eprintln!("skipping MCP pipelining test: no node/python available");
            return;
        };
        let client = Arc::new(McpClient { servers: vec![server] });

        // Start a slow (1.5s) call in the background.
        let bg = client.clone();
        let slow = tokio::spawn(async move { bg.call("mcp__mock__sleep", &json!({ "ms": 1500 })).await });
        tokio::time::sleep(Duration::from_millis(100)).await; // let it get in-flight

        // A fast call must return promptly rather than wait for the slow one.
        let t = Instant::now();
        let (out, is_err) = client.call("mcp__mock__echo", &json!({ "text": "hi" })).await;
        let elapsed = t.elapsed();

        assert!(!is_err, "fast call errored: {out}");
        assert!(out.contains("hi"), "unexpected echo output: {out}");
        assert!(
            elapsed < Duration::from_millis(800),
            "fast call was head-of-line blocked by the slow one ({elapsed:?})"
        );
        assert!(!slow.is_finished(), "slow call should still be running");

        let _ = slow.await;
    }
}
