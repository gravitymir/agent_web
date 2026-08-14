//! Broker: a credential-holding proxy for the isolated executor.
//!
//! The executor VM runs the agent with NO real provider key — only a per-session
//! *broker token*. When it needs a model completion it POSTs the Anthropic-shaped
//! request to `/broker/v1/messages` here. The broker authenticates the token,
//! charges one request against its budget, rewrites the model to the owner's real
//! model, forwards to the real provider (`Provider::from_env`, which holds the
//! real key), and streams the response straight back. The real key never leaves
//! this process, so a compromised executor can't steal it — only spend its own
//! (capped, expiring, revocable) session budget.
//!
//! Provider-agnostic: it forwards to whatever the owner configured (Qwen /
//! Anthropic / …). The upstream must be Anthropic-`/v1/messages`-shaped, since the
//! executor speaks that wire format (Gemini's own shape isn't proxied).

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;
use crate::agent::provider::{Auth, Provider};
use crate::auth::{now, sha256_hex, write_private};

#[derive(Clone, Serialize, Deserialize)]
struct BrokerToken {
    hash: String, // hex(sha256(token)) — never the token itself
    label: String,
    expires: u64, // unix seconds
    #[serde(default)]
    max_requests: u64, // 0 = unlimited
    #[serde(default)]
    used: u64,
}

pub struct Broker {
    store: PathBuf, // broker_tokens.json
}

impl Broker {
    pub fn load() -> Self {
        Self {
            store: crate::config::claude_config_dir().join("broker_tokens.json"),
        }
    }

    fn load_tokens(&self) -> Vec<BrokerToken> {
        std::fs::read_to_string(&self.store)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save_tokens(&self, t: &[BrokerToken]) {
        if let Ok(s) = serde_json::to_string_pretty(t) {
            write_private(&self.store, s.as_bytes());
        }
    }

    /// Mint a session token valid for `ttl_secs` with a `max_requests` budget
    /// (0 = unlimited). Returns the plaintext token (shown once).
    pub fn mint(&self, ttl_secs: u64, max_requests: u64, label: &str) -> String {
        let mut b = [0u8; 24];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut b);
        let token = hex::encode(b);
        let mut toks = self.load_tokens();
        let n = now();
        toks.retain(|t| t.expires > n); // prune expired
        toks.push(BrokerToken {
            hash: sha256_hex(&token),
            label: label.to_string(),
            expires: n + ttl_secs,
            max_requests,
            used: 0,
        });
        self.save_tokens(&toks);
        token
    }

    fn active(&self) -> Vec<BrokerToken> {
        let n = now();
        self.load_tokens()
            .into_iter()
            .filter(|t| t.expires > n)
            .collect()
    }

    pub fn revoke(&self, label: &str) -> usize {
        let mut toks = self.load_tokens();
        let before = toks.len();
        toks.retain(|t| t.label != label);
        self.save_tokens(&toks);
        before - toks.len()
    }

    /// Validate a bearer token and charge one request against its budget.
    fn authorize(&self, token: &str) -> Result<(), (StatusCode, &'static str)> {
        let h = sha256_hex(token.trim());
        let n = now();
        let mut toks = self.load_tokens();
        let Some(t) = toks.iter_mut().find(|t| t.hash == h) else {
            return Err((StatusCode::UNAUTHORIZED, "invalid broker token"));
        };
        if t.expires <= n {
            return Err((StatusCode::UNAUTHORIZED, "broker token expired"));
        }
        if t.max_requests != 0 && t.used >= t.max_requests {
            return Err((
                StatusCode::PAYMENT_REQUIRED,
                "broker token budget exhausted",
            ));
        }
        t.used += 1;
        self.save_tokens(&toks);
        Ok(())
    }
}

/// Extract a `Bearer <token>` value from the Authorization header.
fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
}

