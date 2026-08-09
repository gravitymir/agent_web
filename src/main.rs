mod agent;
mod auth;
mod banner;
mod claude;
mod config;
mod history;
mod ids;
mod models;
mod ratelimit;
mod session;
mod titles;
mod usage;
mod wizard;
mod ws;

use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, State, WebSocketUpgrade},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, put},
    Json, Router,
};
use serde::Deserialize;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::Config;
use crate::session::SessionManager;
use crate::titles::MetaStore;

/// Upper bound on a single inbound WebSocket message. Generous enough for a
/// prompt with several base64-inlined images / attached files, but bounded so a
/// client can't force a huge allocation. Enforced at the protocol layer.
const MAX_WS_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Content-Security-Policy served with every response — an extra layer against
/// injected content. `script-src` is `'self'` only: all scripts are external
/// files (the former inline preboot snippet now lives in `/js/preboot.js`), so an
/// injected `<script>` is blocked outright. `style-src` still needs
/// `'unsafe-inline'` — the frontend applies inline `style="…"` in JS-built
/// markup. `data:` covers pasted images; no external origins are allowed.
const CSP: &str = "default-src 'self'; base-uri 'self'; object-src 'none'; \
    img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; \
    script-src 'self'; connect-src 'self' ws: wss:; \
    frame-src 'self'; form-action 'self'";

/// Attach security headers (CSP + nosniff) to every response.
async fn add_security_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        axum::http::HeaderValue::from_static(CSP),
    );
    h.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    resp
}

/// Shared application state.
pub struct AppState {
    pub config: Config,
    /// User-assigned chat metadata (title + icon) plus accumulated turn
    /// duration (Agentron), overrides/overlays the auto-derived chat summary.
    /// Shared with `SessionManager` (its actors add duration as turns finish),
    /// hence the `Arc` — not just this handle's own private lock.
    pub meta: Arc<Mutex<MetaStore>>,
    /// Live per-session keeper processes.
    pub sessions: Arc<SessionManager>,
    /// Optional built-in access gate (CWI_AUTH); a no-op when disabled.
    pub auth: Arc<auth::Auth>,
}

/// Load `KEY=VALUE` lines from an env file (default `.env`, override with
/// `CWI_ENV_FILE`) into the process environment. Existing vars win, so the
/// shell can still override the file.
fn load_env_file() {
    // Resolve like the static dir: an explicit CWI_ENV_FILE wins; otherwise try
    // the cwd, then next to the executable, then the dev `target/<profile>/`
    // layout — so a deployed folder of `agent_web.exe + .env + static/` works no
    // matter which directory the exe is launched from.
    let candidates: Vec<std::path::PathBuf> = match std::env::var("CWI_ENV_FILE") {
        Ok(p) => vec![std::path::PathBuf::from(p)],
        Err(_) => {
            let mut v = vec![std::path::PathBuf::from(".env")];
            if let Ok(exe) = std::env::current_exe()
                && let Some(dir) = exe.parent() {
                    v.push(dir.join(".env"));
                    v.push(dir.join("..").join("..").join(".env"));
                }
            v
        }
    };
    let Some(content) = candidates
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
    else {
        return;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            if !k.is_empty() && std::env::var(k).is_err() {
                // SAFETY: called only from `main`'s synchronous startup phase,
                // before the async runtime/threads exist — no concurrent env access.
                unsafe { std::env::set_var(k, v) };
            }
        }
    }
}

/// Synchronous startup phase. Everything that MUTATES the environment
/// (`load_env_file`, the CLAUDE_CONFIG_DIR pin, the wizard) runs here, *before*
/// the Tokio runtime — and therefore its worker threads — exist. That is what
/// makes the `set_var` calls sound: no other thread can be reading the
/// environment concurrently, which is exactly the data race edition 2024 marks
/// `set_var`/`remove_var` `unsafe` to prevent. Once this returns, the environment
/// is frozen for the rest of the process.
fn main() -> anyhow::Result<()> {
    load_env_file();

    // Portable storage: when CLAUDE_CONFIG_DIR isn't set, pin it to the resolved
    // default (a `chats/` dir next to the exe) and EXPORT it, so the spawned
    // `claude` CLI subprocess inherits the same isolated dir and never falls back
    // to the user's own ~/.claude. Done before the `guest` CLI and the wizard so
    // both see the same resolved dir.
    if std::env::var("CLAUDE_CONFIG_DIR").map_or(true, |v| v.trim().is_empty()) {
        let dir = config::claude_config_dir();
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: single-threaded startup — no runtime or other threads exist yet,
        // so nothing can read the environment concurrently with this write.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &dir) };
    }

    // `agent_web guest <new|list|revoke>` — manage built-in access codes, then
    // exit. Handled before the wizard so it never triggers the interactive menu.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(|s| s == "guest").unwrap_or(false) {
        auth::run_cli(&argv[2..]);
        return Ok(());
    }

    // Interactive engine/port picker (only on a TTY; skipped for pipes/services
    // and when CWI_NO_MENU is set). Sets CWI_* env vars that Config reads below.
    wizard::run();

    // Environment is fully configured and frozen; now spin up the async runtime.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

