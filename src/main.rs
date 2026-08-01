mod claude;
mod config;
mod history;
mod models;
mod session;
mod titles;
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

/// Shared application state.
pub struct AppState {
    pub config: Config,
    /// User-assigned chat metadata (title + icon), overrides auto-derived ones.
    pub meta: Mutex<MetaStore>,
    /// Live per-session keeper processes.
    pub sessions: Arc<SessionManager>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env();
    tracing::info!(?config, "starting claude_web_interface");
    tracing::info!("workspace: {}", config.workspace_abs().display());
    tracing::info!("session dir: {}", config.session_dir().display());

    let meta_path = config
        .projects_root
        .parent()
        .unwrap_or(&config.projects_root)
        .join("cwi_titles.json");
    let state = Arc::new(AppState {
        config: config.clone(),
        meta: Mutex::new(MetaStore::load(meta_path)),
        sessions: SessionManager::new(config.clone()),
    });

    let static_dir = std::env::var("CWI_STATIC_DIR").unwrap_or_else(|_| "static".to_string());

    let app = Router::new()
        .route("/api/chats", get(list_chats))
        .route("/api/chats/{id}", get(load_chat))
        .route("/api/chats/{id}/meta", put(set_meta))
        .route("/api/models", get(list_models))
        .route("/ws", get(ws_upgrade))
        .fallback_service(ServeDir::new(static_dir))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!("listening on http://{}", config.bind_addr);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn list_chats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut chats = history::list_chats(&state.config.session_dir());
    // Overlay user-assigned titles and icons.
    if let Ok(meta) = state.meta.lock() {
        for chat in &mut chats {
            if let Some(m) = meta.get(&chat.id) {
                if !m.title.is_empty() {
                    chat.title = m.title;
                    chat.custom_title = true;
                }
                chat.icon = m.icon;
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

async fn set_meta(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<SetMeta>,
) -> impl IntoResponse {
    match state.meta.lock() {
        Ok(mut meta) => match meta.set(id, body.title, body.icon) {
            Ok(()) => StatusCode::NO_CONTENT,
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
        },
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn load_chat(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let messages = history::load_chat(&state.config.session_dir(), &id);
    Json(messages)
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws::handle_socket(socket, state))
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