/// `POST /broker/v1/messages` — authenticate, charge budget, forward to the real
/// provider with the real key, stream the response back.
pub async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };
    if let Err((code, msg)) = state.broker.authorize(&token) {
        return (code, msg).into_response();
    }

    let provider = Provider::from_env();
    if !provider.has_key() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "broker: no upstream provider key configured",
        )
            .into_response();
    }

    // Rewrite the model to the owner's real model (the executor doesn't know it,
    // and the broker decides which model — hence which cost — to spend on).
    let outbound: Vec<u8> = match serde_json::from_slice::<Value>(&body) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("model".into(), Value::String(provider.model.clone()));
            }
            serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
        }
        Err(_) => body.to_vec(),
    };

    let mut req = state
        .http
        .post(provider.messages_url())
        .header("content-type", "application/json")
        .header("anthropic-version", &provider.anthropic_version)
        .body(outbound);
    req = match provider.auth {
        Auth::XApiKey => req.header("x-api-key", &provider.api_key),
        Auth::Bearer => req.header("authorization", format!("Bearer {}", provider.api_key)),
        Auth::GoogleApiKey => req.header("x-goog-api-key", &provider.api_key),
    };
    if let Some(beta) = &provider.anthropic_beta {
        req = req.header("anthropic-beta", beta.clone());
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let ctype = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("text/event-stream")
                .to_string();
            Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, ctype)
                .body(Body::from_stream(resp.bytes_stream()))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("broker upstream error: {e}"),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// CLI: `agent_web broker <new|list|revoke>`.
// ---------------------------------------------------------------------------

fn arg_val(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Parse a TTL like `30m`, `24h`, `7d` (bare number = seconds). Default on error.
fn parse_ttl(s: &str) -> u64 {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        _ => (s, 1),
    };
    num.parse::<u64>().unwrap_or(0).saturating_mul(mult)
}

/// Handle the `broker` subcommand and exit. `args` is everything after `broker`.
pub fn run_cli(args: &[String]) {
    let broker = Broker::load();
    match args.first().map(|s| s.as_str()) {
        Some("new") => {
            let label = arg_val(args, "--label").unwrap_or_else(|| "executor".into());
            let ttl = parse_ttl(&arg_val(args, "--ttl").unwrap_or_else(|| "24h".into()));
            let budget = arg_val(args, "--budget")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            if ttl == 0 {
                eprintln!("bad --ttl (use e.g. 30m, 24h, 7d)");
                return;
            }
            let token = broker.mint(ttl, budget, &label);
            println!("Broker session token (shown once — copy it now):\n");
            println!("  {token}\n");
            println!(
                "On the executor set:\n  CWI_AGENT_PROVIDER=broker\n  CWI_AGENT_API_KEY={token}\n  CWI_AGENT_BASE_URL=http://<host>:8787/broker"
            );
            println!(
                "\nLabel: {label}   valid: {}   budget: {}",
                arg_val(args, "--ttl").unwrap_or_else(|| "24h".into()),
                if budget == 0 {
                    "unlimited".into()
                } else {
                    format!("{budget} requests")
                }
            );
        }
        Some("list") => {
            let toks = broker.active();
            if toks.is_empty() {
                println!("no active broker tokens");
                return;
            }
            let n = now();
            for t in toks {
                let left = (t.expires.saturating_sub(n)) / 3600;
                let budget = if t.max_requests == 0 {
                    "unlimited".to_string()
                } else {
                    format!("{}/{} used", t.used, t.max_requests)
                };
                println!("{}  expires in ~{left}h  budget: {budget}", t.label);
            }
        }
        Some("revoke") => match args.get(1) {
            Some(label) => {
                let n = broker.revoke(label);
                println!("revoked {n} token(s) labelled '{label}'");
            }
            None => eprintln!("usage: agent_web broker revoke <label>"),
        },
        _ => eprintln!(
            "usage: agent_web broker <new [--label L] [--ttl 24h] [--budget N] | list | revoke <label>>"
        ),
    }
}
