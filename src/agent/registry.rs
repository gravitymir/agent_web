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
        ProviderMeta {
            id: "qwen",
            name: "Qwen (Alibaba)",
            // The Anthropic endpoint (/apps/anthropic) has no model list, so we
            // fetch from DashScope's OpenAI-compatible surface, which returns the
            // expected `data[].id` shape. Same Bearer key works for both.
            models_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/models",
            auth: Auth::Bearer,
            key_var: "CWI_AGENT_QWEN_API_KEY",
            fallback: &["qwen3.8-max", "qwen3.7-max", "qwen3.7-plus", "qwen3.6-plus"],
        },
        ProviderMeta {
            id: "gemini",
            name: "Gemini (Google)",
            // Real endpoint, for reference — its `{"models":[{"name","displayName"}]}`
            // shape doesn't match `parse_models`'s `data[].id` expectation, so this
            // always yields an empty list and falls back to the static one below
            // (harmless: no error, just skips the live fetch in practice).
            models_url: "https://generativelanguage.googleapis.com/v1beta/models",
            auth: Auth::GoogleApiKey,
            key_var: "CWI_AGENT_GEMINI_API_KEY",
            fallback: &["gemini-pro-latest", "gemini-flash-latest", "gemini-flash-lite-latest"],
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

/// Heuristic: keep only text/chat LLM models in the settings dropdown. Provider
/// `/models` endpoints (DashScope especially) also list image, video, audio,
/// embedding and rerank models, which don't speak the chat Messages API —
/// selecting one 400s every turn. Exclude by well-known id markers. Multimodal
/// *chat* models (e.g. `qwen3-vl-…`) are kept; only single-purpose media/OCR/
/// embedding/rerank ids are dropped.
fn is_chat_model(id: &str) -> bool {
    let m = id.to_ascii_lowercase();
    const NON_CHAT: &[&str] = &[
        "image", "ocr", "video", "t2v", "i2v", "t2i", "wanx", "wan2", "wan-",
        "audio", "-tts", "tts-", "-asr", "asr-", "cosyvoice", "paraformer",
        "sambert", "speech", "voice", "embedding", "-embed", "rerank",
        "diffusion", "flux", "sdxl", "sd3",
    ];
    !NON_CHAT.iter().any(|k| m.contains(k))
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
        Auth::GoogleApiKey => vec![("x-goog-api-key", key)],
    };
    match fetch_model_list(p.models_url, &headers).await {
        Ok(list) => {
            let chat: Vec<ModelInfo> = list.into_iter().filter(|m| is_chat_model(&m.id)).collect();
            if chat.is_empty() { fallback() } else { chat }
        }
        _ => fallback(),
    }
}

/// Build the full provider list for the settings UI. Each provider's model list
/// is a live HTTP fetch, so run them concurrently — `/api/providers` is bounded
/// by the slowest single provider, not their sum.
pub async fn providers() -> Vec<ProviderInfo> {
    let tasks = registry().into_iter().map(|p| async move {
        let has_key = !std::env::var(p.key_var).unwrap_or_default().is_empty();
        let models = models_for(&p).await;
        ProviderInfo {
            id: p.id.to_string(),
            name: p.name.to_string(),
            has_key,
            models,
        }
    });
    futures_util::future::join_all(tasks).await
}

#[cfg(test)]
mod tests {
    use super::is_chat_model;

    #[test]
    fn keeps_chat_models_drops_media_and_embeddings() {
        // Chat / multimodal-chat models are kept.
        for id in [
            "qwen3.8-max", "qwen3.7-plus", "qwen-max", "qwen3-vl-235b-a22b-thinking",
            "claude-opus-5", "kimi-k2.7-code", "glm-5.2", "gemini-pro-latest",
        ] {
            assert!(is_chat_model(id), "should keep {id}");
        }
        // Media / OCR / embedding / rerank models are dropped.
        for id in [
            "qwen-image-3.0-pro", "qwen-vl-ocr-2025-11-20", "wan2.5-t2v", "wanx-i2v",
            "qwen-audio-turbo", "qwen-tts", "paraformer-realtime", "cosyvoice-v2",
            "text-embedding-v4", "gte-rerank", "flux-schnell", "stable-diffusion-3",
        ] {
            assert!(!is_chat_model(id), "should drop {id}");
        }
    }
}
