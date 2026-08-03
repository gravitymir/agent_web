//! The agent's tools: schemas advertised to the model and their execution.
//!
//! File tools are sandboxed to the workspace (paths that escape it are rejected).
//! `Bash` runs in the platform shell (PowerShell on Windows, `sh` elsewhere) with
//! a timeout, capturing stdout+stderr.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

/// Which tool groups the user has enabled (from the composer's tools/permissions
/// panel). Missing fields default to enabled, so an old client that sends no
/// `caps` keeps every tool. Groups map to tools in [`Caps::allows`].
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Caps {
    /// Read / Glob / Grep.
    pub read: bool,
    /// Write / Edit.
    pub modify: bool,
    /// Bash (run scripts / shell commands).
    pub run: bool,
    pub web_fetch: bool,
    pub web_search: bool,
}

impl Default for Caps {
    fn default() -> Self {
        Self { read: true, modify: true, run: true, web_fetch: true, web_search: true }
    }
}

impl Caps {
    /// Whether a built-in tool is currently allowed. Unknown names (e.g. `mcp__*`)
    /// are not gated here and return `true`.
    pub fn allows(&self, tool: &str) -> bool {
        match tool {
            "Read" | "Glob" | "Grep" => self.read,
            "Write" | "Edit" => self.modify,
            "Bash" => self.run,
            "WebFetch" => self.web_fetch,
            "WebSearch" => self.web_search,
            _ => true,
        }
    }
}

const BASH_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT: usize = 60_000; // cap tool output to keep the context sane
const READ_MAX_LINES: usize = 2000;

/// Result of running a tool.
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false }
    }
    fn err(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true }
    }
}

/// The JSON-Schema tool definitions sent to the model, filtered to the tool
/// groups the user has enabled (see [`Caps`]). A disabled tool isn't advertised,
/// so the model won't call it; execution is also guarded (defense in depth).
pub fn schemas(caps: &Caps) -> Vec<Value> {
    all_schemas()
        .into_iter()
        .filter(|t| t.get("name").and_then(Value::as_str).is_some_and(|n| caps.allows(n)))
        .collect()
}

/// Every built-in tool definition, unfiltered.
fn all_schemas() -> Vec<Value> {
    vec![
        json!({
            "name": "Bash",
            "description": "Run a shell command in the working directory and return its combined stdout/stderr. On Windows the shell is PowerShell.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run."},
                    "description": {"type": "string", "description": "5-10 word description of what it does."}
                },
                "required": ["command"]
            }
        }),
        json!({
            "name": "Read",
            "description": "Read a text file. Returns content with line numbers. Optional offset/limit select a line range.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "offset": {"type": "integer", "description": "1-based first line to read."},
                    "limit": {"type": "integer", "description": "Max number of lines."}
                },
                "required": ["file_path"]
            }
        }),
        json!({
            "name": "Write",
            "description": "Create or overwrite a file with the given content.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["file_path", "content"]
            }
        }),
        json!({
            "name": "Edit",
            "description": "Replace an exact string in a file. old_string must be unique unless replace_all is true.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["file_path", "old_string", "new_string"]
            }
        }),
        json!({
            "name": "Glob",
            "description": "Find files matching a glob pattern (e.g. **/*.rs), newest first.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Directory to search in (default: workspace root)."}
                },
                "required": ["pattern"]
            }
        }),
        json!({
            "name": "Grep",
            "description": "Search file contents with a regular expression. Returns file:line:text matches.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "File or directory to search (default: workspace)."},
                    "glob": {"type": "string", "description": "Only search files matching this glob (e.g. *.rs)."}
                },
                "required": ["pattern"]
            }
        }),
        json!({
            "name": "WebFetch",
            "description": "Fetch a URL over HTTP(S) and return its text content (HTML is stripped to plain text).",
            "input_schema": {
                "type": "object",
                "properties": { "url": {"type": "string"} },
                "required": ["url"]
            }
        }),
        json!({
            "name": "WebSearch",
            "description": "Search the web and return the top results (title, url, snippet).",
            "input_schema": {
                "type": "object",
                "properties": { "query": {"type": "string"} },
                "required": ["query"]
            }
        }),
    ]
}

