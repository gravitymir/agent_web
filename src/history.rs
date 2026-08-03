//! Reading Claude Code's on-disk session history and native-engine stores.
//!
//! Each CLI chat is a `<session-id>.jsonl` file under
//! `~/.claude/projects/<encoded-workspace>/`. The native engine stores sessions
//! as `<id>.json` files under `~/.claude/cwi_native/`. Both backends are
//! surfaced in the same chat list.

use std::collections::HashSet;
use std::path::Path;
use std::time::UNIX_EPOCH;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::agent::store;

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
    /// Total output tokens (thinking + answer) spent across the chat.
    pub tokens: u64,
    /// Total input (prompt) tokens.
    pub input_tokens: u64,
    /// Tokens served from the prompt cache (cheaper).
    pub cache_read: u64,
    /// Tokens written to the prompt cache.
    pub cache_creation: u64,
    /// Number of assistant turns (model responses).
    pub turns: usize,
    /// Cumulative `result.duration_ms` across every turn — always 0 here (the
    /// on-disk transcript never carries it); `main.rs::list_chats` overlays
    /// the real value from `MetaStore`, the same way it overlays title/icon.
    /// Used for the Agentron effort metric.
    pub duration_ms: u64,
    /// Context-window fill as of the *last* completed API call (input +
    /// cache_read + cache_creation of the most recent `assistant` line) — not
    /// a sum across the chat. Drives the context-fill ring in the UI.
    pub last_context_tokens: u64,
    /// Which engine owns this chat: `"cli"` (Claude Code `.jsonl`) or `"native"`
    /// (`cwi_native/*.json`). The sidebar always lists both; a chat whose engine
    /// differs from the active `CWI_ENGINE` is shown read-only ("frozen").
    pub engine: &'static str,
}

/// A tool call made by the assistant, with its full arguments.
#[derive(Debug, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub input: Value,
}

/// One rendered turn in a chat transcript.
#[derive(Debug, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub text: String,
    pub timestamp: Option<String>,
    /// Tool calls in this message (assistant only), so history shows the same
    /// command/parameter cards as the live stream.
    pub tools: Vec<ToolCall>,
}

/// List all chats, most-recently-updated first. Combines CLI `.jsonl` sessions
/// and native-engine `.json` stores.
pub fn list_chats(session_dir: &Path, native_dir: Option<&Path>) -> Vec<ChatSummary> {
    let mut chats = list_jsonl_chats(session_dir);
    if let Some(dir) = native_dir {
        chats.extend(list_native_chats(dir));
    }

    // A session may theoretically exist in both forms; keep the CLI jsonl entry
    // and drop duplicates.
    let mut seen = HashSet::new();
    chats.retain(|c| seen.insert(c.id.clone()));

    // Sort by timestamp desc; entries without a timestamp fall to the bottom.
    chats.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    chats
}

fn list_jsonl_chats(session_dir: &Path) -> Vec<ChatSummary> {
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

        if let Some(summary) = summarize_jsonl_file(&path, id) {
            chats.push(summary);
        }
    }

    chats
}

fn list_native_chats(native_dir: &Path) -> Vec<ChatSummary> {
    let mut chats = Vec::new();

    let entries = match std::fs::read_dir(native_dir) {
        Ok(e) => e,
        Err(_) => return chats,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if let Some(summary) = summarize_native_file(&path, id) {
            chats.push(summary);
        }
    }

    chats
}

fn summarize_jsonl_file(path: &Path, id: String) -> Option<ChatSummary> {
    // Lossy decode so a chat with a bit of invalid UTF-8 still lists (partial
    // recovery) instead of silently vanishing from the sidebar.
    let content = std::fs::read(path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())?;

    let mut title: Option<String> = None;
    let mut last_ts: Option<String> = None;
    let mut message_count = 0usize;
    let mut tokens = 0u64;
    let mut input_tokens = 0u64;
    let mut cache_read = 0u64;
    let mut cache_creation = 0u64;
    let mut turns = 0usize;
    // Current context-window fill, not a running total: overwritten (not
    // summed) by each assistant line's own usage, so after the loop it holds
    // the *last* one — i.e. how large the conversation was on the most recent
    // API call, which only grows monotonically as the chat continues.
    let mut last_context_tokens = 0u64;

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
            if ty == Some("assistant") {
                turns += 1;
                if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                    let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    // output_tokens includes the thinking budget.
                    tokens += get("output_tokens");
                    input_tokens += get("input_tokens");
                    cache_read += get("cache_read_input_tokens");
                    cache_creation += get("cache_creation_input_tokens");
                    last_context_tokens =
                        get("input_tokens") + get("cache_read_input_tokens") + get("cache_creation_input_tokens");
                }
            }
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
        title: title.unwrap_or_else(|| format!("Chat {}", id.chars().take(8).collect::<String>())),
        custom_title: false,
        icon: None,
        id,
        updated_at: last_ts.or_else(|| file_modified_iso(path)),
        message_count,
        tokens,
        input_tokens,
        cache_read,
        cache_creation,
        turns,
        duration_ms: 0,
        last_context_tokens,
        engine: "cli",
    })
}

