//! Fetching the list of available models from the Anthropic Models API, using
//! the OAuth access token Claude Code stores on this machine.
//!
//! The token lives in `~/.claude/.credentials.json` (`claudeAiOauth.accessToken`)
//! and is short-lived — Claude Code refreshes it during normal use.
//!
//! The model list barely changes (new models every few weeks at most), so we
//! cache it to `~/.claude/cwi_models.json` and only re-hit the API once the
//! cache is older than [`CACHE_TTL_SECS`]. If the API call fails we serve the
//! stale cache rather than nothing.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const CACHE_TTL_SECS: u64 = 24 * 60 * 60; // refresh at most once a day

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
}

#[derive(Serialize, Deserialize)]
struct Cache {
    fetched_at: u64,
    models: Vec<ModelInfo>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("cwi_models.json"))
}

fn read_cache() -> Option<Cache> {
    let content = std::fs::read_to_string(cache_path()?).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_cache(models: &[ModelInfo]) {
    let Some(path) = cache_path() else { return };
    let cache = Cache {
        fetched_at: now_secs(),
        models: models.to_vec(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&cache) {
        let _ = std::fs::write(path, json);
    }
}

/// Return the model list for `/api/models`: cache if fresh, otherwise refresh
/// (falling back to a stale cache if the refresh fails). The second element is
/// an error message only when there is nothing at all to return.
pub async fn models_for_api() -> (Vec<ModelInfo>, Option<String>) {
    let cache = read_cache();

    if let Some(c) = &cache {
        if now_secs().saturating_sub(c.fetched_at) < CACHE_TTL_SECS {
            return (c.models.clone(), None);
        }
    }

    match fetch_models().await {
        Ok(models) => {
            write_cache(&models);
            (models, None)
        }
        Err(e) => match cache {
            Some(c) => (c.models, None), // serve stale rather than nothing
            None => (Vec::new(), Some(e.to_string())),
        },
    }
}

/// Read Claude Code's OAuth access token from its local credentials file.
fn read_oauth_token() -> Result<String> {
    let path = dirs::home_dir()
        .context("no home directory")?
        .join(".claude")
        .join(".credentials.json");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let v: Value = serde_json::from_str(&content)?;
    v["claudeAiOauth"]["accessToken"]
        .as_str()
        .map(str::to_string)
        .context("no claudeAiOauth.accessToken in credentials")
}

/// Query the Anthropic Models API and return `{ id, display_name }` for each.
async fn fetch_models() -> Result<Vec<ModelInfo>> {
    let token = read_oauth_token()?;
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.anthropic.com/v1/models?limit=100")
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        // Subscription OAuth tokens require this beta header to reach the API.
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await?
        .error_for_status()?;

    let v: Value = resp.json().await?;
    let models = v["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m["id"].as_str()?.to_string();
                    let display_name = m["display_name"].as_str().unwrap_or(&id).to_string();
                    Some(ModelInfo { id, display_name })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(models)
}
