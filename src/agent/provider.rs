//! Provider abstraction for the native agent engine.
//!
//! Any Anthropic-compatible `/v1/messages` endpoint works — Anthropic itself,
//! Moonshot (Kimi), or Zhipu (GLM). A preset is chosen with `CWI_AGENT_PROVIDER`
//! (`anthropic` | `kimi` | `glm` | `gemini`); individual fields can be overridden
//! with `CWI_AGENT_BASE_URL`, `CWI_AGENT_MODEL`, `CWI_AGENT_API_KEY`, etc.
//!
//! Gemini is NOT wire-compatible with `/v1/messages` (different request/response
//! shape, different streaming events, no stable tool-call id) — `kind` flags
//! this so `Engine::run_turn` routes it through `agent::gemini` instead of
//! `agent::client`, translating to/from our internal Anthropic-shaped messages
//! at the edges (see `gemini.rs`'s module doc) rather than bending the shared
//! agent loop/Accumulator/storage to a second wire format.

use std::env;

/// How the API key is presented to the endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    /// Anthropic first-party: `x-api-key: <key>`.
    XApiKey,
    /// Anthropic-compatible third parties (Kimi/GLM): `Authorization: Bearer <key>`.
    Bearer,
    /// Gemini: `x-goog-api-key: <key>`.
    GoogleApiKey,
}

/// Which wire format `Engine::run_turn` should speak to this provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `/v1/messages`, Anthropic streaming events — Anthropic/Kimi/GLM.
    AnthropicMessages,
    /// `:streamGenerateContent`, Gemini's own request/response shape.
    Gemini,
}

#[derive(Clone, Debug)]
pub struct Provider {
    pub name: String,
    /// Base origin, no trailing slash. `/v1/messages` is appended (Anthropic-
    /// shaped providers only — Gemini builds its own path, see `gemini.rs`).
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
    /// Ask for summarized reasoning (`thinking.display = "summarized"` for
    /// Anthropic-shaped providers; Gemini's own `thinkingConfig` for Gemini).
    pub thinking: bool,
    pub anthropic_version: String,
    pub anthropic_beta: Option<String>,
    pub auth: Auth,
    pub kind: Kind,
}

impl Provider {
    pub fn from_env() -> Self {
        let preset = env::var("CWI_AGENT_PROVIDER").unwrap_or_else(|_| "anthropic".to_string());
        Self::build(&preset, None)
    }

    /// Build a provider from a chosen preset and optional model override. The API
    /// key is taken from the generic `CWI_AGENT_API_KEY` or the provider-specific
    /// env var. Env overrides (`CWI_AGENT_MODEL`, `CWI_AGENT_BASE_URL`) still apply.
    pub fn build(preset: &str, model_override: Option<String>) -> Self {
        // (base_url, model, auth, thinking_default, beta, kind)
        let (base_url, model, auth, thinking, beta, kind) = match preset {
            "kimi" | "moonshot" => (
                "https://api.moonshot.ai/anthropic",
                "kimi-k2.7-code",
                Auth::Bearer,
                false,
                None::<&str>,
                Kind::AnthropicMessages,
            ),
            "glm" | "zhipu" | "zai" => (
                "https://api.z.ai/api/anthropic",
                "glm-5.2",
                Auth::Bearer,
                false,
                None::<&str>,
                Kind::AnthropicMessages,
            ),
            // "-latest" aliases auto-track Google's current recommended release
            // instead of pinning an exact version that gets deprecated (Gemini
            // model ids churn quickly — see gemini.rs).
            "gemini" | "google" => (
                "https://generativelanguage.googleapis.com",
                "gemini-pro-latest",
                Auth::GoogleApiKey,
                true,
                None::<&str>,
                Kind::Gemini,
            ),
            // Anthropic first-party (needs a console API key, not the subscription).
            _ => (
                "https://api.anthropic.com",
                "claude-opus-5",
                Auth::XApiKey,
                true,
                None::<&str>,
                Kind::AnthropicMessages,
            ),
        };

        let base_url = env::var("CWI_AGENT_BASE_URL").unwrap_or_else(|_| base_url.to_string());
        let model = model_override
            .filter(|s| !s.is_empty())
            .or_else(|| env::var("CWI_AGENT_MODEL").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| model.to_string());
        // Key: the generic CWI_AGENT_API_KEY, else the provider-specific one.
        let key_var = match preset {
            "kimi" | "moonshot" => "CWI_AGENT_KIMI_API_KEY",
            "glm" | "zhipu" | "zai" => "CWI_AGENT_GLM_API_KEY",
            "gemini" | "google" => "CWI_AGENT_GEMINI_API_KEY",
            _ => "CWI_AGENT_CLAUDE_API_KEY",
        };
        let api_key = env::var("CWI_AGENT_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| env::var(key_var).ok().filter(|s| !s.is_empty()))
            .unwrap_or_default();
        let max_tokens = env::var("CWI_AGENT_MAX_TOKENS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8192);
        let thinking = env::var("CWI_AGENT_THINKING")
            .ok()
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
            .unwrap_or(thinking);
        let anthropic_beta = env::var("CWI_AGENT_BETA").ok().or(beta.map(String::from));

        Provider {
            name: preset.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            max_tokens,
            thinking,
            anthropic_version: "2023-06-01".to_string(),
            anthropic_beta,
            auth,
            kind,
        }
    }

    pub fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base_url)
    }

    pub fn has_key(&self) -> bool {
        !self.api_key.is_empty()
    }
}