/// Async server entry point. Runs after `main()`'s synchronous env setup, so it
/// and every task it spawns only ever *read* the environment.
async fn run() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env();
    // The banner is the launch summary (engine, port, paths, settings). Don't also
    // log the full Config — it just duplicates the banner and clutters the terminal.
    banner::print_startup(&config);

    // The one thing worth a log line: in native mode a missing API key means every
    // request will fail with an auth error — warn loudly up front instead of only
    // when the first turn dies. (CLI mode authenticates via the claude OAuth token,
    // whose expiry surfaces on the first /api/models call.)
    if config.native_engine {
        let p = agent::provider::Provider::from_env();
        if !p.has_key() {
            tracing::warn!(
                "no API key configured for native provider '{}': set CWI_AGENT_API_KEY \
                 or the provider-specific CWI_AGENT_*_API_KEY in your .env — requests will \
                 fail with an authorization error until then",
                p.name
            );
        }
    }

    let meta_path = config
        .projects_root
        .parent()
        .unwrap_or(&config.projects_root)
        .join("cwi_titles.json");
    // Connect MCP servers only in native-engine mode (they'd be unused by the CLI).
    let mcp = if config.native_engine {
        let client = agent::mcp::McpClient::init().await;
        if !client.is_empty() {
            tracing::info!("mcp: connected");
        }
        Some(std::sync::Arc::new(client))
    } else {
        None
    };

    let meta = Arc::new(Mutex::new(MetaStore::load(meta_path)));
    let gate = Arc::new(auth::Auth::load());
    if gate.enabled {
        tracing::info!("access gate ENABLED (CWI_AUTH) — every route requires a valid session");
    }
    let state = Arc::new(AppState {
        config: config.clone(),
        meta: meta.clone(),
        sessions: SessionManager::new(config.clone(), mcp, meta),
        auth: gate,
    });

    let static_dir = config.static_dir.clone();
    let sessions = state.sessions.clone(); // for graceful shutdown cleanup

    let app = Router::new()
        .route("/api/chats", get(list_chats))
        .route("/api/chats/{id}", get(load_chat).delete(delete_chat))
        .route("/api/chats/{id}/meta", put(set_meta))
        .route("/api/models", get(list_models))
        .route("/api/providers", get(list_providers))
        .route("/api/usage", get(get_usage))
        .route("/api/health", get(health))
        .route("/metrics", get(metrics))
        .route("/ws", get(ws_upgrade))
        .route("/login", get(auth::login_get).post(auth::login_post))
        .fallback_service(ServeDir::new(static_dir))
        // Access gate (no-op unless CWI_AUTH): guards every route above, incl. /ws.
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth::gate))
        .layer(axum::middleware::from_fn(add_security_headers))
        // Per-client HTTP rate limiting — outermost, so floods are rejected before
        // any work (incl. /login brute-force, which the gate wouldn't stop).
        .layer(axum::middleware::from_fn(ratelimit::limit))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // Keep the subscription-usage snapshot fresh while running so a guest
    // container (which reads it via CWI_USAGE_FILE) shows current limits even
    // when the owner is idle. Owner only — skip native mode and the guest itself.
    if !config.native_engine && std::env::var("CWI_USAGE_FILE").is_err() {
        let cfg = config.clone();
        tokio::spawn(async move {
            loop {
                let _ = usage::usage_json(&cfg).await;
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            }
        });
    }

    // Graceful drain: `agentctl drain` sends SIGUSR1 (`docker kill --signal`),
    // which flips the flag so the WS layer refuses new turns while in-flight ones
    // finish. The operator then polls /api/health until active_turns == 0 and
    // stops the container. Unix only — SIGUSR1 doesn't exist on Windows, and drain
    // targets the Linux guest container.
    #[cfg(unix)]
    {
        let sessions = sessions.clone(); // the shutdown-cleanup handle (line above)
        tokio::spawn(async move {
            let mut sig = match tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::user_defined1(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("could not install SIGUSR1 drain handler: {e}");
                    return;
                }
            };
            while sig.recv().await.is_some() {
                sessions.set_draining(true);
                tracing::info!(
                    active_turns = sessions.active_turns(),
                    "drain requested (SIGUSR1) — refusing new turns; waiting for in-flight turns to finish"
                );
            }
        });
    }

    // No "listening on" log — the banner's BIND line already shows the address.
    let listener = match tokio::net::TcpListener::bind(&config.bind_addr).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let port = config
                .bind_addr
                .rsplit(':')
                .next()
                .unwrap_or(&config.bind_addr);
            eprintln!();
            eprintln!("  Port {port} is already in use ({}).", config.bind_addr);
            eprintln!("  Another agent_web instance (or another program) is holding it.");
            eprintln!();
            if cfg!(windows) {
                eprintln!("  Find what's using it:");
                eprintln!("      netstat -ano | findstr :{port}");
                eprintln!("  Stop it (PowerShell, one line):");
                eprintln!("      Stop-Process -Id (Get-NetTCPConnection -LocalPort {port} -State Listen).OwningProcess -Force");
            } else {
                eprintln!("  Find what's using it:  ss -ltnp | grep :{port}");
                eprintln!("  Stop it:               fuser -k {port}/tcp   (or: kill <PID>)");
            }
            eprintln!();
            eprintln!("  Then start agent_web again.");
            std::process::exit(1);
        }
        Err(e) => return Err(e.into()),
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Stop accepting → kill live keepers (their processes) before exiting.
    tracing::info!("shutting down; cleaning up sessions");
    sessions.shutdown_all().await;
    Ok(())
}

