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

/// Persist a session, best-effort. Failures can't be handled by the caller
/// (save runs inside the turn loop), but they're logged rather than swallowed
/// so a broken disk / permissions problem is visible instead of silent data loss.
pub fn save(id: &str, stored: &Stored) {
    let dir = dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(session = %id, "native store: create_dir_all {} failed: {e}", dir.display());
        return;
    }
    let json = match serde_json::to_string(stored) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(session = %id, "native store: serialize failed: {e}");
            return;
        }
    };
    let p = path(id);
    if let Err(e) = std::fs::write(&p, json) {
        tracing::warn!(session = %id, "native store: write {} failed: {e}", p.display());
    }
}