pub async fn execute(name: &str, input: &Value, workspace: &Path) -> ToolOutput {
    match name {
        "Bash" => bash(input, workspace).await,
        "Read" => read(input, workspace),
        "Write" => write(input, workspace),
        "Edit" => edit(input, workspace),
        "Glob" => glob_tool(input, workspace),
        "Grep" => grep(input, workspace),
        "WebFetch" => web_fetch(input).await,
        "WebSearch" => web_search(input).await,
        other => ToolOutput::err(format!("Unknown tool: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Path sandbox
// ---------------------------------------------------------------------------

/// Lexically normalize a path (resolve `.`/`..`) without touching the filesystem
/// so it also works for files that don't exist yet.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve a (possibly relative) path against the workspace and reject anything
/// that escapes it — lexically, and (for existing paths) after resolving
/// symlinks so a symlink inside the workspace can't point outside it.
fn resolve(workspace: &Path, p: &str) -> Result<PathBuf, String> {
    let raw = Path::new(p);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        workspace.join(raw)
    };
    let abs = normalize(&joined);
    let ws = normalize(workspace);
    if !abs.starts_with(&ws) {
        return Err(format!("Path escapes the workspace: {p}"));
    }
    // Symlink guard: canonicalize the deepest EXISTING ancestor and check it is
    // still under the canonical workspace. (New files: only ancestors exist.)
    if let Ok(canon_ws) = std::fs::canonicalize(&ws) {
        let mut check: &Path = &abs;
        loop {
            match std::fs::canonicalize(check) {
                Ok(real) => {
                    if !real.starts_with(&canon_ws) {
                        return Err(format!("Path escapes the workspace via symlink: {p}"));
                    }
                    break;
                }
                Err(_) => match check.parent() {
                    Some(parent) => check = parent,
                    None => break,
                },
            }
        }
    }
    Ok(abs)
}

fn str_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_OUTPUT {
        // `String::truncate` panics if the byte index isn't a char boundary, so
        // back up to the nearest one — a multibyte char (Cyrillic, emoji, …) can
        // straddle `MAX_OUTPUT` in real non-ASCII tool output.
        let mut end = MAX_OUTPUT;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str("\n… [output truncated]");
    }
    s
}

#[cfg(test)]
mod truncate_tests {
    use super::{truncate, MAX_OUTPUT};

    #[test]
    fn does_not_panic_on_multibyte_boundary() {
        // Make a 2-byte 'я' straddle the MAX_OUTPUT cut point: byte MAX_OUTPUT
        // lands inside the char, which would panic a naive `String::truncate`.
        let mut s = "a".repeat(MAX_OUTPUT - 1);
        s.push('я');
        assert!(s.len() > MAX_OUTPUT);
        assert!(!s.is_char_boundary(MAX_OUTPUT)); // precondition: mid-char cut
        let out = truncate(s); // must not panic
        assert!(out.ends_with("[output truncated]"));
    }

    #[test]
    fn leaves_short_output_untouched() {
        let s = "hello".to_string();
        assert_eq!(truncate(s), "hello");
    }
}

// ---------------------------------------------------------------------------
// Bash / shell
// ---------------------------------------------------------------------------

async fn bash(input: &Value, workspace: &Path) -> ToolOutput {
    let command = match str_field(input, "command") {
        Some(c) => c,
        None => return ToolOutput::err("Bash: missing 'command'"),
    };

    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("powershell");
        c.arg("-NoProfile").arg("-NonInteractive").arg("-Command").arg(command);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("Bash: failed to spawn: {e}")),
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let run = async {
        // Read BOTH pipes concurrently: draining stdout to EOF before touching
        // stderr deadlocks once the child fills the stderr pipe buffer (verbose
        // compilers etc.). Each stream is capped as it's read, so a runaway
        // command can't exhaust memory — the excess is drained and discarded so
        // the child never blocks on a full pipe.
        let out_fut = async {
            match stdout {
                Some(s) => read_capped(s, MAX_OUTPUT).await,
                None => (String::new(), false),
            }
        };
        let err_fut = async {
            match stderr {
                Some(s) => read_capped(s, MAX_OUTPUT).await,
                None => (String::new(), false),
            }
        };
        let (out, err) = tokio::join!(out_fut, err_fut);
        (out, err, child.wait().await)
    };

    match tokio::time::timeout(BASH_TIMEOUT, run).await {
        Ok(((out, out_trunc), (err, err_trunc), Ok(status))) => {
            let mut combined = String::new();
            if !out.is_empty() {
                combined.push_str(&out);
            }
            if !err.is_empty() {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str(&err);
            }
            if out_trunc || err_trunc {
                combined.push_str("\n…(output truncated)");
            }
            if combined.trim().is_empty() {
                combined = format!("(no output, exit code {})", status.code().unwrap_or(-1));
            }
            let is_error = !status.success();
            ToolOutput { content: truncate(combined), is_error }
        }
        Ok((_, _, Err(e))) => ToolOutput::err(format!("Bash: {e}")),
        Err(_) => ToolOutput::err(format!("Bash: timed out after {}s", BASH_TIMEOUT.as_secs())),
    }
}

/// Read `reader` to EOF but keep only the first `cap` bytes; the rest is drained
/// and discarded so the child never blocks on a full pipe. Returns the captured
/// text and whether it was truncated.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut reader: R, cap: usize) -> (String, bool) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = (cap - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
        }
    }
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