fn summarize_native_file(path: &Path, id: String) -> Option<ChatSummary> {
    let stored: store::Stored = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut message_count = 0usize;
    let mut turns = 0usize;

    for m in &stored.messages {
        let role = m.get("role").and_then(Value::as_str);
        if role == Some("user") || role == Some("assistant") {
            message_count += 1;
        }
        if role == Some("assistant") {
            turns += 1;
        }
    }

    let title = if stored.title.is_empty() {
        None
    } else {
        Some(stored.title.clone())
    };

    Some(ChatSummary {
        title: title.unwrap_or_else(|| format!("Chat {}", id.chars().take(8).collect::<String>())),
        custom_title: false,
        icon: None,
        id,
        updated_at: file_modified_iso(path),
        message_count,
        tokens: stored.output_tokens,
        input_tokens: 0,
        cache_read: 0,
        cache_creation: 0,
        turns,
        duration_ms: 0,
        last_context_tokens: 0,
        engine: "native",
    })
}

/// Load the full transcript of one chat as renderable turns.
pub fn load_chat(session_dir: &Path, native_dir: Option<&Path>, id: &str) -> Vec<ChatMessage> {
    let jsonl = session_dir.join(format!("{id}.jsonl"));
    if jsonl.exists() {
        return load_jsonl_chat(&jsonl);
    }
    if let Some(dir) = native_dir {
        let native = dir.join(format!("{id}.json"));
        if native.exists() {
            return load_native_chat(&native);
        }
    }
    Vec::new()
}

fn load_jsonl_chat(path: &Path) -> Vec<ChatMessage> {
    // Lossy decode so a bit of invalid UTF-8 doesn't wipe the whole transcript.
    let content = match std::fs::read(path) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
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
        // Tool-result messages are stored with role "user" but are internal to
        // the assistant's turn — skip them so they don't break answer grouping.
        if has_tool_result(&v) {
            continue;
        }
        let text = extract_text(&v).unwrap_or_default();
        let text = text.trim().to_string();
        let tools = extract_tools(&v);
        if text.is_empty() && tools.is_empty() {
            continue;
        }
        messages.push(ChatMessage {
            role: role.to_string(),
            text,
            timestamp: v
                .get("timestamp")
                .and_then(Value::as_str)
                .map(str::to_string),
            tools,
        });
    }
    messages
}

fn load_native_chat(path: &Path) -> Vec<ChatMessage> {
    let stored: store::Stored = match std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
    {
        Some(s) => s,
        None => return Vec::new(),
    };

    let mut messages = Vec::new();
    for m in stored.messages {
        let role = match m.get("role").and_then(Value::as_str) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        if role == "user" && is_native_tool_result(&m) {
            continue;
        }
        let text = extract_native_text(&m).unwrap_or_default().trim().to_string();
        let tools = extract_native_tools(&m);
        if text.is_empty() && tools.is_empty() {
            continue;
        }
        messages.push(ChatMessage {
            role: role.to_string(),
            text,
            timestamp: None,
            tools,
        });
    }
    messages
}

