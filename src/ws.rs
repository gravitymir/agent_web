//! WebSocket handler: attaches a browser to a per-session [`SessionKeeper`].
//!
//! Protocol (all frames are JSON text):
//!
//! Client -> server:
//!   { "type": "send", "session_id": "<uuid|null>", "text": "...", "model": "opus", "new_chat": false }
//!   { "type": "attach", "session_id": "<uuid>" }   // (re)connect to a live session
//!   { "type": "interrupt" }
//!   { "type": "permission_response", "request_id": "...", "allow": true,
//!     "answers": { "question text": "chosen label" } }   // AskUserQuestion only
//!   { "type": "permission_response", "request_id": "...", "allow": true,
//!     "response": "freeform text" }   // AskUserQuestion "none of these" fallback,
//!     mutually exclusive with "answers" — replaces the whole structured answer
//!
//! Server -> client:
//!   raw Claude `stream-json` events (objects WITHOUT a `cwi` field), plus
//!   control frames carrying a `cwi` field:
//!   { "cwi": "session", "session_id": "...", "replay": true|false }
//!   { "cwi": "user", "text": "..." }     a user turn (echoed for every viewer)
//!   { "cwi": "exit" }                    the Claude process finished
//!   { "cwi": "no_session" }              nothing live to attach to
//!   { "cwi": "error", "message": "..." }
//!   { "cwi": "permission_request", "request_id": "...", "tool_name": "...", "input": {...} }
//!     a tool the caps panel doesn't auto-approve (or any `AskUserQuestion`) —
//!     answer with a "permission_response" client frame
//!   { "cwi": "permission_resolved", "request_id": "..." }
//!     the request above got an answer (from this viewer or another one)

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, Utf8Bytes, WebSocket};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;

use crate::session::{AttachGuard, ImageData, SessionKeeper};
use crate::AppState;

type WsSink = SplitSink<WebSocket, Message>;

/// Sliding-window rate limiter for a single connection. Generous enough never to
/// bite normal use (the composer already locks during a turn), but caps a client
/// that floods `send`/`interrupt` frames.
struct RateLimiter {
    hits: VecDeque<Instant>,
    max: usize,
    window: Duration,
}

impl RateLimiter {
    fn new() -> Self {
        Self { hits: VecDeque::new(), max: 30, window: Duration::from_secs(10) }
    }

    /// Record a request; return `false` if it exceeds the window budget.
    fn allow(&mut self) -> bool {
        let now = Instant::now();
        while let Some(&front) = self.hits.front() {
            if now.duration_since(front) > self.window {
                self.hits.pop_front();
            } else {
                break;
            }
        }
        if self.hits.len() >= self.max {
            return false;
        }
        self.hits.push_back(now);
        true
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Send {
        #[serde(default)]
        session_id: Option<String>,
        text: String,
        #[serde(default)]
        model: Option<String>,
        /// Native engine only: which provider to use (anthropic|kimi|glm).
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        new_chat: bool,
        #[serde(default)]
        images: Vec<ImageData>,
        /// Native engine: which tool groups are enabled (defaults to all).
        #[serde(default)]
        caps: crate::agent::tools::Caps,
    },
    Attach {
        session_id: String,
    },
    Interrupt,
    /// The user's decision on a `{"cwi":"permission_request"}` — a plain tool
    /// Allow/Deny, an `AskUserQuestion` answer (`answers` populated), or a
    /// freeform reply replacing the whole question (`response` populated,
    /// mutually exclusive with `answers`). Routed to whichever keeper this
    /// connection is attached to (like `Interrupt`) — a request is only ever
    /// shown to viewers already attached to the session it came from.
    PermissionResponse {
        request_id: String,
        allow: bool,
        #[serde(default)]
        answers: Option<serde_json::Map<String, serde_json::Value>>,
        #[serde(default)]
        response: Option<String>,
        #[serde(default)]
        message: Option<String>,
    },
    Ping,
}

pub async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    let mut keeper: Option<Arc<SessionKeeper>> = None;
    let mut guard: Option<AttachGuard> = None;
    let mut rx: Option<broadcast::Receiver<String>> = None;
    let mut rl = RateLimiter::new();

