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

use crate::AppState;
use crate::session::{AttachGuard, ImageData, ROOM_CAP, Role, SessionKeeper};

type WsSink = SplitSink<WebSocket, Message>;

/// Process-wide source of per-connection ids — the key each socket uses in its
/// session room (presence + who drives). Monotonic; wraps are a non-issue.
static CONN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
fn next_conn_id() -> u64 {
    CONN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Process-wide counter for auto-assigned "Гость N" display names.
static GUEST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The connection's display name for the chat: the one it set (via
/// `set_identity`, already sanitized) or an auto "Гость N" assigned once and
/// cached, so it stays stable across this connection's messages.
fn resolve_name(state: &AppState, conn: &mut Conn) -> String {
    if let Some(n) = &conn.name
        && !n.is_empty()
    {
        return n.clone();
    }
    // Auto name — claimed through the hub like a chosen one, so it's unique too.
    let wanted = format!(
        "Гость {}",
        GUEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let name = state.party.claim_name(conn.id, &wanted);
    conn.name = Some(name.clone());
    name
}

/// Per-connection room identity, carried for the socket's lifetime. `country`
/// comes from Cloudflare's `CF-IPCountry` (never the raw IP); `name` is the
/// display name the client sets via `set_identity` (empty → auto "Гость N").
struct Conn {
    id: u64,
    country: Option<String>,
    name: Option<String>,
}

/// A country's flag emoji from its ISO-3166 alpha-2 code (two regional-indicator
/// symbols). Unknown / placeholder codes (`XX`, Tor's `T1`, absent) fall back to
/// the pirate flag 🏴‍☠️, as requested.
fn flag(country: Option<&str>) -> String {
    let cc = country.unwrap_or("").to_ascii_uppercase();
    let ok = cc.len() == 2
        && cc.bytes().all(|b| b.is_ascii_uppercase())
        && !matches!(cc.as_str(), "XX" | "T1" | "AP");
    if ok {
        cc.chars()
            .filter_map(|c| char::from_u32(0x1F1E6 + (c as u32 - 'A' as u32)))
            .collect()
    } else {
        "🏴\u{200d}☠\u{fe0f}".to_string() // 🏴‍☠️
    }
}

/// A `{"cwi":"role",…}` frame telling one client its role and assigned name (so
/// it can show "you are …" and mark its own party-chat messages).
fn role_frame(role: Role, name: &str) -> serde_json::Value {
    json!({
        "cwi": "role",
        "role": if role == Role::Driver { "driver" } else { "observer" },
        "name": name,
    })
}

/// Broadcast the room's roster + current driver to everyone on the session, each
/// member carrying a flag emoji derived from its country.
fn broadcast_roster(k: &SessionKeeper) {
    let members: Vec<serde_json::Value> = k
        .roster()
        .iter()
        .map(|(name, is_driver, country)| {
            json!({ "name": name, "driver": is_driver, "flag": flag(country.as_deref()) })
        })
        .collect();
    k.broadcast_ephemeral(
        json!({ "cwi": "roster", "members": members, "driver": k.driver_name(), "cap": ROOM_CAP })
            .to_string(),
    );
}

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
        Self {
            hits: VecDeque::new(),
            max: 30,
            window: Duration::from_secs(10),
        }
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
    /// Control the executor VM (a host-side singleton, not tied to a chat). The
    /// server streams progress back as `{"cwi":"executor",…}` frames.
    Executor {
        /// `start` | `stop` | `drain` | `status`.
        action: String,
    },
    /// Set this connection's display name for the room (from the join screen).
    /// Sent before attaching; an empty name keeps the auto "Гость N".
    SetIdentity {
        name: String,
    },
    /// A human side-chat message — relayed to everyone in the session room, never
    /// sent to the agent. Allowed for all participants (driver and observers).
    PartyChat {
        text: String,
    },
    /// Take the wheel (become the driver) — granted only if it's free or the
    /// current driver has gone idle. Observers use this to gain agent control.
    TakeControl,
    /// Voluntarily give up the wheel so anyone can take it (the "release" button).
    ReleaseControl,
    Ping,
}

pub async fn handle_socket(socket: WebSocket, state: Arc<AppState>, country: Option<String>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    let mut conn = Conn {
        id: next_conn_id(),
        country,
        name: None,
    };
    let mut keeper: Option<Arc<SessionKeeper>> = None;
    let mut guard: Option<AttachGuard> = None;
    let mut rx: Option<broadcast::Receiver<String>> = None;
    let mut rl = RateLimiter::new();

    // Global human chat — one shared room for the whole instance, independent of
    // any agent session. Subscribe from connect (so you can chat without opening
    // a chat) and replay its history so a joining device catches up. Tell the
    // client its connection id first, so it can recognise (and right-align) its
    // own messages when they echo back through the hub.
    let _ = send_control(&mut ws_tx, json!({ "cwi": "me", "cid": conn.id })).await;
    let mut party_rx = state.party.subscribe();
    let history = state.party.history();
    if !history.is_empty() {
        let messages: Vec<serde_json::Value> = history
            .iter()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect();
        let _ = send_control(
            &mut ws_tx,
            json!({ "cwi": "party_history", "messages": messages }),
        )
        .await;
    }

    loop {
        // The session-events receiver exists only once attached; the party
        // receiver and client stream are always live.
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
                pevt = party_rx.recv() => {
                    if let Ok(line) = pevt
                        && ws_tx.send(Message::Text(Utf8Bytes::from(line))).await.is_err()
                    {
                        break;
                    }
                }
                msg = ws_rx.next() => {
                    match msg {
                        Some(Ok(m)) => {
                            if !handle_client(m, &state, &mut conn, &mut ws_tx, &mut keeper, &mut guard, &mut rx, &mut rl).await {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            }
        } else {
            tokio::select! {
                pevt = party_rx.recv() => {
                    if let Ok(line) = pevt
                        && ws_tx.send(Message::Text(Utf8Bytes::from(line))).await.is_err()
                    {
                        break;
                    }
                }
                msg = ws_rx.next() => {
                    match msg {
                        Some(Ok(m)) => {
                            if !handle_client(m, &state, &mut conn, &mut ws_tx, &mut keeper, &mut guard, &mut rx, &mut rl).await {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            }
        }
    }

    // On disconnect, only the viewer detaches — the keeper (and its process)
    // lives on, to be reaped later if it stays idle. Leave the room too so the
    // roster and the wheel (if this connection held it) free up for the rest.
    if let Some(k) = keeper.as_ref() {
        k.room_leave(conn.id);
        broadcast_roster(k);
    }
    state.party.release_name(conn.id); // free "Антон" for the next Антон
    drop(guard);
}

/// Handle one inbound client frame. Returns `false` to close the socket.
// The per-connection state (keeper/guard/rx) is threaded through as separate
// `&mut`s rather than bundled; adding the room `conn` tips it one over clippy's
// arg limit, but a wrapper struct would obscure more than it clarifies here.
#[allow(clippy::too_many_arguments)]
async fn handle_client(
    msg: Message,
    state: &Arc<AppState>,
    conn: &mut Conn,
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
        let _ = send_control(
            ws_tx,
            json!({ "cwi": "error", "message": "Rate limit exceeded — slow down." }),
        )
        .await;
        return true;
    }

    let client_msg: ClientMsg = match serde_json::from_str(&text) {
        Ok(m) => m,
        Err(e) => {
            let _ = send_control(
                ws_tx,
                json!({ "cwi": "error", "message": format!("bad client message: {e}") }),
            )
            .await;
            return true;
        }
    };

    match client_msg {
        ClientMsg::Send {
            session_id,
            text,
            model,
            provider,
            new_chat,
            images,
            caps,
        } => {
            // Validate client-supplied identifiers before they become file paths or
            // CLI arguments (path traversal / `cmd.exe` injection).
            if let Some(id) = session_id.as_deref()
                && !crate::ids::is_valid_session_id(id)
            {
                let _ = send_control(
                    ws_tx,
                    json!({ "cwi": "error", "message": "invalid session id" }),
                )
                .await;
                return true;
            }
            let model = model.filter(|m| crate::ids::is_valid_model(m));
            let sid = session_id.clone();
            // Freeze guard: a chat that lives only in the *other* engine's store is
            // read-only until the user switches CWI_ENGINE. Viewing is a separate
            // GET; here we refuse to drive a turn on it.
            if let Some(id) = session_id.as_ref()
                && !new_chat
                && chat_is_frozen(state, id)
            {
                let _ = send_control(ws_tx, json!({ "cwi": "error",
                        "message": "Этот чат создан другим движком — только чтение. Переключите CWI_ENGINE, чтобы продолжить." })).await;
                return true;
            }
            // Graceful drain: once a Drain-Stop flips the flag, we stop
            // accepting NEW turns but let in-flight ones finish. Refuse here so the
            // operator can safely `stop` once `active_turns` reaches zero.
            if state.sessions.is_draining() {
                let _ = send_control(ws_tx, json!({ "cwi": "error",
                    "message": "Server is shutting down — not accepting new requests. Please try again shortly." })).await;
                return true;
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
                match state
                    .sessions
                    .get_or_spawn(session_id, resume, model, provider)
                {
                    Ok(k) => {
                        tracing::info!(session = %k.session_id, "user sent request");
                        attach(ws_tx, k, conn, keeper, guard, rx).await;
                    }
                    Err(e) => {
                        tracing::error!(session = ?sid, "failed to start session: {e}");
                        let _ = send_control(ws_tx, json!({ "cwi": "error", "message": format!("failed to start claude: {e}") })).await;
                        return true;
                    }
                }
            }
            if let Some(k) = keeper.as_ref() {
                // On a guest instance, only the driver may drive the agent —
                // observers watch and use the side-chat. (Owner instance leaves
                // room roles unenforced, so multi-device keeps working as before.)
                if state.auth.enabled && k.room_role(conn.id) != Some(Role::Driver) {
                    let _ = send_control(ws_tx, json!({ "cwi": "error",
                        "message": "Управление агентом сейчас у другого участника. Возьмите руль, чтобы писать агенту." })).await;
                    return true;
                }
                k.room_touch(conn.id); // a real turn keeps the driver's wheel warm
                tracing::info!(session = %k.session_id, "agent thinking");
                k.send_user_message(text, images, caps).await;
            }
        }

        ClientMsg::Attach { session_id } => {
            // Same identifier guard as Send: reject anything that isn't a UUID
            // before it reaches the session store / becomes a path component.
            if !crate::ids::is_valid_session_id(&session_id) {
                let _ = send_control(
                    ws_tx,
                    json!({ "cwi": "error", "message": "invalid session id" }),
                )
                .await;
                return true;
            }
            if keeper.is_none() {
                tracing::info!(session = %session_id, "client attaching to session");
                match state.sessions.get(&session_id) {
                    Some(k) => attach(ws_tx, k, conn, keeper, guard, rx).await,
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

        ClientMsg::PermissionResponse {
            request_id,
            allow,
            answers,
            response,
            message,
        } => {
            if let Some(k) = keeper.as_ref() {
                tracing::info!(session = %k.session_id, %request_id, allow, "permission response");
                k.send_permission_response(request_id, allow, answers, response, message)
                    .await;
            }
        }

        ClientMsg::Executor { action } => {
            if !state.admin {
                let _ = send_control(
                    ws_tx,
                    exec_frame("error", "executor control is admin-only", None),
                )
                .await;
                return true;
            }
            handle_executor(&action, state, ws_tx).await;
        }

        ClientMsg::SetIdentity { name } => {
            // Set this connection's display name (sanitized — never trust raw
            // input). Empty clears it, so `resolve_name` falls back to "Гость N".
            // If already in an agent-session room, rename in the roster too.
            let clean = crate::session::sanitize_name(&name);
            if clean.is_empty() {
                conn.name = None; // → resolve_name picks a unique "Гость N"
            } else {
                // Unique among everyone connected: a second "Антон" becomes
                // "Антон 2" — nobody is refused. Tell the client what it got, so
                // it marks its own messages correctly (it may differ from what
                // was typed).
                let assigned = state.party.claim_name(conn.id, &clean);
                conn.name = Some(assigned.clone());
                let _ = send_control(
                    ws_tx,
                    json!({ "cwi": "me", "cid": conn.id, "name": assigned }),
                )
                .await;
            }
            if let Some(k) = keeper.as_ref()
                && let Some(n) = &conn.name
                && k.room_set_name(conn.id, n)
            {
                broadcast_roster(k);
            }
        }

        ClientMsg::PartyChat { text } => {
            let text = text.trim();
            // Global human chat — one shared room for the whole instance, posted
            // to the hub (stored + fanned out to everyone). Not tied to any agent
            // session, so it works from any chat or none. Never sent to the agent.
            if !text.is_empty() {
                let from = resolve_name(state, conn);
                let msg: String = text.chars().take(2000).collect();
                state.party.post(
                    json!({ "cwi": "party_chat", "cid": conn.id, "from": from,
                            "flag": flag(conn.country.as_deref()), "text": msg,
                            "ts": crate::auth::now() })
                    .to_string(),
                );
            }
        }

        ClientMsg::TakeControl => {
            if let Some(k) = keeper.as_ref() {
                if k.room_take(conn.id) {
                    let name = k.room_name(conn.id).unwrap_or_default();
                    let _ = send_control(ws_tx, role_frame(Role::Driver, &name)).await;
                    broadcast_roster(k);
                } else {
                    let _ = send_control(
                        ws_tx,
                        json!({ "cwi": "error",
                        "message": "Управление сейчас у активного участника." }),
                    )
                    .await;
                }
            }
        }

        ClientMsg::ReleaseControl => {
            if let Some(k) = keeper.as_ref()
                && k.room_release(conn.id)
            {
                let name = k.room_name(conn.id).unwrap_or_default();
                let _ = send_control(ws_tx, role_frame(Role::Observer, &name)).await;
                broadcast_roster(k);
            }
        }

        ClientMsg::Ping => {
            // Keep-alive from the browser; no response needed.
        }
    }

    true
}

// ---------------------------------------------------------------------------
// Executor VM control (host-side singleton). Streams `{"cwi":"executor",…}`
// frames to the requesting connection as each step progresses. VBoxManage/SSH
// ops are blocking, so they run on `spawn_blocking`; drain talks to the guest's
// own `agent_web` (forwarded at 127.0.0.1:GUEST_APP_PORT) for its live turn count.
// ---------------------------------------------------------------------------

fn exec_frame(state: &str, progress: &str, active: Option<usize>) -> serde_json::Value {
    json!({ "cwi": "executor", "state": state, "progress": progress, "active_turns": active })
}

/// Ask the guest's agent_web how many turns are in flight (via `/api/health`).
async fn guest_active_turns(client: &reqwest::Client, base: &str) -> Option<usize> {
    let resp = client
        .get(format!("{base}/api/health"))
        .timeout(std::time::Duration::from_secs(4))
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("active_turns")
        .and_then(|x| x.as_u64())
        .map(|n| n as usize)
}

/// Full state snapshot: VM (exists/running/ssh/snapshot) + the guest's live turns.
async fn send_executor_status(ws_tx: &mut WsSink) {
    let st = tokio::task::spawn_blocking(crate::executor::status)
        .await
        .ok();
    let (exists, running, ssh, snap) = match &st {
        Some(s) => (s.exists, s.running, s.ssh_ready, s.has_clean_snapshot),
        None => (false, false, false, false),
    };
    let active = if ssh {
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{}", crate::executor::GUEST_APP_PORT);
        guest_active_turns(&client, &base).await
    } else {
        None
    };
    let state_str = if !exists {
        "absent"
    } else if running && ssh {
        "ready"
    } else if running {
        "booting"
    } else {
        "stopped"
    };
    let _ = send_control(
        ws_tx,
        json!({
            "cwi": "executor",
            "state": state_str,
            "vm": { "exists": exists, "running": running, "ssh_ready": ssh, "clean_snapshot": snap },
            "active_turns": active,
        }),
    )
    .await;
}

async fn handle_executor(action: &str, state: &Arc<AppState>, ws_tx: &mut WsSink) {
    match action {
        "status" => send_executor_status(ws_tx).await,

        "start" => {
            let _ = send_control(
                ws_tx,
                exec_frame("booting", "восстанавливаю снапшот clean…", None),
            )
            .await;
            if !tokio::task::spawn_blocking(crate::executor::restore_clean)
                .await
                .unwrap_or(false)
            {
                let _ = send_control(
                    ws_tx,
                    exec_frame("error", "не удалось восстановить снапшот clean", None),
                )
                .await;
                return;
            }
            let _ =
                send_control(ws_tx, exec_frame("booting", "загружаюсь (headless)…", None)).await;
            let _ = tokio::task::spawn_blocking(crate::executor::start_headless).await;
            let _ = send_control(ws_tx, exec_frame("booting", "жду SSH…", None)).await;
            for _ in 0..30 {
                if tokio::task::spawn_blocking(crate::executor::ssh_ready)
                    .await
                    .unwrap_or(false)
                {
                    // The restored VM has an empty token store — push the current
                    // guest codes so magic links minted on the host work.
                    if let Some(json) = state.auth.store_json() {
                        let _ = send_control(
                            ws_tx,
                            exec_frame("booting", "синхронизирую гостевые ссылки…", None),
                        )
                        .await;
                        let _ = tokio::task::spawn_blocking(move || {
                            crate::executor::push_guest_tokens(&json)
                        })
                        .await;
                    }
                    // Undo a prior Drain-Stop: let the guest sandbox accept turns
                    // again now that its tool backend (the VM) is back up.
                    let _ = reqwest::Client::new()
                        .post(format!(
                            "http://127.0.0.1:{}/api/drain/end",
                            crate::executor::GUEST_SANDBOX_PORT
                        ))
                        .timeout(std::time::Duration::from_secs(3))
                        .send()
                        .await;
                    send_executor_status(ws_tx).await;
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            let _ = send_control(
                ws_tx,
                exec_frame("error", "SSH не поднялся за ~90с (VM ещё грузится?)", None),
            )
            .await;
        }

        "stop" => {
            let _ = send_control(ws_tx, exec_frame("stopping", "выключаю VM (ACPI)…", None)).await;
            let _ = tokio::task::spawn_blocking(crate::executor::stop_graceful).await;
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            send_executor_status(ws_tx).await;
        }

        "drain" => handle_drain(ws_tx).await,

        _ => {
            let _ = send_control(
                ws_tx,
                exec_frame("error", &format!("неизвестное действие: {action}"), None),
            )
            .await;
        }
    }
}

/// Graceful drain-stop: tell the guest to stop taking new turns, wait for its
/// in-flight agents to finish, stop its server, then power the VM off.
async fn handle_drain(ws_tx: &mut WsSink) {
    let client = reqwest::Client::new();
    // Drain the host-side guest SANDBOX (where guests actually connect and their
    // turns run) — not the VM's own agent_web — so we wait for real guest turns
    // before powering the VM (their tool backend) off. It stays drained ("server
    // stopped"); Start (Запустить) clears it.
    let base = format!("http://127.0.0.1:{}", crate::executor::GUEST_SANDBOX_PORT);

    let _ = send_control(
        ws_tx,
        exec_frame(
            "draining",
            "перевожу гостя в drain (новые ходы не принимаются)…",
            None,
        ),
    )
    .await;
    let _ = client
        .post(format!("{base}/api/drain/begin"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;

    // Wait for the guest's agents to finish (up to ~10 min). If the guest server
    // can't be reached (health poll returns None) a few times running, there's
    // nothing to wait for — the VM is down/booting — so stop waiting instead of
    // spinning the full 10 minutes on a dead endpoint.
    const MAX_UNREACHABLE: u32 = 3;
    let mut unreachable = 0u32;
    for _ in 0..120 {
        match guest_active_turns(&client, &base).await {
            Some(0) => {
                let _ = send_control(
                    ws_tx,
                    exec_frame("draining", "все агенты завершили", Some(0)),
                )
                .await;
                break;
            }
            Some(n) => {
                unreachable = 0;
                let msg = format!("жду завершения агентов: активно {n}");
                let _ = send_control(ws_tx, exec_frame("draining", &msg, Some(n))).await;
            }
            None => {
                unreachable += 1;
                if unreachable >= MAX_UNREACHABLE {
                    let _ = send_control(
                        ws_tx,
                        exec_frame(
                            "draining",
                            "гостевой сервер не отвечает — останавливаю",
                            None,
                        ),
                    )
                    .await;
                    break;
                }
                let msg = format!("гостевой сервер не отвечает ({unreachable}/{MAX_UNREACHABLE})…");
                let _ = send_control(ws_tx, exec_frame("draining", &msg, None)).await;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }

    let _ = send_control(
        ws_tx,
        exec_frame("stopping", "останавливаю приложение на VM…", None),
    )
    .await;
    let _ =
        tokio::task::spawn_blocking(|| crate::executor::ssh_run("sudo systemctl stop agent-web"))
            .await;

    let _ = send_control(ws_tx, exec_frame("stopping", "выключаю VM…", None)).await;
    let _ = tokio::task::spawn_blocking(crate::executor::stop_graceful).await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    send_executor_status(ws_tx).await;
}

/// Subscribe to a keeper: announce the session, replay scrollback, and wire up
/// the live receiver + attach guard.
async fn attach(
    ws_tx: &mut WsSink,
    k: Arc<SessionKeeper>,
    conn: &Conn,
    keeper: &mut Option<Arc<SessionKeeper>>,
    guard: &mut Option<AttachGuard>,
    rx: &mut Option<broadcast::Receiver<String>>,
) {
    let (snapshot, receiver) = k.subscribe();
    let replay = !snapshot.is_empty();

    let _ = send_control(
        ws_tx,
        json!({
            "cwi": "session",
            "session_id": k.session_id,
            "replay": replay,
        }),
    )
    .await;

    for line in snapshot {
        if ws_tx
            .send(Message::Text(Utf8Bytes::from(line)))
            .await
            .is_err()
        {
            return;
        }
    }

    // Mark the end of the replayed scrollback so the client can fall back to
    // the on-disk history if the live scrollback is shorter.
    let _ = send_control(ws_tx, json!({ "cwi": "replay_end" })).await;

    *guard = Some(k.attach());
    *rx = Some(receiver);

    // Join the session's room: first in takes the wheel, the rest observe. Tell
    // this client its role and refresh the roster for everyone. (Over cap the
    // join is refused — they still watch the stream, but as a non-member they
    // can't drive or chat until a slot frees; enforced in Send/PartyChat.)
    let role = k
        .room_join(
            conn.id,
            conn.name.as_deref().unwrap_or(""),
            conn.country.as_deref(),
        )
        .unwrap_or(Role::Observer);
    let name = k.room_name(conn.id).unwrap_or_default();
    let _ = send_control(ws_tx, role_frame(role, &name)).await;
    broadcast_roster(&k);

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