/// Resolve when the process is asked to stop (Ctrl-C, or SIGTERM on unix).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Liveness/version endpoint. Also drives `agentctl drain`: `draining` reports
/// whether new turns are being refused, and `active_turns` counts in-flight agent
/// turns so the operator knows when it's safe to stop the container.
async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "draining": state.sessions.is_draining(),
        "active_turns": state.sessions.active_turns(),
    }))
}

/// Prometheus-style metrics.
async fn metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let sessions = state.sessions.session_count();
    format!(
        "# HELP cwi_sessions Live session keepers\n\
         # TYPE cwi_sessions gauge\n\
         cwi_sessions {sessions}\n"
    )
}

async fn list_chats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Always list BOTH stores regardless of the active engine, so the sidebar is
    // stable across a CWI_ENGINE switch. Chats from the inactive engine are shown
    // read-only ("frozen") in the frontend, keyed off `ChatSummary::engine`.
    let native_dir = agent::store::dir();
    let mut chats = history::list_chats(&state.config.session_dir(), Some(&native_dir));
    // Overlay user-assigned titles and icons.
    if let Ok(meta) = state.meta.lock() {
        for chat in &mut chats {
            if let Some(m) = meta.get(&chat.id) {
                if !m.title.is_empty() {
                    chat.title = m.title;
                    chat.custom_title = true;
                }
                chat.icon = m.icon;
                chat.duration_ms = m.duration_ms;
                // Per-model breakdown, sorted by total tokens desc.
                let mut models: Vec<history::ModelContribution> = m
                    .models
                    .into_iter()
                    .map(|(model, s)| history::ModelContribution {
                        model,
                        input_tokens: s.input_tokens,
                        output_tokens: s.output_tokens,
                        duration_ms: s.duration_ms,
                    })
                    .collect();
                models.sort_by(|a, b| {
                    (b.input_tokens + b.output_tokens).cmp(&(a.input_tokens + a.output_tokens))
                });
                chat.models = models;
            }
        }
    }
    Json(chats)
}

#[derive(Deserialize)]
struct SetMeta {
    #[serde(default)]
    title: String,
    #[serde(default)]
    icon: Option<String>,
}

/// Ensure a chat record exists on disk so a brand-new named chat shows up in
/// the sidebar before the first message is sent.
fn ensure_chat_exists(state: &AppState, id: &str) {
    if state.config.native_engine {
        let path = agent::store::path(id);
        if !path.exists() {
            agent::store::save(id, &agent::store::Stored::default());
        }
    } else {
        let path = state.config.session_dir().join(format!("{id}.jsonl"));
        if !path.exists() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path,
                serde_json::json!({"timestamp": chrono::Utc::now().to_rfc3339()}).to_string() + "\n");
        }
    }
}