fn read(input: &Value, workspace: &Path) -> ToolOutput {
    let path = match str_field(input, "file_path") {
        Some(p) => p,
        None => return ToolOutput::err("Read: missing 'file_path'"),
    };
    let abs = match resolve(workspace, path) {
        Ok(a) => a,
        Err(e) => return ToolOutput::err(e),
    };
    let content = match std::fs::read_to_string(&abs) {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("Read: {e}")),
    };
    let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(READ_MAX_LINES);

    let mut out = String::new();
    for (i, line) in content.lines().enumerate().skip(offset - 1).take(limit) {
        out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
    }
    if out.is_empty() {
        out = "(file is empty or the range is out of bounds)".to_string();
    }
    ToolOutput::ok(truncate(out))
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

fn write(input: &Value, workspace: &Path) -> ToolOutput {
    let path = match str_field(input, "file_path") {
        Some(p) => p,
        None => return ToolOutput::err("Write: missing 'file_path'"),
    };
    let content = str_field(input, "content").unwrap_or("");
    let abs = match resolve(workspace, path) {
        Ok(a) => a,
        Err(e) => return ToolOutput::err(e),
    };
    if let Some(parent) = abs.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&abs, content) {
        Ok(()) => ToolOutput::ok(format!(
            "Wrote {} lines to {}",
            content.lines().count(),
            path
        )),
        Err(e) => ToolOutput::err(format!("Write: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Edit
// ---------------------------------------------------------------------------

fn edit(input: &Value, workspace: &Path) -> ToolOutput {
    let path = match str_field(input, "file_path") {
        Some(p) => p,
        None => return ToolOutput::err("Edit: missing 'file_path'"),
    };
    let old = match str_field(input, "old_string") {
        Some(s) => s,
        None => return ToolOutput::err("Edit: missing 'old_string'"),
    };
    let new = str_field(input, "new_string").unwrap_or("");
    let replace_all = input.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
    let abs = match resolve(workspace, path) {
        Ok(a) => a,
        Err(e) => return ToolOutput::err(e),
    };
    let content = match std::fs::read_to_string(&abs) {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("Edit: {e}")),
    };
    let count = content.matches(old).count();
    if count == 0 {
        return ToolOutput::err("Edit: old_string not found in file");
    }
    if count > 1 && !replace_all {
        return ToolOutput::err(format!(
            "Edit: old_string is not unique ({count} matches). Provide a larger unique snippet or set replace_all."
        ));
    }
    let updated = if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };
    match std::fs::write(&abs, updated) {
        Ok(()) => ToolOutput::ok(format!(
            "Edited {} ({} replacement{})",
            path,
            if replace_all { count } else { 1 },
            if replace_all && count != 1 { "s" } else { "" }
        )),
        Err(e) => ToolOutput::err(format!("Edit: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Glob
// ---------------------------------------------------------------------------

fn glob_tool(input: &Value, workspace: &Path) -> ToolOutput {
    let pattern = match str_field(input, "pattern") {
        Some(p) => p,
        None => return ToolOutput::err("Glob: missing 'pattern'"),
    };
    let base = match str_field(input, "path") {
        Some(p) => match resolve(workspace, p) {
            Ok(a) => a,
            Err(e) => return ToolOutput::err(e),
        },
        None => workspace.to_path_buf(),
    };
    let full = base.join(pattern);
    let full = full.to_string_lossy().to_string();

    let ws_norm = normalize(workspace);
    let mut matches: Vec<(std::time::SystemTime, String)> = Vec::new();
    match glob::glob(&full) {
        Ok(paths) => {
            for entry in paths.flatten() {
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                // Show workspace-relative paths (also drops the Windows \\?\ prefix).
                let rel = entry.strip_prefix(&ws_norm).unwrap_or(&entry);
                let disp = rel.to_string_lossy().trim_start_matches(r"\\?\").to_string();
                matches.push((mtime, disp));
            }
        }
        Err(e) => return ToolOutput::err(format!("Glob: invalid pattern: {e}")),
    }
    matches.sort_by(|a, b| b.0.cmp(&a.0));
    let list: Vec<String> = matches.into_iter().take(300).map(|(_, p)| p).collect();
    if list.is_empty() {
        ToolOutput::ok("(no files matched)")
    } else {
        ToolOutput::ok(truncate(list.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// Grep
// ---------------------------------------------------------------------------

fn grep(input: &Value, workspace: &Path) -> ToolOutput {
    let pattern = match str_field(input, "pattern") {
        Some(p) => p,
        None => return ToolOutput::err("Grep: missing 'pattern'"),
    };
    let re = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return ToolOutput::err(format!("Grep: invalid regex: {e}")),
    };
    let root = match str_field(input, "path") {
        Some(p) => match resolve(workspace, p) {
            Ok(a) => a,
            Err(e) => return ToolOutput::err(e),
        },
        None => workspace.to_path_buf(),
    };
    let glob_filter = str_field(input, "glob").and_then(|g| glob::Pattern::new(g).ok());

    let mut hits: Vec<String> = Vec::new();
    let mut count = 0usize;
    for entry in walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        // Skip noisy directories.
        let path = entry.path();
        if path.components().any(|c| {
            matches!(c.as_os_str().to_str(), Some(".git") | Some("target") | Some("node_modules"))
        }) {
            continue;
        }
        if let Some(g) = &glob_filter {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !g.matches(name) {
                continue;
            }
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue, // binary / unreadable
        };
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                let rel = path.strip_prefix(&root).unwrap_or(path);
                hits.push(format!("{}:{}:{}", rel.display(), i + 1, line.trim_end()));
                count += 1;
                if count >= 500 {
                    break;
                }
            }
        }
        if count >= 500 {
            hits.push("… [more matches omitted]".to_string());
            break;
        }
    }
    if hits.is_empty() {
        ToolOutput::ok("(no matches)")
    } else {
        ToolOutput::ok(truncate(hits.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// WebFetch / WebSearch
// ---------------------------------------------------------------------------

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Very small HTML→text: drop script/style, strip tags, decode a few entities,
/// collapse whitespace.
fn html_to_text(html: &str) -> String {
    let drop = regex::Regex::new(r"(?is)<(script|style)[^>]*>.*?</\s*\1\s*>").unwrap();
    let no_scripts = drop.replace_all(html, " ");
    let tag = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    let text = tag.replace_all(&no_scripts, " ");
    let text = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let ws = regex::Regex::new(r"[ \t\x0b\x0c\r]+").unwrap();
    let text = ws.replace_all(&text, " ");
    let nl = regex::Regex::new(r"\n\s*\n\s*\n+").unwrap();
    nl.replace_all(text.trim(), "\n\n").to_string()
}

/// Block obvious SSRF targets: loopback, private, link-local, and internal
/// hostnames. (Not exhaustive — a public name resolving to a private IP via DNS
/// rebinding is not caught here.)
fn is_blocked_host(url: &str) -> bool {
    let host = match reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
    {
        Some(h) => h.to_lowercase(),
        None => return true,
    };
    if host == "localhost" || host.ends_with(".local") || host.ends_with(".internal") {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00
            }
        };
    }
    false
}

async fn web_fetch(input: &Value) -> ToolOutput {
    let url = match str_field(input, "url") {
        Some(u) => u,
        None => return ToolOutput::err("WebFetch: missing 'url'"),
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return ToolOutput::err("WebFetch: url must start with http:// or https://");
    }
    if is_blocked_host(url) {
        return ToolOutput::err("WebFetch: blocked host (loopback/private/internal)");
    }
    let client = match reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; cwi-agent/0.1)")
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("WebFetch: {e}")),
    };
    match client.get(url).send().await {
        Ok(resp) => {
            let status = resp.status();
            let ctype = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let body = resp.text().await.unwrap_or_default();
            let text = if ctype.contains("html") {
                html_to_text(&body)
            } else {
                body
            };
            ToolOutput {
                content: truncate(format!("HTTP {} · {}\n\n{}", status.as_u16(), url, text)),
                is_error: !status.is_success(),
            }
        }
        Err(e) => ToolOutput::err(format!("WebFetch: {e}")),
    }
}

async fn web_search(input: &Value) -> ToolOutput {
    let query = match str_field(input, "query") {
        Some(q) => q,
        None => return ToolOutput::err("WebSearch: missing 'query'"),
    };
    let client = match reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; cwi-agent/0.1)")
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("WebSearch: {e}")),
    };
    let url = format!("https://html.duckduckgo.com/html/?q={}", url_encode(query));
    let html = match client.get(&url).send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(e) => return ToolOutput::err(format!("WebSearch: {e}")),
    };

    // Parse DuckDuckGo's HTML result list (best-effort).
    let link_re =
        regex::Regex::new(r#"(?s)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap();
    let snip_re =
        regex::Regex::new(r#"(?s)class="result__snippet"[^>]*>(.*?)</a>"#).unwrap();
    let snippets: Vec<String> = snip_re
        .captures_iter(&html)
        .map(|c| html_to_text(&c[1]))
        .collect();

    let mut out = String::new();
    for (i, cap) in link_re.captures_iter(&html).enumerate().take(8) {
        let raw = &cap[1];
        // DDG links are redirects: extract the uddg= target if present.
        let target = raw
            .split("uddg=")
            .nth(1)
            .map(|s| decode_component(s.split('&').next().unwrap_or(s)))
            .unwrap_or_else(|| raw.to_string());
        let title = html_to_text(&cap[2]);
        let snip = snippets.get(i).cloned().unwrap_or_default();
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, title, target));
        if !snip.is_empty() {
            out.push_str(&format!("   {snip}\n"));
        }
    }
    if out.is_empty() {
        ToolOutput::ok("(no results)")
    } else {
        ToolOutput::ok(truncate(out))
    }
}

/// Minimal percent-decoding for the URL captured from DuckDuckGo redirects.
fn decode_component(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::{normalize, resolve, schemas, Caps};
    use std::path::{Path, PathBuf};

    #[test]
    fn caps_default_enables_everything() {
        let c = Caps::default();
        for t in ["Read", "Glob", "Grep", "Write", "Edit", "Bash", "WebFetch", "WebSearch"] {
            assert!(c.allows(t), "{t} should be allowed by default");
        }
        assert!(c.allows("mcp__srv__tool")); // MCP tools are never gated here
    }

    #[test]
    fn caps_gate_groups() {
        let mut c = Caps::default();
        c.modify = false;
        c.run = false;
        assert!(!c.allows("Write"));
        assert!(!c.allows("Edit"));
        assert!(!c.allows("Bash"));
        assert!(c.allows("Read")); // read group still on
        assert!(c.allows("mcp__x__y"));
    }

    #[test]
    fn schemas_filtered_by_caps() {
        let mut c = Caps::default();
        c.run = false;
        c.web_search = false;
        let names: Vec<String> = schemas(&c)
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        assert!(!names.iter().any(|n| n == "Bash"));
        assert!(!names.iter().any(|n| n == "WebSearch"));
        assert!(names.iter().any(|n| n == "Read"));
        assert!(names.iter().any(|n| n == "WebFetch"));
    }

    #[test]
    fn normalize_resolves_dotdot() {
        assert_eq!(normalize(Path::new("a/b/../c")), PathBuf::from("a/c"));
        assert_eq!(normalize(Path::new("a/./b")), PathBuf::from("a/b"));
    }

    #[test]
    fn resolve_stays_in_workspace() {
        let ws = Path::new("work");
        assert!(resolve(ws, "src/main.rs").is_ok());
        assert!(resolve(ws, "./a/b.txt").is_ok());
    }

    #[test]
    fn resolve_rejects_escape() {
        let ws = Path::new("work");
        assert!(resolve(ws, "../secret").is_err());
        assert!(resolve(ws, "a/../../etc").is_err());
    }
}
