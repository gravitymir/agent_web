//! Minimal MCP (Model Context Protocol) client over stdio.
//!
//! Reads a config (`CWI_MCP_CONFIG` or `~/.claude/cwi_mcp.json`) of the form
//! `{ "servers": { "name": { "command": "...", "args": [...], "env": {...} } } }`,
//! spawns each server, performs the `initialize` handshake, lists its tools, and
//! exposes them to the agent as `mcp__<server>__<tool>`. Tool calls are routed
//! back over JSON-RPC (`tools/call`). Missing/broken config → no servers (no-op).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout};
use tokio::sync::Mutex;

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

struct Io {
    stdin: tokio::process::ChildStdin,
    reader: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

struct Server {
    name: String,
    io: Mutex<Io>,
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

    /// Execute an `mcp__server__tool` call. Returns (content, is_error).
    pub async fn call(&self, name: &str, args: &Value) -> (String, bool) {
        let rest = name.strip_prefix("mcp__").unwrap_or(name);
        let Some((server_name, tool_name)) = rest.split_once("__") else {
            return (format!("bad MCP tool name: {name}"), true);
        };
        let Some(server) = self.servers.iter().find(|s| s.name == server_name) else {
            return (format!("MCP server not found: {server_name}"), true);
        };
        let mut io = server.io.lock().await;
        let id = {
            io.next_id += 1;
            io.next_id
        };
        let req = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": tool_name, "arguments": args }
        });
        match request(&mut io, id, req).await {
            Ok(resp) => {
                let result = &resp["result"];
                let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
                (extract_content(result), is_error)
            }
            Err(e) => (format!("MCP call failed: {e}"), true),
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
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
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
    let mut io = Io {
        stdin,
        reader: BufReader::new(stdout).lines(),
        next_id: 0,
    };

    // initialize
    let id = {
        io.next_id += 1;
        io.next_id
    };
    let init = json!({
        "jsonrpc": "2.0", "id": id, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "claude_web_interface", "version": "0.1" }
        }
    });
    request(&mut io, id, init).await?;
    notify(&mut io, json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await?;

    // tools/list
    let id = {
        io.next_id += 1;
        io.next_id
    };
    let list = request(&mut io, id, json!({"jsonrpc":"2.0","id":id,"method":"tools/list"})).await?;
    let tools = list["result"]["tools"].as_array().cloned().unwrap_or_default();

    Ok(Server {
        name: name.to_string(),
        io: Mutex::new(io),
        tools,
        _child: child,
    })
}

async fn request(io: &mut Io, id: u64, req: Value) -> Result<Value> {
    let mut line = req.to_string();
    line.push('\n');
    io.stdin.write_all(line.as_bytes()).await?;
    io.stdin.flush().await?;
    loop {
        let next = tokio::time::timeout(Duration::from_secs(60), io.reader.next_line()).await??;
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
        // otherwise a notification or unrelated id — ignore and keep reading.
    }
}

async fn notify(io: &mut Io, v: Value) -> Result<()> {
    let mut line = v.to_string();
    line.push('\n');
    io.stdin.write_all(line.as_bytes()).await?;
    io.stdin.flush().await?;
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