async fn set_meta(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<SetMeta>,
) -> impl IntoResponse {
    // Reject non-UUID ids before they become a file path (`ensure_chat_exists`).
    if !ids::is_valid_session_id(&id) {
        return StatusCode::BAD_REQUEST;
    }
    match state.meta.lock() {
        Ok(mut meta) => {
            // Creating a new chat from the frontend saves its title/icon before
            // any message is sent. Make sure the chat appears in the list.
            ensure_chat_exists(&state, &id);
            match meta.set(id, body.title, body.icon) {
                Ok(()) => StatusCode::NO_CONTENT,
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
            }
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn load_chat(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    // Reject non-UUID ids before they become a file path (arbitrary file read).
    if !ids::is_valid_session_id(&id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // Load from whichever store holds the chat, regardless of active engine —
    // frozen chats are still fully readable.
    let native_dir = agent::store::dir();
    let messages = history::load_chat(
        &state.config.session_dir(), Some(&native_dir), &id);
    Json(messages).into_response()
}

/// Delete a chat: kill any live keeper, then remove its transcript
/// (`<id>.jsonl`), the native store (`cwi_native/<id>.json`), and any custom
/// title/icon metadata. Best-effort — a missing file is not an error.
async fn delete_chat(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // A chat id is always a UUID; reject anything else (path traversal / injection).
    if !ids::is_valid_session_id(&id) {
        return StatusCode::BAD_REQUEST;
    }

    // Stop the running process (if any) before touching files on disk.
    state.sessions.remove(&id).await;

    let jsonl = state.config.session_dir().join(format!("{id}.jsonl"));
    remove_if_present(&jsonl, &id);
    remove_if_present(&agent::store::path(&id), &id);
    match state.meta.lock() {
        Ok(mut meta) => {
            if let Err(e) = meta.remove(&id) {
                tracing::warn!(session = %id, "delete_chat: meta remove failed: {e}");
            }
        }
        Err(e) => tracing::warn!(session = %id, "delete_chat: meta lock poisoned: {e}"),
    }

    StatusCode::NO_CONTENT
}

/// Remove a file for chat deletion. A missing file is the expected best-effort
/// case (silent); any other error (e.g. the file is locked by another process)
/// is logged so a failed delete isn't invisible.
fn remove_if_present(path: &std::path::Path, id: &str) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(session = %id, "delete_chat: remove {} failed: {e}", path.display()),
    }
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> axum::response::Response {
    // Reject cross-site WebSocket hijacking: a page on evil.com sends its own
    // Origin, so refuse any Origin that isn't a loopback host. (A browser always
    // sends Origin on a WS handshake; non-browser tools may omit it, and they
    // can't be driven cross-site, so a missing Origin is allowed.)
    if !origin_is_local(&headers) {
        tracing::warn!("ws: rejected cross-origin upgrade");
        return StatusCode::FORBIDDEN.into_response();
    }
    // Cap inbound frame/message size so a client can't force a huge allocation.
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| ws::handle_socket(socket, state))
        .into_response()
}

/// True if the request has no `Origin` (non-browser client) or an `Origin` that
/// is **same-origin** with the `Host` we were reached on, or a loopback host.
/// Blocks cross-site WebSocket hijacking without breaking access via a LAN
/// address (same-origin covers any bind address; loopback covers dev).
fn origin_is_local(headers: &axum::http::HeaderMap) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else { return false };
    // The authority ("host:port") of the Origin URL.
    let authority = match origin.split_once("://") {
        Some((_, rest)) => rest.split('/').next().unwrap_or(rest),
        None => return false,
    };
    // Same-origin: the Origin's authority equals the Host header we answered on.
    if let Some(host) = headers.get(axum::http::header::HOST).and_then(|h| h.to_str().ok())
        && authority.eq_ignore_ascii_case(host) {
            return true;
        }
    // Otherwise only a loopback host is allowed.
    let host_only = authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority);
    matches!(host_only, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Available models from the Anthropic Models API (via Claude Code's OAuth token).
/// Returns `{ "models": [...] }`, or `{ "models": [], "error": "..." }` on failure.
async fn list_models() -> impl IntoResponse {
    let (models, error) = models::models_for_api().await;
    if let Some(e) = &error {
        tracing::warn!("models fetch failed: {e}");
    }
    Json(serde_json::json!({ "models": models, "error": error }))
}

/// Native-engine providers (Anthropic / Kimi / GLM) with their models, for the
/// settings UI: pick a provider first, then a model of that provider.
/// Subscription usage/limits (5-hour window + weekly), sourced from the CLI's
/// own `/usage` screen. Only meaningful for the CLI engine; native returns
/// `{available:false}`. See `src/usage.rs`.
async fn get_usage(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(usage::usage_json(&state.config).await)
}

async fn list_providers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let providers = agent::registry::providers().await;
    // The operator's configured provider (wizard / CWI_AGENT_PROVIDER) — the UI
    // uses it as the authoritative default so the badge/dropdown match the running
    // engine instead of a stale localStorage choice.
    let active_provider = agent::provider::Provider::from_env();
    Json(serde_json::json!({
        "native": state.config.native_engine,
        "active": active_provider.name,
        "active_model": active_provider.model,
        "providers": providers,
    }))
}
