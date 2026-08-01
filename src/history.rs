//! Reading Claude Code's on-disk session history.
//!
//! Each chat is a `<session-id>.jsonl` file under
//! `~/.claude/projects/<encoded-workspace>/`. Every line is a JSON event; we
//! tolerantly extract the user/assistant turns and ignore internal bookkeeping
//! events we don't render.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

/// Summary of one chat, for the sidebar list.
#[derive(Debug, Serialize)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    /// True when `title` was set manually by the user (not auto-derived).
    pub custom_title: bool,
    /// Optional icon (emoji glyph) shown before the title.
    pub icon: Option<String>,
    /// ISO-8601 timestamp of the last activity, if known.
    pub updated_at: Option<String>,
    pub message_count: usize,
}

/// One rendered turn in a chat transcript.
#[derive(Debug, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub text: String,
    pub timestamp: Option<String>,
}

/// List all chats in `session_dir`, most-recently-updated first.
pub fn list_chats(session_dir: &Path) -> Vec<ChatSummary> {
    let mut chats = Vec::new();

    let entries = match std::fs::read_dir(session_dir) {
        Ok(e) => e,
        Err(_) => return chats, // no sessions yet
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        if let Some(summary) = summarize_file(&path, id) {
            chats.push(summary);
        }
    }

    // Sort by timestamp desc; entries without a timestamp fall to the bottom.
    chats.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    chats
}

fn summarize_file(path: &Path, id: String) -> Option<ChatSummary> {
    let content = std::fs::read_to_string(path).ok()?;

    let mut title: Option<String> = None;
    let mut last_ts: Option<String> = None;
    let mut message_count = 0usize;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Prefer an explicit AI-generated title if present.
        if v.get("type").and_then(Value::as_str) == Some("summary") {
            if let Some(s) = v.get("summary").and_then(Value::as_str) {
                title = Some(truncate(s, 120));
            }
        }

        if let Some(ts) = v.get("timestamp").and_then(Value::as_str) {
            last_ts = Some(ts.to_string());
        }

        let ty = v.get("type").and_then(Value::as_str);
        if matches!(ty, Some("user") | Some("assistant")) {
            message_count += 1;
            if title.is_none() && ty == Some("user") {
                if let Some(text) = extract_text(&v) {
                    let text = text.trim();
                    if !text.is_empty() {
                        title = Some(truncate(text, 120));
                    }
                }
            }
        }
    }

    Some(ChatSummary {
        title: title.unwrap_or_else(|| format!("Chat {}", &id[..id.len().min(8)])),
        custom_title: false,
        icon: None,
        id,
        updated_at: last_ts,
        message_count,
    })
}

/// Load the full transcript of one chat as renderable turns.
pub fn load_chat(session_dir: &Path, id: &str) -> Vec<ChatMessage> {
    let path = session_dir.join(format!("{id}.jsonl"));
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut messages = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let role = match v.get("type").and_then(Value::as_str) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        if let Some(text) = extract_text(&v) {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            messages.push(ChatMessage {
                role: role.to_string(),
                text: text.to_string(),
                timestamp: v
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }
    messages
}

/// Pull renderable text out of a `user`/`assistant` event.
///
/// `message.content` is either a plain string or an array of content blocks;
/// we concatenate the `text` blocks and skip images/tool calls.
fn extract_text(v: &Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;

    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }

    if let Some(arr) = content.as_array() {
        let mut out = String::new();
        for block in arr {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        out.push_str(t);
                    }
                }
                Some("tool_use") => {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                    out.push_str(&format!("\n\n`⚙ {name}`\n"));
                }
                _ => {}
            }
        }
        if out.is_empty() {
            return None;
        }
        return Some(out);
    }

    None
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}
