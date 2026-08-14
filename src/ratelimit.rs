//! Per-client HTTP rate limiting (defense-in-depth alongside the per-connection
//! WS limiter in `ws.rs`). A generous budget on every route stops a flood of
//! `/api/*` calls; a stricter budget on `/login` blunts access-code brute-force
//! (the codes are 128-bit random, so this is belt-and-suspenders with the
//! login handler's fixed delay).
//!
//! Client identity behind a Cloudflare tunnel: the socket peer is always the
//! tunnel (localhost), so we key on `CF-Connecting-IP` / `X-Forwarded-For` (set
//! by our own trusted tunnel) and fall back to a single `local` bucket for direct
//! connections (the owner on loopback).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// key → recent request timestamps (sliding window).
static BUCKETS: LazyLock<Mutex<HashMap<String, VecDeque<Instant>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Call counter to trigger the occasional stale-key sweep.
static CALLS: AtomicUsize = AtomicUsize::new(0);
/// Sweep the map every N calls (amortizes the O(n) retain).
const SWEEP_EVERY: usize = 512;
/// A key with no hit in this long is dead for any of our windows (all <= 60s) —
/// drop it so idle IPs (thousands can share the Cloudflare edge) don't accumulate.
const STALE: Duration = Duration::from_secs(300);

/// Record a hit for `key`; return `false` if it exceeds `max` within `window`.
fn allow(key: String, max: usize, window: Duration) -> bool {
    let now = Instant::now();
    let mut map = BUCKETS.lock().unwrap_or_else(|e| e.into_inner());

    // Periodic prune: `pop_front` only trims within a live key, so a key that's
    // never hit again would linger forever. Drop keys whose newest hit is stale.
    if CALLS
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(SWEEP_EVERY)
    {
        map.retain(|_, hits| {
            hits.back()
                .is_some_and(|&last| now.duration_since(last) < STALE)
        });
    }

    let hits = map.entry(key).or_default();
    while let Some(&front) = hits.front() {
        if now.duration_since(front) > window {
            hits.pop_front();
        } else {
            break;
        }
    }
    if hits.len() >= max {
        return false;
    }
    hits.push_back(now);
    true
}

/// Best-effort client identifier: the real IP behind our tunnel, else `local`.
fn client_id(headers: &HeaderMap) -> String {
    for h in ["cf-connecting-ip", "x-forwarded-for"] {
        if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()) {
            // X-Forwarded-For may be a comma list; the first hop is the client.
            let ip = v.split(',').next().unwrap_or(v).trim();
            if !ip.is_empty() {
                return ip.to_string();
            }
        }
    }
    "local".to_string()
}

/// Axum middleware: reject with 429 once a client exceeds its budget. `/login`
/// (magic-link GET and form POST) gets a tight budget; everything else a loose
/// one that never bites normal use.
pub async fn limit(req: Request, next: Next) -> Response {
    let (max, window, bucket) = if req.uri().path() == "/login" {
        (20usize, Duration::from_secs(60), "login")
    } else {
        (300usize, Duration::from_secs(60), "gen")
    };
    let key = format!("{bucket}:{}", client_id(req.headers()));
    if !allow(key, max, window) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "rate limit exceeded — slow down",
        )
            .into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_max_then_blocks() {
        let win = Duration::from_secs(60);
        let k = "test:1.2.3.4".to_string();
        for _ in 0..5 {
            assert!(allow(k.clone(), 5, win));
        }
        assert!(!allow(k.clone(), 5, win)); // 6th within window is blocked
    }

    #[test]
    fn separate_keys_are_independent() {
        let win = Duration::from_secs(60);
        assert!(allow("test:a".to_string(), 1, win));
        assert!(!allow("test:a".to_string(), 1, win));
        assert!(allow("test:b".to_string(), 1, win)); // different key, own budget
    }
}
