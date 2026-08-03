//! Persistent store for user-assigned chat metadata (title + icon).
//!
//! Claude Code owns the session `.jsonl` files, so we never write into them.
//! Custom metadata lives in our own sidecar file — a single JSON map of
//! `session_id -> { title, icon }` — and overrides the auto-derived title when
//! present.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

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
}

pub struct MetaStore {
    path: PathBuf,
    map: HashMap<String, ChatMeta>,
}

impl MetaStore {
    /// Load the store from `path`, tolerating a missing or malformed file.
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, ChatMeta>>(&s).ok())
            .unwrap_or_default();
        Self { path, map }
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
        let duration_ms = self.map.get(&id).map(|m| m.duration_ms).unwrap_or(0);

        if title.is_empty() && icon.is_none() && duration_ms == 0 {
            self.map.remove(&id);
        } else {
            self.map.insert(id, ChatMeta { title, icon, duration_ms });
        }
        self.save()
    }

    /// Add `ms` to a chat's cumulative turn duration, creating a blank entry
    /// (no title/icon) if none exists yet. A no-op for `ms == 0` so a chat
    /// that never finishes a turn doesn't leave an empty entry behind.
    pub fn add_duration(&mut self, id: &str, ms: u64) -> Result<()> {
        if ms == 0 {
            return Ok(());
        }
        let entry = self.map.entry(id.to_string()).or_default();
        entry.duration_ms = entry.duration_ms.saturating_add(ms);
        self.save()
    }

    /// Remove any stored metadata for a chat (used when the chat is deleted).
    pub fn remove(&mut self, id: &str) -> Result<()> {
        if self.map.remove(id).is_some() {
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.map)?;
        std::fs::write(&self.path, json)?;
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
    fn add_duration_accumulates_and_is_a_noop_for_zero() {
        let mut m = temp_store("accumulates");
        assert_eq!(m.get("a"), None); // nothing yet — no entry created for 0ms
        m.add_duration("a", 0).unwrap();
        assert_eq!(m.get("a"), None);
        m.add_duration("a", 1000).unwrap();
        m.add_duration("a", 2500).unwrap();
        assert_eq!(m.get("a").unwrap().duration_ms, 3500);
    }

    #[test]
    fn clearing_title_and_icon_keeps_accumulated_duration() {
        let mut m = temp_store("keeps_duration");
        m.set("b".into(), "Some title".into(), Some("🚀".into())).unwrap();
        m.add_duration("b", 5000).unwrap();
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
