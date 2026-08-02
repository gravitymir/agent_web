//! Streaming client for an Anthropic-compatible `/v1/messages` endpoint.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use futures_util::StreamExt;
use serde_json::Value;

use crate::agent::provider::{Auth, Provider};

/// A structured error from the API call, so the retry loop can classify it
/// (retryable vs fatal) and honor a server-provided `Retry-After` delay.
#[derive(Debug)]
pub struct ApiError {
    /// HTTP status, or `None` for a transport/network error (no response).
    pub status: Option<u16>,
    /// `Retry-After` header value in seconds, if the server sent one.
    pub retry_after: Option<u64>,
    /// Response body / error text (for logging + surfacing to the user).
    pub message: String,
}

impl ApiError {
    fn network(message: String) -> Self {
        Self { status: None, retry_after: None, message }
    }

    /// Worth retrying? Rate limit (429), overloaded (529), transient 5xx, request
    /// timeout (408), or any transport error (no HTTP status).
    pub fn is_retryable(&self) -> bool {
        match self.status {
            Some(s) => matches!(s, 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529),
            None => true,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "HTTP {}: {}", s, self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for ApiError {}

/// POST a Messages request with `stream: true` and invoke `on_event` for each
/// parsed SSE `data:` payload (the raw Anthropic stream events). The `client` is
/// reused across calls (connection pooling).
///
/// `interrupt` is polled after every SSE event; when it becomes `true` the HTTP
/// response stream is aborted so the caller can stop promptly even if the
/// provider keeps buffering data.
pub async fn stream(
    provider: &Provider,
    client: &reqwest::Client,
    mut body: Value,
    mut on_event: impl FnMut(Value),
    interrupt: &AtomicBool,
) -> Result<()> {
    body["stream"] = Value::Bool(true);

    let mut req = client
        .post(provider.messages_url())
        .header("anthropic-version", &provider.anthropic_version)
        .header("content-type", "application/json");
    req = match provider.auth {
        Auth::XApiKey => req.header("x-api-key", &provider.api_key),
        Auth::Bearer => req.header("authorization", format!("Bearer {}", provider.api_key)),
    };
    if let Some(beta) = &provider.anthropic_beta {
        req = req.header("anthropic-beta", beta.clone());
    }

    let resp = match req.json(&body).send().await {
        Ok(r) => r,
        // Connection refused / DNS / TLS / timeout — no HTTP response at all.
        Err(e) => return Err(ApiError::network(e.to_string()).into()),
    };
    let status = resp.status();
    if !status.is_success() {
        // Honor a server-provided Retry-After (integer seconds form).
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        let text = resp.text().await.unwrap_or_default();
        return Err(ApiError { status: Some(status.as_u16()), retry_after, message: text }.into());
    }

    // Abort the underlying HTTP body when the user hits stop. `bytes_stream()`
    // returns chunks from the response body; dropping it cancels further reads.
    let resp = resp;
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();

    loop {
        if interrupt.load(Ordering::SeqCst) {
            break;
        }
        match tokio::time::timeout(std::time::Duration::from_millis(50), stream.next()).await {
            Ok(Some(chunk)) => {
                let chunk = chunk?;
                buf.push_str(&String::from_utf8_lossy(&chunk));
                // Process complete lines; SSE `data:` lines carry the JSON events.
                while let Some(nl) = buf.find('\n') {
                    let line = buf[..nl].trim_end().to_string();
                    buf.drain(..=nl);
                    if let Some(data) = line.strip_prefix("data:") {
                        let data = data.trim();
                        if data.is_empty() || data == "[DONE]" {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            on_event(v);
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(_) => continue, // timeout: re-check interrupt
        }
    }

    Ok(())
}
