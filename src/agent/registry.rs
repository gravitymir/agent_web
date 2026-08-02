//! Provider registry for the settings UI: which providers exist, whether a key
//! is configured, and each provider's available models (fetched live, with a
//! static fallback).

use serde::Serialize;

use crate::agent::provider::Auth;
use crate::models::{fetch_model_list, ModelInfo};

struct ProviderMeta {
    id: &'static str,
    name: &'static str,
    models_url: &'static str,
    auth: Auth,
    key_var: &'static str,
    fallback: &'static [&'static str],
}

fn registry() -> Vec<ProviderMeta> {
    vec![
        ProviderMeta {
            id: "anthropic",
            name: "Anthropic (Claude)",
            models_url: "https://api.anthropic.com/v1/models",
            auth: Auth::XApiKey,
            key_var: "CWI_AGENT_CLAUDE_API_KEY",
            fallback: &["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"],
        },
        ProviderMeta {
            id: "kimi",
            name: "Kimi (Moonshot)",
            models_url: "https://api.moonshot.ai/v1/models",
            auth: Auth::Bearer,
            key_var: "CWI_AGENT_KIMI_API_KEY",
            fallback: &["kimi-k2.7-code", "kimi-k2.7-code-highspeed", "kimi-k2.6", "kimi-k3"],
        },
        ProviderMeta {
            id: "glm",
            name: "GLM (Z.ai)",
            models_url: "https://api.z.ai/api/paas/v4/models",
            auth: Auth::Bearer,
            key_var: "CWI_AGENT_GLM_API_KEY",
            fallback: &["glm-5.2", "glm-4.6", "glm-5-turbo", "glm-4.5-air"],
        },
    ]
}

#[derive(Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub has_key: bool,
    pub models: Vec<ModelInfo>,
}

/// Fetch a provider's model list from its OpenAI/Anthropic-style `/models`
/// endpoint; fall back to the static list on any failure or missing key.
async fn models_for(p: &ProviderMeta) -> Vec<ModelInfo> {
    let fallback = || -> Vec<ModelInfo> {
        p.fallback
            .iter()
            .map(|id| ModelInfo { id: id.to_string(), display_name: id.to_string() })
            .collect()
    };
    let key = std::env::var(p.key_var).unwrap_or_default();
    if key.is_empty() {
        return fallback();
    }
    let headers: Vec<(&str, String)> = match p.auth {
        Auth::XApiKey => vec![
            ("x-api-key", key),
            ("anthropic-version", "2023-06-01".to_string()),
        ],
        Auth::Bearer => vec![("authorization", format!("Bearer {key}"))],
    };
    match fetch_model_list(p.models_url, &headers).await {
        Ok(list) if !list.is_empty() => list,
        _ => fallback(),
    }
}

/// Build the full provider list for the settings UI.
pub async fn providers() -> Vec<ProviderInfo> {
    let mut out = Vec::new();
    for p in registry() {
        let has_key = !std::env::var(p.key_var).unwrap_or_default().is_empty();
        let models = models_for(&p).await;
        out.push(ProviderInfo {
            id: p.id.to_string(),
            name: p.name.to_string(),
            has_key,
            models,
        });
    }
    out
}