/// True if the message contains only `tool_result` content blocks (an internal
/// tool-output turn, not a real user message).
fn is_native_tool_result(v: &Value) -> bool {
    v.get("content")
        .and_then(Value::as_array)
        .map(|arr| !arr.is_empty() && arr.iter().all(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")))
        .unwrap_or(false)
}

/// True if the message contains a `tool_result` content block.
fn has_tool_result(v: &Value) -> bool {
    get_content(v)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
        .unwrap_or(false)
}

/// Collect the assistant's `tool_use` blocks (name + full input) from an event.
fn extract_tools(v: &Value) -> Vec<ToolCall> {
    let mut tools = Vec::new();
    if let Some(arr) = get_content(v).and_then(Value::as_array) {
        for block in arr {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                tools.push(ToolCall { name, input });
            }
        }
    }
    tools
}

/// Collect tool_use blocks from a native stored message.
fn extract_native_tools(v: &Value) -> Vec<ToolCall> {
    let mut tools = Vec::new();
    if let Some(arr) = v.get("content").and_then(Value::as_array) {
        for block in arr {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                tools.push(ToolCall { name, input });
            }
        }
    }
    tools
}

/// Pull renderable text out of a `user`/`assistant` event.
///
/// `message.content` is either a plain string or an array of content blocks;
/// we concatenate the `text` blocks and skip images/tool calls.
fn extract_text(v: &Value) -> Option<String> {
    let content = get_content(v)?;

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
                // tool_use blocks are surfaced separately (see extract_tools).
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

/// Pull renderable text out of a native stored message.
fn extract_native_text(v: &Value) -> Option<String> {
    let content = v.get("content")?;

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

/// Locate the content payload whether the event wraps it in `message.content`
/// (CLI stream-json) or exposes it directly as `content` (native store).
fn get_content(v: &Value) -> Option<&Value> {
    v.get("message")
        .and_then(|m| m.get("content"))
        .or_else(|| v.get("content"))
}

/// File modification time as an RFC 3339 string, used as a fallback `updated_at`
/// so brand-new empty chats still sort near the top.
fn file_modified_iso(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let secs = modified.duration_since(UNIX_EPOCH).ok()?.as_secs();
    DateTime::from_timestamp(secs as i64, 0).map(|dt: DateTime<Utc>| dt.to_rfc3339())
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

#[cfg(test)]
mod tests {
    use super::{extract_text, extract_tools, has_tool_result, summarize_jsonl_file};
    use serde_json::json;

    #[test]
    fn last_context_tokens_is_the_last_message_not_a_sum() {
        let path = std::env::temp_dir().join("cwi_history_test_last_context.jsonl");
        let lines = [
            json!({"type":"assistant","message":{"usage":{
                "input_tokens": 10, "output_tokens": 5,
                "cache_read_input_tokens": 0, "cache_creation_input_tokens": 100
            }}}),
            json!({"type":"assistant","message":{"usage":{
                "input_tokens": 2, "output_tokens": 8,
                "cache_read_input_tokens": 500, "cache_creation_input_tokens": 20
            }}}),
        ]
        .map(|v| v.to_string())
        .join("\n");
        std::fs::write(&path, lines).unwrap();

        let summary = summarize_jsonl_file(&path, "test-id".into()).expect("should parse");
        // Sums (existing behavior) cover both messages...
        assert_eq!(summary.input_tokens, 12);
        assert_eq!(summary.tokens, 13);
        // ...but the context gauge reflects only the most recent call's size.
        assert_eq!(summary.last_context_tokens, 2 + 500 + 20);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn extracts_string_content() {
        let v = json!({"message":{"content":"hello"}});
        assert_eq!(extract_text(&v).as_deref(), Some("hello"));
    }

    #[test]
    fn extracts_direct_content() {
        let v = json!({"role":"user","content":"hello"});
        assert_eq!(extract_text(&v).as_deref(), Some("hello"));
    }

    #[test]
    fn extracts_text_blocks_and_skips_tools() {
        let v = json!({"message":{"content":[
            {"type":"text","text":"a"},
            {"type":"tool_use","name":"X","input":{}}
        ]}});
        assert_eq!(extract_text(&v).as_deref(), Some("a"));
    }

    #[test]
    fn detects_tool_result() {
        let with = json!({"message":{"content":[{"type":"tool_result","tool_use_id":"t"}]}});
        let without = json!({"message":{"content":[{"type":"text","text":"x"}]}});
        assert!(has_tool_result(&with));
        assert!(!has_tool_result(&without));
    }

    #[test]
    fn extracts_tool_calls() {
        let v = json!({"message":{"content":[
            {"type":"tool_use","name":"Read","input":{"file_path":"a"}}
        ]}});
        let tools = extract_tools(&v);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "Read");
        assert_eq!(tools[0].input["file_path"], "a");
    }
}
