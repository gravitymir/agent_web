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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatMeta {
    #[serde(default)]
    pub title: String,
    /// Optional icon (an emoji glyph) shown before the title in the list.
    #[serde(default)]
    pub icon: Option<String>,
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

    /// Set metadata for a chat. A blank title with no icon removes the entry.
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

        if title.is_empty() && icon.is_none() {
            self.map.remove(&id);
        } else {
            self.map.insert(id, ChatMeta { title, icon });
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

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.map)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }
}
