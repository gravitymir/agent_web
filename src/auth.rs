//! Optional built-in access gate — guard the whole app behind time-limited
//! access codes, independent of any external identity provider or tunnel.
//! Enabled with `CWI_AUTH=1` (off by default, so existing runs are unchanged).
//!
//! Flow:
//! - The owner mints a code from the CLI: `agent_web guest new --ttl 24h`.
//!   It prints both a raw code and a magic link (`/login?code=...`).
//! - Each code is stored only as a SHA-256 hash + label + expiry in
//!   `<config_dir>/guest_tokens.json`; a leaked store yields no usable codes.
//! - Presenting a valid code (magic link or the login form) sets an
//!   HMAC-signed, expiring session cookie. Middleware then guards every route,
//!   including `/ws` — otherwise the gate would be bypassable over the socket.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Form, Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::config::claude_config_dir;

type HmacSha256 = Hmac<Sha256>;

/// Write a secret file with owner-only permissions. On Unix the file is chmod'd
/// to `0o600` (matters on multi-user hosts); on other platforms this is a plain
/// write. Best-effort — a failed chmod doesn't fail the write.
pub(crate) fn write_private(path: &Path, contents: &[u8]) {
    let wrote = crate::config::write_atomic(path, contents).is_ok();
    #[cfg(unix)]
    if wrote {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = wrote; // only consulted on unix; avoid an unused-var warning elsewhere
}

/// Session cookie name.
const COOKIE: &str = "cwi_session";

/// One active session per magic link. After this long without *real* activity
/// (a user action or an agent turn — not background polling) a seat is "asleep"
/// and a newcomer on the same link may take it over. An active seat (touched more
/// recently) rejects newcomers.
const SEAT_IDLE_SECS: u64 = 600; // 10 min

/// Who currently holds a magic link's single seat, and when it last saw activity.
struct Seat {
    /// Random per-login id; the cookie must carry a matching one to keep the seat.
    holder: String,
    /// Unix seconds of the last real activity.
    last_seen: u64,
}

/// Outcome of trying to claim a link's seat at login.
pub enum Claim {
    /// Granted — the `Set-Cookie` value to send.
    Granted(String),
    /// The link is held by an active session on another device.
    Busy,
    /// The code is invalid/expired.
    Invalid,
}

/// Outcome of an activity ping.
pub enum Touch {
    /// Still the holder — last-seen refreshed.
    Ok,
    /// Evicted (another device took the seat) or the seat is gone.
    Kicked,
}

#[derive(Clone, Serialize, Deserialize)]
struct Token {
    hash: String, // hex(sha256(code)) — never the code itself
    label: String,
    expires: u64, // unix seconds
}

/// API-facing summary of an active code (no secret material).
#[derive(Serialize)]
pub struct CodeInfo {
    pub label: String,
    pub expires: u64,    // unix seconds
    pub expires_in: u64, // seconds remaining
}

/// Current session state for the client — drives the guest's access countdown.
#[derive(Serialize)]
pub struct SessionInfo {
    /// True when the access gate is on (a guest instance behind a magic link).
    pub gated: bool,
    /// Unix seconds when access expires (None when not gated / no valid session).
    pub expires: Option<u64>,
    /// Seconds remaining (None as above). A hint only — the client ticks locally.
    pub expires_in: Option<u64>,
}

/// `GET /api/session` — the current session's expiry so the client can show a
/// countdown to access expiry. A gated route: on a guest it runs only for a valid
/// session; on the owner (gate off) it reports `gated: false`.
pub async fn session_info(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<SessionInfo> {
    if !state.auth.enabled {
        return Json(SessionInfo {
            gated: false,
            expires: None,
            expires_in: None,
        });
    }
    let cookie = headers.get(header::COOKIE).and_then(|v| v.to_str().ok());
    let exp = state.auth.session_expiry(cookie);
    let n = now();
    Json(SessionInfo {
        gated: true,
        expires: exp,
        expires_in: exp.map(|e| e.saturating_sub(n)),
    })
}

/// `POST /api/activity` — a real user action (or an in-flight agent turn) keeping
/// the session's seat warm so it stays the active holder. Public (bypasses the
/// cookie gate) so an evicted device gets a clean `{kicked:true}` instead of a
/// bare 401. Returns `idle_secs` so the client knows the window.
pub async fn activity(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !state.auth.enabled {
        return Json(serde_json::json!({ "ok": true })).into_response();
    }
    let cookie = headers.get(header::COOKIE).and_then(|v| v.to_str().ok());
    match state.auth.touch_seat(cookie) {
        Touch::Ok => {
            Json(serde_json::json!({ "ok": true, "idle_secs": SEAT_IDLE_SECS })).into_response()
        }
        Touch::Kicked => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "kicked": true })),
        )
            .into_response(),
    }
}

