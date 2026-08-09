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

use anyhow::{anyhow, Context, Result};
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
    Some(crate::config::claude_config_dir().join("cwi_models.json"))
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
    let json = match serde_json::to_string_pretty(&cache) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!("models cache: serialize failed: {e}");
            return;
        }
    };
    if let Err(e) = std::fs::write(&path, json) {
        tracing::warn!("models cache: write {} failed: {e}", path.display());
    }
}

/// Return the model list for `/api/models`: cache if fresh, otherwise refresh
/// (falling back to a stale cache if the refresh fails). The second element is
/// an error message only when there is nothing at all to return.
pub async fn models_for_api() -> (Vec<ModelInfo>, Option<String>) {
    let cache = read_cache();

    if let Some(c) = &cache
        && now_secs().saturating_sub(c.fetched_at) < CACHE_TTL_SECS {
            return (c.models.clone(), None);
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

/// Read Claude Code's OAuth access token: prefer `CLAUDE_CODE_OAUTH_TOKEN` (the
/// long-lived subscription token used when running against an isolated
/// `CLAUDE_CONFIG_DIR`), falling back to the credentials file in that config dir.
fn read_oauth_token() -> Result<String> {
    if let Ok(tok) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
        let tok = tok.trim();
        if !tok.is_empty() {
            return Ok(tok.to_string());
        }
    }
    let path = crate::config::claude_config_dir().join(".credentials.json");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let v: Value = serde_json::from_str(&content)?;
    v["claudeAiOauth"]["accessToken"]
        .as_str()
        .map(str::to_string)
        .context("no claudeAiOauth.accessToken in credentials")
}

/// Parse a Models endpoint's `data[]` array into `{ id, display_name }`.
/// Shared with the native-engine provider registry.
pub fn parse_models(v: &Value) -> Vec<ModelInfo> {
    v["data"]
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
        .unwrap_or_default()
}

/// GET a `/models` endpoint with the given headers and parse its model list.
/// Retries once on 429 / 503 / 529, honoring a `Retry-After` header (capped, so
/// the `/api/models` handler never blocks for long — a stale cache is served if
/// this ultimately fails).
pub async fn fetch_model_list(url: &str, headers: &[(&str, String)]) -> Result<Vec<ModelInfo>> {
    const MAX_RETRY_WAIT_SECS: u64 = 5;
    let client = reqwest::Client::new();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let mut req = client.get(url);
        for (name, value) in headers {
            req = req.header(*name, value.clone());
        }
        let resp = req.send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(parse_models(&resp.json().await?));
        }
        if matches!(status.as_u16(), 429 | 503 | 529) && attempt < 2 {
            let wait = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(1)
                .min(MAX_RETRY_WAIT_SECS);
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            continue;
        }
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("HTTP {}: {}", status.as_u16(), text));
    }
}

/// Query the Anthropic Models API (via Claude Code's OAuth token) for `/api/models`.
async fn fetch_models() -> Result<Vec<ModelInfo>> {
    let token = read_oauth_token()?;
    fetch_model_list(
        "https://api.anthropic.com/v1/models?limit=100",
        &[
            ("authorization", format!("Bearer {token}")),
            ("anthropic-version", "2023-06-01".to_string()),
            // Subscription OAuth tokens require this beta header to reach the API.
            ("anthropic-beta", "oauth-2025-04-20".to_string()),
        ],
    )
    .await
}
