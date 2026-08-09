//! Persistent store for user-assigned chat metadata (title + icon).
//!
//! Claude Code owns the session `.jsonl` files, so we never write into them.
//! Custom metadata lives in our own sidecar file — a single JSON map of
//! `session_id -> { title, icon }` — and overrides the auto-derived title when
//! present.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// One model's cumulative contribution to a chat — the raw material for the
/// per-model breakdown and the "named" Agentron (`Agₘ = durationₘ × tokensₘ`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelStat {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatMeta {
    #[serde(default)]
    pub title: String,
    /// Optional icon (an emoji glyph) shown before the title in the list.
    #[serde(default)]
    pub icon: Option<String>,
    /// Cumulative `duration_ms` from every `result` event the chat has ever
    /// produced (see `session.rs::track_turn_duration`) — the CLI never
    /// persists this to its own `.jsonl`, so we do, for the Agentron effort
    /// metric (`H` in `Ag = H × (Tᵢ+Tₒ)/1e6`; tokens come from the `.jsonl`
    /// itself, only the time axis needs a sidecar).
    #[serde(default)]
    pub duration_ms: u64,
    /// Per-model contribution, keyed by the model string (`"gemini / …"`,
    /// `"claude-opus-4-8"`, …). Accumulated forward as the chat runs, so a chat
    /// that switches engines/models mid-way records each model's share. Old
    /// chats predate this and stay empty (no retroactive split is possible).
    #[serde(default)]
    pub models: HashMap<String, ModelStat>,
}

pub struct MetaStore {
    map: HashMap<String, ChatMeta>,
    /// Serialized-JSON channel to the background writer thread. `save()` sends the
    /// full serialized map here instead of blocking on `fs::write` — important
    /// because callers hold the `AppState` meta lock (often on an async worker,
    /// e.g. per tool-step in `session.rs`), so disk I/O must not run under it.
    writer: Sender<String>,
}

impl MetaStore {
    /// Load the store from `path`, tolerating a missing or malformed file, and
    /// spawn the background writer that owns all disk writes for it.
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, ChatMeta>>(&s).ok())
            .unwrap_or_default();

        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            // Each message is the full serialized map. While one write is in
            // flight, later updates queue; drain them and write only the latest so
            // a burst of turns collapses into a single disk write (debounce).
            while let Ok(mut json) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    json = newer;
                }
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&path, json);
            }
        });

        Self { map, writer: tx }
    }

    pub fn get(&self, id: &str) -> Option<ChatMeta> {
        self.map.get(id).cloned()
    }

    /// Set metadata for a chat. A blank title with no icon removes the entry —
    /// unless it still carries accumulated duration (Agentron), in which case
    /// it's kept around with blank title/icon rather than losing that total.
    /// Persists to disk.
    pub fn set(&mut self, id: String, title: String, icon: Option<String>) -> Result<()> {
        let title = title.trim().to_string();
        let icon = icon.and_then(|i| {
            let i = i.trim().to_string();
            if i.is_empty() {
                None
            } else {
                Some(i)
            }
        });
        // Preserve the accumulated duration AND per-model breakdown — set() must
        // never drop stats just because the title/icon were cleared.
        let (duration_ms, models) = self
            .map
            .get(&id)
            .map(|m| (m.duration_ms, m.models.clone()))
            .unwrap_or_default();

        if title.is_empty() && icon.is_none() && duration_ms == 0 && models.is_empty() {
            self.map.remove(&id);
        } else {
            self.map.insert(id, ChatMeta { title, icon, duration_ms, models });
        }
        self.save()
    }

    /// Record one finished turn: adds `ms` to the chat's total duration and, when
    /// a model is known, folds `(input, output, ms)` into that model's bucket.
    /// A no-op for a zero-duration turn (so a never-finished turn leaves nothing).
    pub fn record_turn(
        &mut self,
        id: &str,
        model: &str,
        input: u64,
        output: u64,
        ms: u64,
    ) -> Result<()> {
        if ms == 0 && input == 0 && output == 0 {
            return Ok(());
        }
        let entry = self.map.entry(id.to_string()).or_default();
        entry.duration_ms = entry.duration_ms.saturating_add(ms);
        if !model.is_empty() {
            let m = entry.models.entry(model.to_string()).or_default();
            m.input_tokens = m.input_tokens.saturating_add(input);
            m.output_tokens = m.output_tokens.saturating_add(output);
            m.duration_ms = m.duration_ms.saturating_add(ms);
        }
        self.save()
    }

    /// Remove any stored metadata for a chat (used when the chat is deleted).
    pub fn remove(&mut self, id: &str) -> Result<()> {
        if self.map.remove(id).is_some() {
            self.save()?;
        }
        Ok(())
    }

    /// Serialize the map and hand it to the background writer. Serialization is
    /// CPU-only (safe under the caller's lock); the actual disk write happens off
    /// this thread. A dropped receiver (writer thread gone) is ignored — the
    /// in-memory state is still correct and this is a best-effort sidecar.
    fn save(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.map)?;
        let _ = self.writer.send(json);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MetaStore;
    use std::path::PathBuf;

    fn temp_store(name: &str) -> MetaStore {
        let path: PathBuf = std::env::temp_dir().join(format!("cwi_titles_test_{name}.json"));
        let _ = std::fs::remove_file(&path); // start clean if a prior run left it
        MetaStore::load(path)
    }

    #[test]
    fn record_turn_accumulates_and_is_a_noop_when_empty() {
        let mut m = temp_store("accumulates");
        assert_eq!(m.get("a"), None); // nothing yet — no entry for an empty turn
        m.record_turn("a", "", 0, 0, 0).unwrap();
        assert_eq!(m.get("a"), None);
        m.record_turn("a", "", 0, 0, 1000).unwrap();
        m.record_turn("a", "", 0, 0, 2500).unwrap();
        assert_eq!(m.get("a").unwrap().duration_ms, 3500);
    }

    #[test]
    fn clearing_title_and_icon_keeps_accumulated_duration() {
        let mut m = temp_store("keeps_duration");
        m.set("b".into(), "Some title".into(), Some("🚀".into())).unwrap();
        m.record_turn("b", "", 0, 0, 5000).unwrap();
        // Clearing title/icon must not silently drop the Agentron total.
        m.set("b".into(), "".into(), None).unwrap();
        let entry = m.get("b").unwrap();
        assert_eq!(entry.title, "");
        assert_eq!(entry.icon, None);
        assert_eq!(entry.duration_ms, 5000);
    }

    #[test]
    fn clearing_title_and_icon_with_no_duration_removes_the_entry() {
        let mut m = temp_store("removes_entry");
        m.set("c".into(), "Title".into(), None).unwrap();
        m.set("c".into(), "".into(), None).unwrap();
        assert_eq!(m.get("c"), None);
    }
}
