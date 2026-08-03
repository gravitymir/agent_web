//! Session storage for the native engine — our own format, independent of
//! Claude Code's `.jsonl`. Each session is the raw Anthropic `messages` array
//! plus lightweight metadata, saved as one JSON file under
//! `~/.claude/cwi_native/<id>.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Stored {
    /// The Anthropic `messages` array (role/content objects) driving the chat.
    #[serde(default)]
    pub messages: Vec<Value>,
    /// First user prompt, used as a fallback title.
    #[serde(default)]
    pub title: String,
    /// Provider/model that produced the session (for display).
    #[serde(default)]
    pub model: String,
    /// Cumulative output tokens across the whole chat.
    #[serde(default)]
    pub output_tokens: u64,
    /// Estimated thinking (reasoning) tokens, split from output by text volume.
    #[serde(default)]
    pub thinking_tokens: u64,
    /// Estimated answer tokens (output minus thinking).
    #[serde(default)]
    pub answer_tokens: u64,
    /// Cumulative input (prompt) tokens across all model calls.
    #[serde(default)]
    pub input_tokens: u64,
    /// Cumulative prompt-cache read / creation tokens (0 for providers without caching).
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_creation: u64,
    /// Context-window fill on the MOST RECENT call (input + cache), overwritten
    /// each turn — drives the context-fill ring, not a running total.
    #[serde(default)]
    pub last_context_tokens: u64,
}

pub fn dir() -> PathBuf {
    crate::config::claude_config_dir().join("cwi_native")
}

pub fn path(id: &str) -> PathBuf {
    dir().join(format!("{id}.json"))
}

pub fn load(id: &str) -> Stored {
    std::fs::read_to_string(path(id))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist a session synchronously, best-effort. Used off the hot path (e.g.
/// `ensure_chat_exists`). Failures are logged, not swallowed.
pub fn save(id: &str, stored: &Stored) {
    match serde_json::to_string(stored) {
        Ok(json) => write_json(id, json),
        Err(e) => tracing::warn!(session = %id, "native store: serialize failed: {e}"),
    }
}

/// Async variant for the turn loop (`Engine::save`, called every tool step):
/// serialize on the async thread (cheap CPU) but offload the blocking disk write
/// to the blocking pool, so the executor isn't stalled on `std::fs::write` each
/// step. Borrows `stored` — no clone of the (growing) messages array.
pub async fn save_async(id: &str, stored: &Stored) {
    let json = match serde_json::to_string(stored) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(session = %id, "native store: serialize failed: {e}");
            return;
        }
    };
    let id = id.to_string();
    let _ = tokio::task::spawn_blocking(move || write_json(&id, json)).await;
}

fn write_json(id: &str, json: String) {
    let dir = dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(session = %id, "native store: create_dir_all {} failed: {e}", dir.display());
        return;
    }
    let p = path(id);
    if let Err(e) = std::fs::write(&p, json) {
        tracing::warn!(session = %id, "native store: write {} failed: {e}", p.display());
    }
}
