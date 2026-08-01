//! WebSocket handler: attaches a browser to a per-session [`SessionKeeper`].
//!
//! Protocol (all frames are JSON text):
//!
//! Client -> server:
//!   { "type": "send", "session_id": "<uuid|null>", "text": "...", "model": "opus", "new_chat": false }
//!   { "type": "attach", "session_id": "<uuid>" }   // (re)connect to a live session
//!   { "type": "interrupt" }
//!
//! Server -> client:
//!   raw Claude `stream-json` events (objects WITHOUT a `cwi` field), plus
//!   control frames carrying a `cwi` field:
//!   { "cwi": "session", "session_id": "...", "replay": true|false }
//!   { "cwi": "user", "text": "..." }     a user turn (echoed for every viewer)
//!   { "cwi": "exit" }                    the Claude process finished
//!   { "cwi": "no_session" }              nothing live to attach to
//!   { "cwi": "error", "message": "..." }

use std::sync::Arc;

use axum::extract::ws::{Message, Utf8Bytes, WebSocket};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;

use crate::session::{AttachGuard, SessionKeeper};
use crate::AppState;

type WsSink = SplitSink<WebSocket, Message>;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Send {
        #[serde(default)]
        session_id: Option<String>,
        text: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        new_chat: bool,
    },
    Attach {
        session_id: String,
    },
    Interrupt,
}

pub async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    let mut keeper: Option<Arc<SessionKeeper>> = None;
    let mut guard: Option<AttachGuard> = None;
    let mut rx: Option<broadcast::Receiver<String>> = None;

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
                            if !handle_client(m, &state, &mut ws_tx, &mut keeper, &mut guard, &mut rx).await {
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
                    if !handle_client(m, &state, &mut ws_tx, &mut keeper, &mut guard, &mut rx).await {
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
) -> bool {
    let text = match msg {
        Message::Text(t) => t.to_string(),
        Message::Close(_) => return false,
        _ => return true, // ignore ping/pong/binary
    };

    let client_msg: ClientMsg = match serde_json::from_str(&text) {
        Ok(m) => m,
        Err(e) => {
            let _ = send_control(ws_tx, json!({ "cwi": "error", "message": format!("bad client message: {e}") })).await;
            return true;
        }
    };

    match client_msg {
        ClientMsg::Send { session_id, text, model, new_chat } => {
            if keeper.is_none() {
                let resume = session_id.is_some() && !new_chat;
                match state.sessions.get_or_spawn(session_id, resume, model) {
                    Ok(k) => attach(ws_tx, k, keeper, guard, rx).await,
                    Err(e) => {
                        let _ = send_control(ws_tx, json!({ "cwi": "error", "message": format!("failed to start claude: {e}") })).await;
                        return true;
                    }
                }
            }
            if let Some(k) = keeper.as_ref() {
                k.send_user_message(text).await;
            }
        }

        ClientMsg::Attach { session_id } => {
            if keeper.is_none() {
                match state.sessions.get(&session_id) {
                    Some(k) => attach(ws_tx, k, keeper, guard, rx).await,
                    None => {
                        let _ = send_control(ws_tx, json!({ "cwi": "no_session" })).await;
                    }
                }
            }
        }

        ClientMsg::Interrupt => {
            if let Some(k) = keeper.as_ref() {
                k.interrupt().await;
            }
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

    *guard = Some(k.attach());
    *rx = Some(receiver);
    *keeper = Some(k);
}

async fn send_control(ws_tx: &mut WsSink, value: serde_json::Value) -> Result<(), ()> {
    ws_tx
        .send(Message::Text(Utf8Bytes::from(value.to_string())))
        .await
        .map_err(|_| ())
}