pub struct Auth {
    pub enabled: bool,
    secret: Vec<u8>, // HMAC key for session cookies
    store: PathBuf,  // guest_tokens.json
    /// One seat per magic link (keyed by the code's SHA-256 hash) — enforces
    /// "one active session per link" (see [`Auth::claim_seat`]/[`touch_seat`]).
    /// In-memory: on this disposable executor, a restart resets all seats.
    seats: Mutex<HashMap<String, Seat>>,
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

/// Constant-time string comparison, via the audited `subtle` crate rather than a
/// hand-rolled loop. `ct_eq` on unequal-length inputs returns false in constant
/// time w.r.t. the compared bytes (length isn't secret here — both sides are
/// fixed-length hex from our own encoding).
fn ct_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

fn load_or_create_secret(dir: &Path) -> Vec<u8> {
    let p = dir.join("auth_secret");
    if let Ok(s) = fs::read_to_string(&p)
        && let Ok(b) = hex::decode(s.trim())
        && b.len() >= 32
    {
        return b;
    }
    let mut b = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    write_private(&p, hex::encode(&b).as_bytes());
    b
}

fn random_code() -> String {
    let mut b = [0u8; 16]; // 128-bit
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

impl Auth {
    /// Build from the environment: `CWI_AUTH` toggles the gate; the secret and
    /// token store live in the resolved config dir (shared with the running
    /// server, so freshly minted codes work without a restart).
    pub fn load() -> Self {
        let enabled = std::env::var("CWI_AUTH")
            .map(|v| {
                let v = v.trim();
                v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
            })
            .unwrap_or(false);
        let dir = claude_config_dir();
        let _ = fs::create_dir_all(&dir);
        Self {
            enabled,
            secret: load_or_create_secret(&dir),
            store: dir.join("guest_tokens.json"),
            seats: Mutex::new(HashMap::new()),
        }
    }

    fn load_tokens(&self) -> Vec<Token> {
        fs::read_to_string(&self.store)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save_tokens(&self, t: &[Token]) {
        if let Ok(s) = serde_json::to_string_pretty(t) {
            write_private(&self.store, s.as_bytes());
        }
    }

    /// Mint a code valid for `ttl_secs`; returns the plaintext code (shown once).
    pub fn mint(&self, ttl_secs: u64, label: &str) -> String {
        let code = random_code();
        let mut toks = self.load_tokens();
        let n = now();
        toks.retain(|t| t.expires > n); // prune expired
        toks.push(Token {
            hash: sha256_hex(&code),
            label: label.to_string(),
            expires: n + ttl_secs,
        });
        self.save_tokens(&toks);
        code
    }

    /// Active (non-expired) tokens.
    fn active(&self) -> Vec<Token> {
        let n = now();
        self.load_tokens()
            .into_iter()
            .filter(|t| t.expires > n)
            .collect()
    }

    /// Remove tokens by label or by a full code. Returns how many were removed.
    pub fn revoke(&self, label_or_code: &str) -> usize {
        let mut toks = self.load_tokens();
        let before = toks.len();
        let code_hash = sha256_hex(label_or_code);
        toks.retain(|t| t.label != label_or_code && t.hash != code_hash);
        self.save_tokens(&toks);
        before - toks.len()
    }

    /// Active codes as API-friendly summaries — never the code or its hash.
    pub fn list_public(&self) -> Vec<CodeInfo> {
        let n = now();
        self.active()
            .into_iter()
            .map(|t| CodeInfo {
                label: t.label,
                expires: t.expires,
                expires_in: t.expires.saturating_sub(n),
            })
            .collect()
    }

    /// Raw JSON of the token store (hashed codes only), for pushing to the
    /// disposable executor so its gate validates codes minted here. `None` if
    /// the store doesn't exist yet.
    pub fn store_json(&self) -> Option<String> {
        fs::read_to_string(&self.store).ok()
    }

    /// Validate a code; returns the remaining lifetime (secs) on success, so the
    /// session cookie expires no later than the code does.
    fn verify_code(&self, code: &str) -> Option<u64> {
        let h = sha256_hex(code.trim());
        let n = now();
        self.load_tokens()
            .iter()
            .find(|t| t.expires > n && ct_eq(&t.hash, &h))
            .map(|t| t.expires - n)
    }

    /// HMAC over the seat-cookie fields (`code_hash.holder.exp`) — binds the
    /// cookie to a specific link (`code_hash`) and login (`holder`).
    fn sign_seat(&self, code_hash: &str, holder: &str, exp: u64) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.secret).expect("hmac accepts any key len");
        mac.update(format!("{code_hash}.{holder}.{exp}").as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn seat_cookie(&self, code_hash: &str, holder: &str, ttl: u64, secure: bool) -> String {
        let exp = now() + ttl;
        let sig = self.sign_seat(code_hash, holder, exp);
        let val = format!("{code_hash}.{holder}.{exp}.{sig}");
        let sec = if secure { "; Secure" } else { "" };
        format!("{COOKIE}={val}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl}{sec}")
    }

    /// Parse + verify the session cookie → `(code_hash, holder, exp)`. `None` if
    /// absent, malformed, badly signed, or expired. (Old single-field cookies from
    /// before seats fail here → the client is bounced to `/login` to re-claim.)
    fn parse_cookie(&self, cookie_header: Option<&str>) -> Option<(String, String, u64)> {
        let h = cookie_header?;
        let val = h
            .split(';')
            .map(|c| c.trim())
            .find_map(|c| c.strip_prefix(COOKIE).and_then(|r| r.strip_prefix('=')))?;
        let mut parts = val.splitn(4, '.');
        let code_hash = parts.next()?;
        let holder = parts.next()?;
        let exp: u64 = parts.next()?.parse().ok()?;
        let sig = parts.next()?;
        if exp <= now() {
            return None;
        }
        ct_eq(&self.sign_seat(code_hash, holder, exp), sig)
            .then(|| (code_hash.to_string(), holder.to_string(), exp))
    }

    /// The session cookie's expiry (unix secs) if valid — for the access countdown.
    pub fn session_expiry(&self, cookie_header: Option<&str>) -> Option<u64> {
        self.parse_cookie(cookie_header).map(|(_, _, exp)| exp)
    }

    /// Gate check: the cookie is valid AND still the current holder of its seat
    /// (i.e. this device wasn't evicted by a newcomer, and the server hasn't
    /// forgotten the seat via a restart).
    fn session_active(&self, cookie_header: Option<&str>) -> bool {
        let Some((code_hash, holder, _)) = self.parse_cookie(cookie_header) else {
            return false;
        };
        self.seats
            .lock()
            .map(|s| {
                s.get(&code_hash)
                    .is_some_and(|seat| ct_eq(&seat.holder, &holder))
            })
            .unwrap_or(false)
    }

    /// Claim a link's single seat at login. Grants when the seat is free or its
    /// holder is asleep (idle ≥ [`SEAT_IDLE_SECS`]) — evicting the sleeper; rejects
    /// when an active session already holds it. Returns the `Set-Cookie` on grant.
    pub fn claim_seat(&self, code: &str, secure: bool) -> Claim {
        let Some(ttl) = self.verify_code(code) else {
            return Claim::Invalid;
        };
        let code_hash = sha256_hex(code.trim());
        let holder = random_code();
        let n = now();
        {
            let mut seats = match self.seats.lock() {
                Ok(s) => s,
                Err(_) => return Claim::Invalid,
            };
            if let Some(seat) = seats.get(&code_hash)
                && n.saturating_sub(seat.last_seen) < SEAT_IDLE_SECS
            {
                return Claim::Busy;
            }
            seats.insert(
                code_hash.clone(),
                Seat {
                    holder: holder.clone(),
                    last_seen: n,
                },
            );
        }
        Claim::Granted(self.seat_cookie(&code_hash, &holder, ttl, secure))
    }

    /// Refresh a seat's last-activity from a real action. `Kicked` when the caller
    /// no longer holds the seat (evicted, or the seat was forgotten on restart).
    pub fn touch_seat(&self, cookie_header: Option<&str>) -> Touch {
        let Some((code_hash, holder, _)) = self.parse_cookie(cookie_header) else {
            return Touch::Kicked;
        };
        let Ok(mut seats) = self.seats.lock() else {
            return Touch::Kicked;
        };
        match seats.get_mut(&code_hash) {
            Some(seat) if ct_eq(&seat.holder, &holder) => {
                seat.last_seen = now();
                Touch::Ok
            }
            _ => Touch::Kicked,
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP: gate middleware + login endpoints.
// ---------------------------------------------------------------------------

/// True when the original client request arrived over HTTPS (Cloudflare and
/// other proxies set `x-forwarded-proto`). We only mark the cookie `Secure`
/// then, so local HTTP testing still works.
fn is_secure(h: &HeaderMap) -> bool {
    h.get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|p| p.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

/// Paths reachable without a session cookie: the login page, a health probe, the
/// broker endpoint (which has its own per-session bearer-token auth), and the
/// drain trigger. `/api/drain/begin` is the host→guest control channel for
/// Drain-Stop: the host (over the NAT forward) flips the guest into "no new
/// turns" before waiting on `/api/health`. It carries no session cookie, and is
/// idempotent + non-destructive (only sets a flag), so it must bypass the gate —
/// otherwise a gated guest's Drain-Stop can't refuse new turns during the drain.
fn is_public(path: &str) -> bool {
    path == "/login"
        || path == "/api/health"
        || path == "/api/activity"
        || path == "/api/drain/begin"
        || path == "/api/drain/end"
        || path.starts_with("/broker/")
}

/// Guard every route when the gate is enabled. No session → redirect browsers to
/// `/login`, and reject `/api/*` and `/ws` with 401 (so the socket is covered).
pub async fn gate(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let auth = &state.auth;
    if !auth.enabled {
        return next.run(req).await;
    }
    let path = req.uri().path().to_string();
    if is_public(&path) {
        return next.run(req).await;
    }
    let cookie = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok());
    if auth.session_active(cookie) {
        return next.run(req).await;
    }
    if path.starts_with("/api") || path == "/ws" {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Redirect::to("/login").into_response()
}

#[derive(Deserialize)]
pub struct LoginQuery {
    code: Option<String>,
}

#[derive(Deserialize)]
pub struct LoginForm {
    code: String,
}

pub async fn login_get(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LoginQuery>,
    headers: HeaderMap,
) -> Response {
    let auth = &state.auth;
    if let Some(code) = q.code.as_deref() {
        return claim_response(auth.claim_seat(code, is_secure(&headers)));
    }
    Html(login_html(None)).into_response()
}

/// Message shown when the link is already in active use elsewhere.
const BUSY_MSG: &str = "Эта ссылка сейчас открыта на другом устройстве. Попробуйте позже — место \
освободится после 10 минут бездействия.";

/// Turn a [`Claim`] into an HTTP response: grant sets the cookie and redirects to
/// the app; busy/invalid re-render the login page with a message.
fn claim_response(claim: Claim) -> Response {
    match claim {
        Claim::Granted(cookie) => {
            let mut resp = Redirect::to("/").into_response();
            if let Ok(v) = header::HeaderValue::from_str(&cookie) {
                resp.headers_mut().insert(header::SET_COOKIE, v);
            }
            resp
        }
        Claim::Busy => Html(login_html(Some(BUSY_MSG))).into_response(),
        Claim::Invalid => Html(login_html(Some("Неверный или истёкший код."))).into_response(),
    }
}

pub async fn login_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Response {
    let claim = state.auth.claim_seat(&f.code, is_secure(&headers));
    // Small delay on a bad code to blunt automated guessing (128-bit, so this is
    // belt-and-suspenders).
    if matches!(claim, Claim::Invalid) {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    claim_response(claim)
}

// ---------------------------------------------------------------------------
// Admin API: mint / list / revoke guest magic links from the master page.
// Admin-only (`state.admin`) — on a guest instance (executor) these 403, so a
// logged-in guest can't issue codes. The master page itself is protected by the
// external tunnel gate (e.g. Cloudflare Access), per the deployment model.
// ---------------------------------------------------------------------------

/// Base URL for guest magic links — the *guest* tunnel, not the admin host.
/// `CWI_GUEST_URL` overrides `CWI_PUBLIC_URL`; falls back to the local NAT port.
fn guest_base() -> String {
    std::env::var("CWI_GUEST_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("CWI_PUBLIC_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| format!("http://localhost:{}", crate::executor::GUEST_APP_PORT))
        .trim_end_matches('/')
        .to_string()
}

/// Push the current token store to the running executor so guests validate
/// against codes minted here (the executor is disposable — its store is wiped by
/// each snapshot restore). Best-effort; a no-op if the VM is down.
async fn sync_to_executor(state: &Arc<AppState>) {
    let Some(json) = state.auth.store_json() else {
        return;
    };
    let _ = tokio::task::spawn_blocking(move || crate::executor::push_guest_tokens(&json)).await;
}

#[derive(Deserialize)]
pub struct MintReq {
    label: String,
    ttl: Option<String>,
}

#[derive(Serialize)]
pub struct MintResp {
    code: String,
    magic_link: String,
    label: String,
    expires: u64,
}

/// `POST /api/links` — mint a guest magic link. Returns the code + link once.
pub async fn links_create(
    State(state): State<Arc<AppState>>,
    Json(req): Json<MintReq>,
) -> Response {
    if !state.admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    let label = req.label.trim().to_string();
    if label.is_empty() {
        return (StatusCode::BAD_REQUEST, "label required").into_response();
    }
    let ttl = req.ttl.as_deref().and_then(parse_ttl).unwrap_or(86400);
    let code = state.auth.mint(ttl, &label);
    sync_to_executor(&state).await;
    let base = guest_base();
    Json(MintResp {
        magic_link: format!("{base}/login?code={code}"),
        code,
        label,
        expires: now() + ttl,
    })
    .into_response()
}

/// `GET /api/links` — list active guest codes (labels + expiry only).
pub async fn links_list(State(state): State<Arc<AppState>>) -> Response {
    if !state.admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    Json(state.auth.list_public()).into_response()
}

/// `DELETE /api/links/{label}` — revoke every code with this label.
pub async fn links_revoke(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(label): axum::extract::Path<String>,
) -> Response {
    if !state.admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    let removed = state.auth.revoke(&label);
    sync_to_executor(&state).await;
    Json(serde_json::json!({ "removed": removed })).into_response()
}

fn login_html(notice: Option<&str>) -> String {
    let err = match notice {
        Some(msg) => format!(r#"<p class="err">{msg}</p>"#),
        None => String::new(),
    };
    format!(
        r##"<!doctype html><html lang="ru"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Agent Web — вход</title>
<style>
  :root {{ color-scheme: light dark; }}
  * {{ box-sizing: border-box; }}
  body {{ margin:0; min-height:100vh; display:flex; align-items:center; justify-content:center;
         font:16px/1.4 system-ui,-apple-system,Segoe UI,Roboto,sans-serif;
         background:#0e0f13; color:#e6e7ea; }}
  .card {{ width:min(92vw,360px); padding:28px 26px; border-radius:14px;
          background:#171922; border:1px solid #262a36; box-shadow:0 10px 40px rgba(0,0,0,.4); }}
  h1 {{ margin:0 0 4px; font-size:1.25rem; }}
  p.sub {{ margin:0 0 18px; color:#9aa0ab; font-size:.9rem; }}
  label {{ display:block; font-size:.8rem; color:#9aa0ab; margin-bottom:6px; }}
  input {{ width:100%; padding:11px 12px; border-radius:9px; border:1px solid #2c313d;
          background:#0e0f13; color:#e6e7ea; font-size:1rem; letter-spacing:.02em; }}
  input:focus {{ outline:none; border-color:#e07005; }}
  button {{ width:100%; margin-top:14px; padding:11px 12px; border:0; border-radius:9px;
           background:#e07005; color:#fff; font-size:1rem; font-weight:600; cursor:pointer; }}
  button:hover {{ background:#c56304; }}
  .err {{ color:#e5534b; font-size:.85rem; margin:0 0 12px; }}
</style></head>
<body>
  <div class="card">
    <h1>Agent Web</h1>
    <p class="sub">Введите код доступа</p>
    {err}
    <form method="post" action="/login" autocomplete="off">
      <label for="code">Код доступа</label>
      <input id="code" name="code" type="text" autofocus placeholder="напр. 3f9a…">
      <button type="submit">Войти</button>
    </form>
  </div>
</body></html>"##
    )
}

// ---------------------------------------------------------------------------
// CLI: `agent_web guest <new|list|revoke>`.
// ---------------------------------------------------------------------------

/// Parse a TTL like `24h`, `30m`, `7d`, `3600s`, or a bare number of seconds.
fn parse_ttl(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = if let Some(n) = s.strip_suffix('d') {
        (n, 86400)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else {
        (s, 1)
    };
    num.trim().parse::<u64>().ok().map(|v| v * mult)
}

fn human_ttl(secs: u64) -> String {
    if secs >= 86400 {
        format!("{}d {}h", secs / 86400, (secs % 86400) / 3600)
    } else if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

fn arg_val(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Handle the `guest` subcommand and exit. `args` is everything after `guest`.
pub fn run_cli(args: &[String]) {
    let auth = Auth::load();
    match args.first().map(|s| s.as_str()) {
        Some("new") => {
            let ttl = arg_val(args, "--ttl")
                .and_then(|v| parse_ttl(&v))
                .unwrap_or(86400);
            let label = arg_val(args, "--label").unwrap_or_else(|| "guest".into());
            let base = arg_val(args, "--url")
                .or_else(|| std::env::var("CWI_PUBLIC_URL").ok())
                .unwrap_or_else(|| "http://localhost:8787".into());
            let base = base.trim_end_matches('/');
            let code = auth.mint(ttl, &label);
            println!();
            println!("  Access code : {code}");
            println!("  Magic link  : {base}/login?code={code}");
            println!("  Label       : {label}");
            println!("  Valid for   : {}", human_ttl(ttl));
            println!();
            println!("  Share the magic link (or the code) with the guest. Revoke with:");
            println!("    agent_web guest revoke {label}");
        }
        Some("list") => {
            let n = now();
            let toks = auth.active();
            if toks.is_empty() {
                println!("no active access codes");
            } else {
                for t in toks {
                    println!("  {:<24} expires in {}", t.label, human_ttl(t.expires - n));
                }
            }
        }
        Some("revoke") => {
            let key = args.get(1).cloned().unwrap_or_default();
            if key.is_empty() {
                eprintln!("usage: agent_web guest revoke <label|code>");
                return;
            }
            let removed = auth.revoke(&key);
            println!("revoked {removed} token(s)");
        }
        _ => {
            eprintln!(
                "usage: agent_web guest <new|list|revoke> [--ttl 24h] [--label NAME] [--url https://host]"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seat_rejects_active_but_takes_over_idle() {
        let store = std::env::temp_dir().join(format!("cwi_seat_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&store);
        let auth = Auth {
            enabled: true,
            secret: vec![7u8; 32],
            store: store.clone(),
            seats: Mutex::new(HashMap::new()),
        };
        let code = auth.mint(3600, "t"); // writes the token store this Auth reads
        // First device claims the seat and gets a cookie.
        let Claim::Granted(set_cookie) = auth.claim_seat(&code, false) else {
            panic!("first claim should be granted");
        };
        let cookie = set_cookie.split(';').next().unwrap().to_string(); // "cwi_session=…"
        assert!(auth.session_active(Some(&cookie)));
        assert!(matches!(auth.touch_seat(Some(&cookie)), Touch::Ok));
        // A second device on the still-active link is rejected.
        assert!(matches!(auth.claim_seat(&code, false), Claim::Busy));
        // Make the seat look idle, then a newcomer takes it over.
        for seat in auth.seats.lock().unwrap().values_mut() {
            seat.last_seen = now() - SEAT_IDLE_SECS - 1;
        }
        let Claim::Granted(cookie2) = auth.claim_seat(&code, false) else {
            panic!("an idle seat should be grantable to a newcomer");
        };
        assert_ne!(set_cookie, cookie2, "the takeover mints a new holder");
        // The evicted first device is no longer active and gets kicked.
        assert!(!auth.session_active(Some(&cookie)));
        assert!(matches!(auth.touch_seat(Some(&cookie)), Touch::Kicked));
        let _ = std::fs::remove_file(&store);
    }

    #[test]
    fn public_paths_bypass_the_gate() {
        // Reachable without a session: login, health, the broker (own bearer
        // auth), and the host→guest drain trigger.
        for p in [
            "/login",
            "/api/health",
            "/api/activity",
            "/api/drain/begin",
            "/api/drain/end",
            "/broker/v1/messages",
        ] {
            assert!(is_public(p), "{p} should be public");
        }
    }

    #[test]
    fn protected_paths_require_the_gate() {
        // Data + admin routes must NOT be public — they go through the gate, and
        // admin routes (e.g. /api/links) additionally 403 for guests in-handler.
        for p in ["/api/links", "/api/chats", "/api/workspace.zip", "/ws", "/"] {
            assert!(!is_public(p), "{p} must not be public");
        }
    }
}
