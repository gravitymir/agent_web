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

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Form, Json,
};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::claude_config_dir;
use crate::AppState;

type HmacSha256 = Hmac<Sha256>;

/// Write a secret file with owner-only permissions. On Unix the file is chmod'd
/// to `0o600` (matters on multi-user hosts); on other platforms this is a plain
/// write. Best-effort — a failed chmod doesn't fail the write.
pub(crate) fn write_private(path: &Path, contents: &[u8]) {
    let wrote = fs::write(path, contents).is_ok();
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
    pub expires: u64,     // unix seconds
    pub expires_in: u64,  // seconds remaining
}

pub struct Auth {
    pub enabled: bool,
    secret: Vec<u8>, // HMAC key for session cookies
    store: PathBuf,  // guest_tokens.json
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
            && b.len() >= 32 {
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
        self.load_tokens().into_iter().filter(|t| t.expires > n).collect()
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

    fn sign(&self, exp: u64) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.secret).expect("hmac accepts any key len");
        mac.update(exp.to_string().as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn make_cookie(&self, ttl: u64, secure: bool) -> String {
        let exp = now() + ttl;
        let val = format!("{}.{}", exp, self.sign(exp));
        let sec = if secure { "; Secure" } else { "" };
        format!("{COOKIE}={val}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl}{sec}")
    }

    fn valid_session(&self, cookie_header: Option<&str>) -> bool {
        let Some(h) = cookie_header else { return false };
        let Some(val) = h
            .split(';')
            .map(|c| c.trim())
            .find_map(|c| c.strip_prefix(COOKIE).and_then(|r| r.strip_prefix('=')))
        else {
            return false;
        };
        let Some((exp_s, sig)) = val.split_once('.') else { return false };
        let Ok(exp) = exp_s.parse::<u64>() else { return false };
        if exp <= now() {
            return false;
        }
        ct_eq(&self.sign(exp), sig)
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

/// Paths reachable without a session cookie: the login page, a health probe, and
/// the broker endpoint (which has its own per-session bearer-token auth).
fn is_public(path: &str) -> bool {
    path == "/login" || path == "/api/health" || path.starts_with("/broker/")
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
    if auth.valid_session(cookie) {
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
        if let Some(ttl) = auth.verify_code(code) {
            return authed_redirect(auth, ttl, is_secure(&headers));
        }
        return Html(login_html(true)).into_response();
    }
    Html(login_html(false)).into_response()
}

pub async fn login_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Response {
    let auth = &state.auth;
    if let Some(ttl) = auth.verify_code(&f.code) {
        return authed_redirect(auth, ttl, is_secure(&headers));
    }
    // Small delay to blunt automated guessing (codes are 128-bit, so this is
    // belt-and-suspenders).
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    Html(login_html(true)).into_response()
}

fn authed_redirect(auth: &Auth, ttl: u64, secure: bool) -> Response {
    let cookie = auth.make_cookie(ttl, secure);
    let mut resp = Redirect::to("/").into_response();
    if let Ok(v) = header::HeaderValue::from_str(&cookie) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
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
        .or_else(|| std::env::var("CWI_PUBLIC_URL").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| format!("http://localhost:{}", crate::executor::GUEST_APP_PORT))
        .trim_end_matches('/')
        .to_string()
}

/// Push the current token store to the running executor so guests validate
/// against codes minted here (the executor is disposable — its store is wiped by
/// each snapshot restore). Best-effort; a no-op if the VM is down.
async fn sync_to_executor(state: &Arc<AppState>) {
    let Some(json) = state.auth.store_json() else { return };
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

fn login_html(error: bool) -> String {
    let err = if error {
        r#"<p class="err">Неверный или истёкший код.</p>"#
    } else {
        ""
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
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
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