    loop {
        // Once attached, multiplex keeper events and client messages; before
        // that, just wait for client messages.
        if let Some(r) = rx.as_mut() {
            tokio::select! {
                evt = r.recv() => match evt {
                    Ok(line) => {
                        if ws_tx.send(Message::Text(Utf8Bytes::from(line))).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {} // skip; client keeps up next
                    Err(broadcast::error::RecvError::Closed) => { rx = None; }
                },
                msg = ws_rx.next() => {
                    match msg {
                        Some(Ok(m)) => {
                            if !handle_client(m, &state, &mut ws_tx, &mut keeper, &mut guard, &mut rx, &mut rl).await {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            }
        } else {
            match ws_rx.next().await {
                Some(Ok(m)) => {
                    if !handle_client(m, &state, &mut ws_tx, &mut keeper, &mut guard, &mut rx, &mut rl).await {
                        break;
                    }
                }
                _ => break,
            }
        }
    }

    // On disconnect, only the viewer detaches — the keeper (and its process)
    // lives on, to be reaped later if it stays idle.
    drop(guard);
}

/// Handle one inbound client frame. Returns `false` to close the socket.
async fn handle_client(
    msg: Message,
    state: &Arc<AppState>,
    ws_tx: &mut WsSink,
    keeper: &mut Option<Arc<SessionKeeper>>,
    guard: &mut Option<AttachGuard>,
    rx: &mut Option<broadcast::Receiver<String>>,
    rl: &mut RateLimiter,
) -> bool {
    let text = match msg {
        Message::Text(t) => t.to_string(),
        Message::Close(_) => return false,
        _ => return true, // ignore ping/pong/binary
    };

    // Cap how fast a client can drive the socket (protects the keeper/process).
    if !rl.allow() {
        let _ = send_control(ws_tx, json!({ "cwi": "error", "message": "Rate limit exceeded — slow down." })).await;
        return true;
    }

    let client_msg: ClientMsg = match serde_json::from_str(&text) {
        Ok(m) => m,
        Err(e) => {
            let _ = send_control(ws_tx, json!({ "cwi": "error", "message": format!("bad client message: {e}") })).await;
            return true;
        }
    };

    match client_msg {
        ClientMsg::Send { session_id, text, model, provider, new_chat, images, caps } => {
            // Validate client-supplied identifiers before they become file paths or
            // CLI arguments (path traversal / `cmd.exe` injection).
            if let Some(id) = session_id.as_deref() {
                if !crate::ids::is_valid_session_id(id) {
                    let _ = send_control(ws_tx, json!({ "cwi": "error", "message": "invalid session id" })).await;
                    return true;
                }
            }
            let model = model.filter(|m| crate::ids::is_valid_model(m));
            let sid = session_id.clone();
            // Freeze guard: a chat that lives only in the *other* engine's store is
            // read-only until the user switches CWI_ENGINE. Viewing is a separate
            // GET; here we refuse to drive a turn on it.
            if let Some(id) = session_id.as_ref() {
                if !new_chat && chat_is_frozen(state, id) {
                    let _ = send_control(ws_tx, json!({ "cwi": "error",
                        "message": "Этот чат создан другим движком — только чтение. Переключите CWI_ENGINE, чтобы продолжить." })).await;
                    return true;
                }
            }
            // A cached keeper can be stale: `interrupt()` kills the CLI process
            // (see `SessionKeeper::interrupt`), which marks it `finished` without
            // ever clearing this connection's reference to it. Left unchecked, the
            // next `send_user_message` below would go to a keeper whose actor task
            // already exited — the channel send silently no-ops (closed receiver),
            // so the message vanishes with no error and no server-side trace.
            if keeper.as_ref().is_none_or(|k| k.is_finished()) {
                *keeper = None;
                let resume = session_id.is_some() && !new_chat;
                match state.sessions.get_or_spawn(session_id, resume, model, provider) {
                    Ok(k) => {
                        tracing::info!(session = %k.session_id, "user sent request");
                        attach(ws_tx, k, keeper, guard, rx).await;
                    }
                    Err(e) => {
                        tracing::error!(session = ?sid, "failed to start session: {e}");
                        let _ = send_control(ws_tx, json!({ "cwi": "error", "message": format!("failed to start claude: {e}") })).await;
                        return true;
                    }
                }
            }
            if let Some(k) = keeper.as_ref() {
                tracing::info!(session = %k.session_id, "agent thinking");
                k.send_user_message(text, images, caps).await;
            }
        }

        ClientMsg::Attach { session_id } => {
            if keeper.is_none() {
                tracing::info!(session = %session_id, "client attaching to session");
                match state.sessions.get(&session_id) {
                    Some(k) => attach(ws_tx, k, keeper, guard, rx).await,
                    None => {
                        tracing::warn!(session = %session_id, "no live session to attach to");
                        let _ = send_control(ws_tx, json!({ "cwi": "no_session" })).await;
                    }
                }
            }
        }

        ClientMsg::Interrupt => {
            if let Some(k) = keeper.as_ref() {
                tracing::info!(session = %k.session_id, "user interrupted");
                k.interrupt().await;
            }
        }

        ClientMsg::PermissionResponse { request_id, allow, answers, response, message } => {
            if let Some(k) = keeper.as_ref() {
                tracing::info!(session = %k.session_id, %request_id, allow, "permission response");
                k.send_permission_response(request_id, allow, answers, response, message).await;
            }
        }

        ClientMsg::Ping => {
            // Keep-alive from the browser; no response needed.
        }
    }

    true
}

/// Subscribe to a keeper: announce the session, replay scrollback, and wire up
/// the live receiver + attach guard.
async fn attach(
    ws_tx: &mut WsSink,
    k: Arc<SessionKeeper>,
    keeper: &mut Option<Arc<SessionKeeper>>,
    guard: &mut Option<AttachGuard>,
    rx: &mut Option<broadcast::Receiver<String>>,
) {
    let (snapshot, receiver) = k.subscribe();
    let replay = !snapshot.is_empty();

    let _ = send_control(ws_tx, json!({
        "cwi": "session",
        "session_id": k.session_id,
        "replay": replay,
    })).await;

    for line in snapshot {
        if ws_tx.send(Message::Text(Utf8Bytes::from(line))).await.is_err() {
            return;
        }
    }

    // Mark the end of the replayed scrollback so the client can fall back to
    // the on-disk history if the live scrollback is shorter.
    let _ = send_control(ws_tx, json!({ "cwi": "replay_end" })).await;

    *guard = Some(k.attach());
    *rx = Some(receiver);
    *keeper = Some(k);
}

/// A chat is "frozen" when it exists only in the store of the engine that is NOT
/// currently active: readable, but a turn can't be driven until the user flips
/// `CWI_ENGINE`. A chat present in both stores (or the active one) is never frozen.
fn chat_is_frozen(state: &AppState, id: &str) -> bool {
    let native_exists = crate::agent::store::path(id).exists();
    let cli_exists = state
        .config
        .session_dir()
        .join(format!("{id}.jsonl"))
        .exists();
    if state.config.native_engine {
        cli_exists && !native_exists
    } else {
        native_exists && !cli_exists
    }
}

async fn send_control(ws_tx: &mut WsSink, value: serde_json::Value) -> Result<(), ()> {
    ws_tx
        .send(Message::Text(Utf8Bytes::from(value.to_string())))
        .await
        .map_err(|_| ())
}
