mod agent;
mod auth;
mod banner;
mod broker;
mod claude;
mod config;
mod executor;
mod history;
mod ids;
mod mcp_guest;
mod models;
mod ratelimit;
mod session;
mod titles;
mod usage;
mod wizard;
mod ws;

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{Path, Query, State, WebSocketUpgrade},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put},
};
use serde::Deserialize;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::Config;
use crate::session::SessionManager;
use crate::titles::MetaStore;

/// Build number, appended to the crate version in the banner (`v0.1.0.NNN`).
/// Bumped by one on every release build so the launched build is visible in the
/// terminal at a glance.
pub const BUILD: &str = "022";

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
    // Static assets (HTML/CSS/JS) change on every deploy; `no-cache` makes the
    // browser revalidate (cheap 304s) and stops Cloudflare from serving a stale
    // edge copy — otherwise a UI fix isn't visible until caches expire.
    h.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache"),
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
    /// Per-session broker tokens for the executor proxy (`/broker/v1/messages`).
    pub broker: Arc<broker::Broker>,
    /// Shared HTTP client for the broker's upstream forwarding (connection reuse).
    pub http: reqwest::Client,
    /// This instance manages guests (mint magic links, control the executor VM).
    /// True on the owner's host; false on the disposable executor (which runs the
    /// same binary). Admin-only routes 403 when false, so a logged-in guest on
    /// the executor can't mint codes or drive the VM. Defaults to "the built-in
    /// gate is off" (CWI_AUTH unset ⇒ owner instance); `CWI_ADMIN` overrides.
    pub admin: bool,
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
                && let Some(dir) = exe.parent()
            {
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
    // `agent_web broker <new|list|revoke>` — manage executor session tokens.
    if argv.get(1).map(|s| s == "broker").unwrap_or(false) {
        broker::run_cli(&argv[2..]);
        return Ok(());
    }
    // `agent_web mcp-guest` — MCP stdio server whose tools run inside the executor
    // guest over SSH. The subscription `claude` CLI uses these instead of its own
    // Bash/Read/Write, so the model's hands act only in the sandbox. STDOUT is the
    // protocol channel: no banner, no wizard, no logging to stdout.
    if argv.get(1).map(|s| s == "mcp-guest").unwrap_or(false) {
        mcp_guest::run();
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
    // Admin instance? Explicit CWI_ADMIN wins; otherwise "the gate is off" means
    // this is the owner's host (guests run the executor with CWI_AUTH on).
    let admin = std::env::var("CWI_ADMIN")
        .map(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
        })
        .unwrap_or(!gate.enabled);
    if admin {
        tracing::info!("admin instance — guest magic-links and executor control enabled");
    }
    let state = Arc::new(AppState {
        config: config.clone(),
        meta: meta.clone(),
        sessions: SessionManager::new(config.clone(), mcp, meta),
        auth: gate,
        broker: Arc::new(broker::Broker::load()),
        http: reqwest::Client::new(),
        admin,
    });

    let static_dir = config.static_dir.clone();
    let sessions = state.sessions.clone(); // for graceful shutdown cleanup

    let app = Router::new()
        .route("/api/chats", get(list_chats))
        .route("/api/chats/{id}", get(load_chat).delete(delete_chat))
        .route("/api/chats/{id}/meta", put(set_meta))
        .route("/api/workspace.zip", get(download_workspace))
        .route("/api/models", get(list_models))
        .route("/api/providers", get(list_providers))
        .route("/api/session", get(auth::session_info))
        .route("/api/activity", post(auth::activity))
        .route("/api/usage", get(get_usage))
        .route("/api/links", get(auth::links_list).post(auth::links_create))
        .route(
            "/api/links/{label}",
            axum::routing::delete(auth::links_revoke),
        )
        .route("/api/drain/begin", post(drain_begin))
        .route("/api/drain/end", post(drain_end))
        .route("/api/health", get(health))
        .route("/metrics", get(metrics))
        .route("/broker/v1/messages", post(broker::messages))
        .route("/ws", get(ws_upgrade))
        .route("/login", get(auth::login_get).post(auth::login_post))
        .fallback_service(ServeDir::new(static_dir))
        // Access gate (no-op unless CWI_AUTH): guards every route above, incl. /ws.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::gate,
        ))
        .layer(axum::middleware::from_fn(add_security_headers))
        // Per-client HTTP rate limiting — outermost, so floods are rejected before
        // any work (incl. /login brute-force, which the gate wouldn't stop).
        .layer(axum::middleware::from_fn(ratelimit::limit))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // Graceful drain: a SIGUSR1 flips the flag so the WS layer refuses new turns
    // while in-flight ones finish; the operator then polls /api/health until
    // active_turns == 0 before stopping the process. Unix only — SIGUSR1 doesn't
    // exist on Windows, so this is for a Linux host (e.g. the executor VM).
    #[cfg(unix)]
    {
        let sessions = sessions.clone(); // the shutdown-cleanup handle (line above)
        tokio::spawn(async move {
            let mut sig =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
                {
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
                eprintln!(
                    "      Stop-Process -Id (Get-NetTCPConnection -LocalPort {port} -State Listen).OwningProcess -Force"
                );
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
        .with_graceful_shutdown(graceful_shutdown(sessions.clone()))
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
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
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

/// Wrap the stop signal with a drain: on the first Ctrl+C (or SIGTERM), refuse new
/// turns and wait for in-flight agent answers to finish before the server shuts
/// down — so stopping mid-answer doesn't cut the agent off. A second signal forces
/// an immediate shutdown. Resolving this future is what triggers axum's graceful
/// shutdown (then `sessions.shutdown_all()` kills the now-idle keepers).
async fn graceful_shutdown(sessions: Arc<SessionManager>) {
    shutdown_signal().await;
    if sessions.active_turns() == 0 {
        return; // nothing in flight — stop right away
    }
    sessions.set_draining(true);
    tracing::warn!(
        active_turns = sessions.active_turns(),
        "stop requested — draining: refusing new turns, waiting for in-flight answers to finish (Ctrl+C again to force)"
    );
    let drain = async {
        while sessions.active_turns() > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    };
    tokio::select! {
        _ = drain => tracing::info!("drain complete — shutting down"),
        _ = shutdown_signal() => tracing::warn!("second stop signal — forcing shutdown (in-flight answers cut off)"),
    }
}

/// Liveness/version endpoint. Also drives the guest Drain-Stop flow: `draining` reports
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

/// Enter graceful-drain mode: stop accepting new turns, let running ones finish.
/// Read progress from `/api/health` (`active_turns` → 0). Used by the host's
/// executor "Drain-Stop" flow, which POSTs this to the guest before powering off.
async fn drain_begin(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.sessions.set_draining(true);
    Json(serde_json::json!({
        "ok": true,
        "draining": true,
        "active_turns": state.sessions.active_turns(),
    }))
}

/// `POST /api/drain/end` — clear the drain flag so the guest server accepts turns
/// again. The host calls this from Start (Запустить) after the VM boots, undoing a
/// prior Drain-Stop. Same host→guest control channel as `drain/begin`.
async fn drain_end(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.sessions.set_draining(false);
    Json(serde_json::json!({ "ok": true, "draining": false }))
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
            let _ = std::fs::write(
                &path,
                serde_json::json!({"timestamp": chrono::Utc::now().to_rfc3339()}).to_string()
                    + "\n",
            );
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
    let messages = history::load_chat(&state.config.session_dir(), Some(&native_dir), &id);
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

    // Sandbox (guest): the chat's files live on the executor VM under <base>/<id>
    // (see spawn_claude's per-chat CWI_GUEST_WORKDIR). Remove that dir too so a
    // deleted chat leaves nothing behind on the VM. Fire-and-forget best-effort:
    // the id is a validated UUID (no traversal) and the VM may be down anyway.
    if state.config.sandbox && executor::running() {
        let dir = format!("{}/{}", mcp_guest::base_workdir().trim_end_matches('/'), id);
        let sid = id.clone();
        tokio::task::spawn_blocking(move || {
            if !executor::ssh_run(&format!("rm -rf {}", mcp_guest::sh(&dir))) {
                tracing::warn!(session = %sid, "delete_chat: guest workdir cleanup failed");
            }
        });
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
        Err(e) => {
            tracing::warn!(session = %id, "delete_chat: remove {} failed: {e}", path.display())
        }
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
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    // The authority ("host:port") of the Origin URL.
    let authority = match origin.split_once("://") {
        Some((_, rest)) => rest.split('/').next().unwrap_or(rest),
        None => return false,
    };
    // Same-origin: the Origin's authority equals the Host header we answered on.
    if let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        && authority.eq_ignore_ascii_case(host)
    {
        return true;
    }
    // Otherwise only a loopback host is allowed.
    let host_only = authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority);
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

#[derive(Deserialize)]
struct WorkspaceQuery {
    /// Chat/session id — required on the guest instance to pick the per-chat dir.
    chat: Option<String>,
}

/// Download the current chat's workspace. How a guest (no shell, git, or other
/// file export) gets their work out.
///
/// Two sources, because the files live in different places:
/// - **Guest (sandbox):** the model's files are on the executor VM, written by
///   mcp-guest over SSH under `<base>/<session-id>` — NOT in the local workspace.
///   Stream that per-chat subdir as a `tar.gz` piped over SSH.
/// - **Owner:** the local `workspace/` dir, zipped in-memory (shared across chats;
///   per-chat would require moving Claude's cwd/session storage).
///
/// The chat id is validated as a UUID, so it can't traverse out of the base dir.
async fn download_workspace(
    State(state): State<Arc<AppState>>,
    Query(q): Query<WorkspaceQuery>,
) -> axum::response::Response {
    if state.config.sandbox {
        let Some(id) = q.chat.filter(|s| ids::is_valid_session_id(s)) else {
            return (StatusCode::BAD_REQUEST, "a valid chat id is required").into_response();
        };
        if !executor::running() {
            return (StatusCode::SERVICE_UNAVAILABLE, "guest VM is not running").into_response();
        }
        let base = mcp_guest::base_workdir();
        // `tar -C <base> <id>`: archive only this chat's subdir, with paths inside
        // relative to it. Streamed as bytes so the gzip payload isn't corrupted.
        let remote = format!(
            "tar czf - -C {} {}",
            mcp_guest::sh(base.trim_end_matches('/')),
            mcp_guest::sh(&id),
        );
        let fname = format!("workspace-{id}.tar.gz");
        return match tokio::task::spawn_blocking(move || executor::ssh_capture_raw(&remote, None))
            .await
        {
            Ok((0, bytes, _)) if !bytes.is_empty() => (
                [
                    (
                        axum::http::header::CONTENT_TYPE,
                        "application/gzip".to_string(),
                    ),
                    (
                        axum::http::header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{fname}\""),
                    ),
                ],
                bytes,
            )
                .into_response(),
            Ok((code, _, err)) => {
                tracing::warn!("guest workspace tar failed (exit {code}): {}", err.trim());
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to archive guest workspace",
                )
                    .into_response()
            }
            Err(e) => {
                tracing::warn!("guest workspace tar task panicked: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to archive guest workspace",
                )
                    .into_response()
            }
        };
    }

    let dir = state.config.workspace_abs();
    match tokio::task::spawn_blocking(move || build_workspace_zip(&dir)).await {
        Ok(Ok(bytes)) => (
            [
                (axum::http::header::CONTENT_TYPE, "application/zip"),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    "attachment; filename=\"workspace.zip\"",
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::warn!("workspace zip failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build workspace archive",
            )
                .into_response()
        }
        Err(e) => {
            tracing::warn!("workspace zip task panicked: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build workspace archive",
            )
                .into_response()
        }
    }
}

/// Build an in-memory zip of everything under `dir` (relative paths, forward
/// slashes). Deflate via pure-Rust miniz_oxide.
fn build_workspace_zip(dir: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    use std::io::Write;
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for entry in walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(Result::ok)
        {
            let rel = match entry.path().strip_prefix(dir) {
                Ok(r) if !r.as_os_str().is_empty() => r,
                _ => continue,
            };
            let name = rel.to_string_lossy().replace('\\', "/");
            if entry.file_type().is_dir() {
                zip.add_directory(format!("{name}/"), opts)?;
            } else if entry.file_type().is_file() {
                zip.start_file(name, opts)?;
                zip.write_all(&std::fs::read(entry.path())?)?;
            }
        }
        zip.finish()?;
    }
    Ok(cursor.into_inner())
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
        "admin": state.admin,
        "providers": providers,
    }))
}
