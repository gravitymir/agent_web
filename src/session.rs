//! Per-session "keeper" processes, decoupled from any WebSocket connection.
//!
//! Each conversation is owned by a [`SessionKeeper`] whose background actor task
//! holds the `claude` child process. The keeper:
//!   * survives client disconnects and page reloads (the process keeps running),
//!   * fans out Claude's events to any number of attached viewers via a
//!     `broadcast` channel (multi-client / multi-device on one session),
//!   * keeps a bounded scrollback buffer so a (re)connecting client can replay
//!     the live session instead of starting from a blank screen,
//!   * echoes the user's own turns into the stream (`{"cwi":"user"}`) so every
//!     viewer — and any replay — sees prompts as well as answers.
//!
//! An idle keeper (no viewers for [`IDLE_TIMEOUT`]) is reaped.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};

use crate::claude::{Spawned, spawn_claude};
use crate::config::Config;
use crate::titles::MetaStore;

const SCROLLBACK_MAX: usize = 3000;
const BROADCAST_CAP: usize = 2048;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// A base64-encoded image pasted by the user.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageData {
    pub media_type: String,
    pub data: String,
}

/// Commands sent to a keeper's actor task.
enum Cmd {
    User {
        text: String,
        images: Vec<ImageData>,
        caps: crate::agent::tools::Caps,
    },
    Interrupt,
    /// The user's decision on a pending `control_request` from the CLI — either
    /// a plain tool-approval Allow/Deny, or an `AskUserQuestion` answer (see
    /// `run_actor`'s handling of `control_request` lines on stdout). Ignored by
    /// the native keeper, which never emits one.
    PermissionResponse {
        request_id: String,
        allow: bool,
        /// `AskUserQuestion` only: each question's text mapped to the
        /// selected label(s). Mutually exclusive with `response`.
        answers: Option<serde_json::Map<String, Value>>,
        /// `AskUserQuestion` only: a freeform reply replacing the whole
        /// structured answer set (the user typed something instead of picking
        /// options) — a distinct protocol field, not just another answer.
        /// Mutually exclusive with `answers`.
        response: Option<String>,
        /// Shown to Claude when `allow` is false.
        message: Option<String>,
    },
}

/// Max participants in one session's room (driver + observers). A guardrail on
/// fan-out to the host's uplink, not a hard technical limit.
pub const ROOM_CAP: usize = 30;

/// How many room (human) chat messages to retain and replay to a joiner, so a
/// second device sees what was said before it connected. In-memory only.
const PARTY_LOG_MAX: usize = 200;

/// After this long without real activity the driver's hold on the wheel is
/// "asleep": any observer may take control (mirrors the login seat's idle rule).
const WHEEL_IDLE: Duration = Duration::from_secs(600);

/// The global human side-chat: one shared room for the whole instance, not tied
/// to any agent session — so people can talk from any chat (or none), and a
/// joining device replays the history. In-memory (cleared on restart).
pub struct PartyHub {
    events: broadcast::Sender<String>,
    log: Mutex<VecDeque<String>>,
    /// Display name currently held by each live connection — so two people who
    /// pick the same name are told apart ("Антон", "Антон 2", …). Freed on leave.
    names: Mutex<HashMap<u64, String>>,
}

impl Default for PartyHub {
    fn default() -> Self {
        Self::new()
    }
}

impl PartyHub {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel::<String>(BROADCAST_CAP);
        Self {
            events,
            log: Mutex::new(VecDeque::new()),
            names: Mutex::new(HashMap::new()),
        }
    }

    /// Claim a display name for a connection, made unique among everyone currently
    /// connected: if "Антон" is taken by someone else, this one becomes "Антон 2"
    /// (then 3, …). Nobody is refused — they just get a numbered variant. Returns
    /// the name actually assigned. Re-claiming your own current name is a no-op.
    pub fn claim_name(&self, conn: u64, wanted: &str) -> String {
        let mut names = self.names.lock().unwrap();
        let taken =
            |n: &str, names: &HashMap<u64, String>| names.iter().any(|(c, v)| *c != conn && v == n);
        let mut name = wanted.to_string();
        let mut i = 2;
        while taken(&name, &names) {
            name = format!("{wanted} {i}");
            i += 1;
        }
        names.insert(conn, name.clone());
        name
    }

    /// The name a connection currently holds, if it claimed one.
    pub fn name_of(&self, conn: u64) -> Option<String> {
        self.names.lock().unwrap().get(&conn).cloned()
    }

    /// Release a connection's name (on disconnect) so it can be reused.
    pub fn release_name(&self, conn: u64) {
        self.names.lock().unwrap().remove(&conn);
    }

    /// Subscribe a connection to future messages.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }

    /// Store a message in the bounded log and fan it out to everyone connected.
    pub fn post(&self, line: String) {
        {
            let mut log = self.log.lock().unwrap();
            log.push_back(line.clone());
            while log.len() > PARTY_LOG_MAX {
                log.pop_front();
            }
        }
        let _ = self.events.send(line);
    }

    /// The retained history (oldest first), replayed to a joining device.
    pub fn history(&self) -> Vec<String> {
        self.log.lock().unwrap().iter().cloned().collect()
    }
}

/// A participant's role in a session room.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Controls the agent — may send turns and run the executor.
    Driver,
    /// Watches the live conversation and talks in the side-chat only.
    Observer,
}

/// Whitelist a display name to letters (Latin + Cyrillic), digits, spaces, and a
/// few safe punctuation marks — dropping everything else: emoji, combining /
/// "zalgo" marks, bidi overrides, zero-width and control characters. This is
/// server-side and authoritative — a client can bypass the join screen and send
/// a raw `set_identity`, so names are never trusted, only cleaned here. Interior
/// whitespace is collapsed and the result capped at 40 characters.
pub(crate) fn sanitize_name(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = 0usize;
    let mut prev_space = false;
    for ch in raw.trim().chars() {
        let allowed = ch.is_ascii_alphanumeric()
            // Cyrillic block, letters only — the `is_alphabetic` guard drops the
            // combining marks (U+0483–0489) that stack into zalgo text.
            || (('\u{0400}'..='\u{04FF}').contains(&ch) && ch.is_alphabetic())
            || matches!(ch, ' ' | '-' | '_' | '.' | '#' | '!' | '?');
        if !allowed {
            continue;
        }
        if ch == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        out.push(ch);
        chars += 1;
        if chars >= 40 {
            break;
        }
    }
    out.trim_end().to_string()
}

/// One connected participant (a live WebSocket) in a session room.
struct Participant {
    name: String,
    /// ISO-3166 alpha-2 country code from Cloudflare's `CF-IPCountry` header, if
    /// known. Rendered as a flag emoji by the presentation layer (pirate flag
    /// when absent). Never the raw IP — that stays server-side.
    country: Option<String>,
    /// Last real activity — drives idle takeover of the wheel.
    last_seen: Instant,
}

/// The people currently gathered on one session, and who drives the agent.
/// Presence is keyed by live connection id (from ws.rs), so it clears on
/// disconnect. The wheel (`driver`) is handed off explicitly or on idle.
#[derive(Default)]
struct Room {
    members: HashMap<u64, Participant>,
    /// Connection id holding the wheel; `None` = free for anyone to take.
    driver: Option<u64>,
    /// Counter for auto-assigned names ("Гость N").
    seq: u32,
}

/// A live conversation and its process, shared behind an `Arc`.
pub struct SessionKeeper {
    pub session_id: String,
    cmd_tx: mpsc::Sender<Cmd>,
    events: broadcast::Sender<String>,
    scrollback: Arc<Mutex<VecDeque<String>>>,
    finished: Arc<AtomicBool>,
    /// True while a turn is actively being processed (set by the actor on a user
    /// turn, cleared when it completes). Drives `SessionManager::active_turns`.
    busy: Arc<AtomicBool>,
    subscribers: AtomicUsize,
    idle_since: Mutex<Option<Instant>>,
    /// Who's gathered on this session and who drives — the "party" room.
    room: Mutex<Room>,
}

impl SessionKeeper {
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Send a user turn to Claude (echoed into the stream by the actor). `caps`
    /// gates which native-engine tools are available (ignored by the CLI keeper).
    pub async fn send_user_message(
        &self,
        text: String,
        images: Vec<ImageData>,
        caps: crate::agent::tools::Caps,
    ) {
        let _ = self.cmd_tx.send(Cmd::User { text, images, caps }).await;
    }

    /// Kill the process (ends the session; a later open re-spawns via `--resume`).
    pub async fn interrupt(&self) {
        let _ = self.cmd_tx.send(Cmd::Interrupt).await;
    }

    /// Answer a pending tool-approval / `AskUserQuestion` request (see
    /// `Cmd::PermissionResponse`).
    pub async fn send_permission_response(
        &self,
        request_id: String,
        allow: bool,
        answers: Option<serde_json::Map<String, Value>>,
        response: Option<String>,
        message: Option<String>,
    ) {
        let _ = self
            .cmd_tx
            .send(Cmd::PermissionResponse {
                request_id,
                allow,
                answers,
                response,
                message,
            })
            .await;
    }

    /// Snapshot the scrollback and subscribe to future events. Holding the
    /// scrollback lock across `subscribe` guarantees each line is delivered
    /// exactly once (either in the snapshot or via the receiver, never both).
    pub fn subscribe(&self) -> (Vec<String>, broadcast::Receiver<String>) {
        let sb = self.scrollback.lock().unwrap();
        let rx = self.events.subscribe();
        (sb.iter().cloned().collect(), rx)
    }

    /// Register a viewer; the returned guard decrements the count on drop.
    pub fn attach(self: &Arc<Self>) -> AttachGuard {
        self.subscribers.fetch_add(1, Ordering::Relaxed);
        *self.idle_since.lock().unwrap() = None;
        AttachGuard {
            keeper: self.clone(),
        }
    }

    // --- Room (party): presence + control hand-off, all per session. --------

    /// A participant joins (called when a socket attaches). Returns their role, or
    /// `None` if the room is full. The first to join takes the wheel; the rest are
    /// observers. An empty `name` gets an auto "Гость N"; `country` is an optional
    /// ISO alpha-2 code for the flag.
    pub fn room_join(&self, conn: u64, name: &str, country: Option<&str>) -> Option<Role> {
        let mut room = self.room.lock().unwrap();
        if !room.members.contains_key(&conn) && room.members.len() >= ROOM_CAP {
            return None;
        }
        let name = {
            let clean = sanitize_name(name);
            if clean.is_empty() {
                room.seq += 1;
                format!("Гость {}", room.seq)
            } else {
                clean
            }
        };
        room.members.insert(
            conn,
            Participant {
                name,
                country: country.map(|c| c.to_string()),
                last_seen: Instant::now(),
            },
        );
        if room.driver.is_none() {
            room.driver = Some(conn);
        }
        Some(if room.driver == Some(conn) {
            Role::Driver
        } else {
            Role::Observer
        })
    }

    /// A participant leaves (socket closed). Frees the wheel if they held it.
    pub fn room_leave(&self, conn: u64) {
        let mut room = self.room.lock().unwrap();
        room.members.remove(&conn);
        if room.driver == Some(conn) {
            room.driver = None;
        }
    }

    /// This connection's current role, or `None` if it isn't a member.
    pub fn room_role(&self, conn: u64) -> Option<Role> {
        let room = self.room.lock().unwrap();
        if !room.members.contains_key(&conn) {
            return None;
        }
        Some(if room.driver == Some(conn) {
            Role::Driver
        } else {
            Role::Observer
        })
    }

    /// Refresh a member's activity (keeps the wheel warm while they drive).
    pub fn room_touch(&self, conn: u64) {
        if let Some(p) = self.room.lock().unwrap().members.get_mut(&conn) {
            p.last_seen = Instant::now();
        }
    }

    /// A member's display name (for stamping their party-chat messages).
    pub fn room_name(&self, conn: u64) -> Option<String> {
        self.room
            .lock()
            .unwrap()
            .members
            .get(&conn)
            .map(|p| p.name.clone())
    }

    /// The driver voluntarily releases the wheel. Returns true if they held it.
    pub fn room_release(&self, conn: u64) -> bool {
        let mut room = self.room.lock().unwrap();
        if room.driver == Some(conn) {
            room.driver = None;
            true
        } else {
            false
        }
    }

    /// A member takes the wheel — granted only if it's free, already theirs, or
    /// the current driver has gone idle (`WHEEL_IDLE`). Returns true on success.
    pub fn room_take(&self, conn: u64) -> bool {
        let mut room = self.room.lock().unwrap();
        if !room.members.contains_key(&conn) {
            return false;
        }
        let free = match room.driver {
            None => true,
            Some(d) if d == conn => true,
            Some(d) => room
                .members
                .get(&d)
                .map(|p| p.last_seen.elapsed() >= WHEEL_IDLE)
                .unwrap_or(true),
        };
        if free {
            room.driver = Some(conn);
            if let Some(p) = room.members.get_mut(&conn) {
                p.last_seen = Instant::now();
            }
        }
        free
    }

    /// The current driver's display name, if the wheel is held.
    pub fn driver_name(&self) -> Option<String> {
        let room = self.room.lock().unwrap();
        room.driver
            .and_then(|d| room.members.get(&d).map(|p| p.name.clone()))
    }

    /// Set (rename) a member's display name after join. Returns true if they're a
    /// member. An empty name is ignored (keeps their existing/auto name).
    pub fn room_set_name(&self, conn: u64, name: &str) -> bool {
        let name = sanitize_name(name);
        if name.is_empty() {
            return self.room.lock().unwrap().members.contains_key(&conn);
        }
        match self.room.lock().unwrap().members.get_mut(&conn) {
            Some(p) => {
                p.name = name;
                true
            }
            None => false,
        }
    }

    /// Roster for presence frames: `(name, is_driver, country)` per participant,
    /// sorted by name for a stable display order.
    pub fn roster(&self) -> Vec<(String, bool, Option<String>)> {
        let room = self.room.lock().unwrap();
        let mut list: Vec<(String, bool, Option<String>)> = room
            .members
            .iter()
            .map(|(id, p)| (p.name.clone(), room.driver == Some(*id), p.country.clone()))
            .collect();
        list.sort_by(|a, b| a.0.cmp(&b.0));
        list
    }

    /// Broadcast a line to every connected socket WITHOUT persisting it to the
    /// scrollback — for ephemeral room traffic (roster/presence) that a late
    /// joiner shouldn't replay.
    pub fn broadcast_ephemeral(&self, line: String) {
        let _ = self.events.send(line);
    }
}

/// Dropping this marks the keeper idle when the last viewer leaves.
pub struct AttachGuard {
    keeper: Arc<SessionKeeper>,
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        let prev = self.keeper.subscribers.fetch_sub(1, Ordering::Relaxed);
        if prev == 1 {
            *self.keeper.idle_since.lock().unwrap() = Some(Instant::now());
        }
    }
}

/// Owns all live sessions.
pub struct SessionManager {
    config: Config,
    sessions: Mutex<HashMap<String, Arc<SessionKeeper>>>,
    /// Connected MCP servers (native engine only), shared across sessions.
    mcp: Option<Arc<crate::agent::mcp::McpClient>>,
    /// Same store as `AppState::meta` — the actor accumulates each turn's
    /// `duration_ms` here (see `track_turn_duration`), while HTTP handlers use
    /// it for title/icon. Shared so both sides see the same on-disk file.
    meta: Arc<Mutex<MetaStore>>,
    /// Graceful-drain flag: when set, no NEW turns are accepted, but in-flight
    /// turns run to completion (see `active_turns`). Toggled via SIGUSR1.
    draining: Arc<AtomicBool>,
}

impl SessionManager {
    pub fn new(
        config: Config,
        mcp: Option<Arc<crate::agent::mcp::McpClient>>,
        meta: Arc<Mutex<MetaStore>>,
    ) -> Arc<Self> {
        let mgr = Arc::new(Self {
            config,
            sessions: Mutex::new(HashMap::new()),
            mcp,
            meta,
            draining: Arc::new(AtomicBool::new(false)),
        });
        spawn_reaper(mgr.clone());
        mgr
    }

    /// Enter graceful-drain mode: refuse new turns, let running ones finish.
    pub fn set_draining(&self, v: bool) {
        self.draining.store(v, Ordering::Release);
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// How many live sessions are currently mid-turn (an agent actively working).
    pub fn active_turns(&self) -> usize {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .filter(|k| !k.is_finished() && k.busy.load(Ordering::Acquire))
            .count()
    }

    /// Return the live keeper for `id`, if one exists and is still running.
    pub fn get(&self, id: &str) -> Option<Arc<SessionKeeper>> {
        let map = self.sessions.lock().unwrap();
        map.get(id).filter(|k| !k.is_finished()).cloned()
    }

    /// Get the live keeper for this session, spawning one if there isn't one (or
    /// the previous one finished). The whole check-and-spawn runs under one lock,
    /// so two concurrent requests for the same `session_id` can't both spawn a
    /// real `claude --resume` process. Building a keeper is synchronous — the
    /// process spawn is a quick OS call and the actor runs on a spawned task — so
    /// the lock is held only briefly and doesn't block across an await.
    pub fn get_or_spawn(
        &self,
        session_id: Option<String>,
        resume: bool,
        model: Option<String>,
        provider: Option<String>,
    ) -> Result<Arc<SessionKeeper>> {
        let mut map = self.sessions.lock().unwrap();

        // Reuse a live keeper for this id if one exists.
        if let Some(id) = session_id.as_deref()
            && let Some(k) = map.get(id)
            && !k.is_finished()
        {
            return Ok(k.clone());
        }

        // Otherwise spawn one — still under the lock, so a concurrent request for
        // the same id waits here and then takes this keeper instead of spawning a
        // second process.
        let keeper = if self.config.native_engine {
            self.build_native_keeper(session_id, model, provider)
        } else {
            self.build_cli_keeper(session_id, resume, model)?
        };
        map.insert(keeper.session_id.clone(), keeper.clone());
        Ok(keeper)
    }

    /// Native keeper: a CLI-free actor drives the `/v1/messages` agent loop.
    fn build_native_keeper(
        &self,
        session_id: Option<String>,
        model: Option<String>,
        provider: Option<String>,
    ) -> Arc<SessionKeeper> {
        let id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(64);
        let (events, _) = broadcast::channel::<String>(BROADCAST_CAP);
        let scrollback = Arc::new(Mutex::new(VecDeque::new()));
        let finished = Arc::new(AtomicBool::new(false));
        let provider_obj = match provider.as_deref() {
            Some(p) if !p.is_empty() => crate::agent::provider::Provider::build(p, model.clone()),
            _ => crate::agent::provider::Provider::from_env(),
        };
        let engine = crate::agent::Engine::new(
            id.clone(),
            provider_obj,
            self.config.workspace_abs(),
            self.mcp.clone(),
        );
        let busy = Arc::new(AtomicBool::new(false));
        tokio::spawn(run_native_actor(
            engine,
            cmd_rx,
            events.clone(),
            scrollback.clone(),
            finished.clone(),
            busy.clone(),
            self.meta.clone(),
        ));
        Arc::new(SessionKeeper {
            session_id: id,
            cmd_tx,
            events,
            scrollback,
            finished,
            busy,
            subscribers: AtomicUsize::new(0),
            idle_since: Mutex::new(Some(Instant::now())),
            room: Mutex::new(Room::default()),
        })
    }

    /// CLI keeper: spawns the `claude` child process and pumps its stdout.
    fn build_cli_keeper(
        &self,
        session_id: Option<String>,
        resume: bool,
        model: Option<String>,
    ) -> Result<Arc<SessionKeeper>> {
        let Spawned {
            child,
            stdin,
            stdout,
            session_id: id,
        } = spawn_claude(&self.config, session_id, resume, model)?;
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(64);
        let (events, _) = broadcast::channel::<String>(BROADCAST_CAP);
        let scrollback = Arc::new(Mutex::new(VecDeque::new()));
        let finished = Arc::new(AtomicBool::new(false));
        let busy = Arc::new(AtomicBool::new(false));
        tokio::spawn(run_actor(
            child,
            stdin,
            stdout,
            cmd_rx,
            events.clone(),
            scrollback.clone(),
            finished.clone(),
            busy.clone(),
            id.clone(),
            self.meta.clone(),
        ));
        Ok(Arc::new(SessionKeeper {
            session_id: id,
            cmd_tx,
            events,
            scrollback,
            finished,
            busy,
            subscribers: AtomicUsize::new(0),
            idle_since: Mutex::new(Some(Instant::now())),
            room: Mutex::new(Room::default()),
        }))
    }

    /// Number of live keepers (for `/metrics`).
    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    /// Drop the live keeper for `id` (if any), interrupting its process. Used
    /// when a chat is deleted so no orphaned `claude`/native actor lingers.
    pub async fn remove(&self, id: &str) {
        let keeper = self.sessions.lock().unwrap().remove(id);
        if let Some(k) = keeper {
            k.interrupt().await;
            // Wait (bounded) for the actor to actually kill the process before the
            // caller deletes the transcript, so we don't race a still-writing
            // `claude`. `finished` flips only after the child is reaped.
            for _ in 0..30 {
                if k.is_finished() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    /// Kill every live keeper (graceful shutdown). Sends an interrupt to each
    /// (CLI keepers kill their `claude` child), then holds the keepers alive for
    /// a short grace period so the actors actually process the kill before the
    /// runtime shuts down.
    pub async fn shutdown_all(&self) {
        let keepers: Vec<Arc<SessionKeeper>> = {
            let mut map = self.sessions.lock().unwrap();
            map.drain().map(|(_, k)| k).collect()
        };
        if keepers.is_empty() {
            return;
        }
        for k in &keepers {
            k.interrupt().await;
        }
        // Let the actor tasks run the actual kill before `keepers` is dropped.
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Append a line to the scrollback and broadcast it. Sending under the
/// scrollback lock keeps [`SessionKeeper::subscribe`] exactly-once.
fn emit(
    scrollback: &Arc<Mutex<VecDeque<String>>>,
    events: &broadcast::Sender<String>,
    line: String,
) {
    let mut sb = scrollback.lock().unwrap();
    sb.push_back(line.clone());
    while sb.len() > SCROLLBACK_MAX {
        sb.pop_front();
    }
    let _ = events.send(line);
}

/// A `{"type":"control_request", "request":{"subtype":"can_use_tool",...}}`
/// line from the CLI's stdout — sent (via `--permission-prompt-tool stdio`)
/// whenever a tool needs a decision the CLI can't make on its own, including
/// `AskUserQuestion`. Without `--permission-prompt-tool stdio` these auto-deny
/// instead of ever appearing on stdout.
struct ControlRequest {
    request_id: String,
    tool_name: String,
    input: Value,
}

fn parse_control_request(line: &str) -> Option<ControlRequest> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("control_request") {
        return None;
    }
    let request_id = v.get("request_id")?.as_str()?.to_string();
    let request = v.get("request")?;
    if request.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
        return None;
    }
    let tool_name = request.get("tool_name")?.as_str()?.to_string();
    let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
    Some(ControlRequest {
        request_id,
        tool_name,
        input,
    })
}

/// If `line` is a `{"type":"result", "duration_ms":...}` frame — the one
/// per-turn event neither engine ever persists to its own on-disk store (the
/// CLI's `.jsonl` has no `result` lines at all; the native store only keeps
/// `messages`) — add its duration to the chat's running total. Used for the
/// Agentron effort metric (`Ag = H × (Tᵢ+Tₒ)/1e6`; tokens are cheap to sum
/// from history on demand, but elapsed time only ever exists in this event).
/// Cheap substring pre-filter first since this runs on every emitted line,
/// most of which are high-frequency streaming deltas, not `result` frames.
/// Watches a session's event stream and records each finished turn's
/// `(model, input, output, duration)` into the meta store. Stateful because a
/// CLI turn's model comes from the `assistant` event (`message.model`) that
/// precedes its `result`; the native engine stamps the model on the result
/// itself. This is what powers the per-model breakdown + "named" Agentron, and
/// it attributes each turn to whatever model actually answered — so switching
/// engines/models mid-chat splits the contribution automatically.
#[derive(Default)]
struct TurnTracker {
    model: String,
}

impl TurnTracker {
    fn observe(&mut self, line: &str, session_id: &str, meta: &Mutex<MetaStore>) {
        // Only `assistant` (carries the model) and `result` (carries the stats)
        // lines matter — skip the per-token delta flood without parsing it.
        let is_assistant = line.contains(r#""type":"assistant""#);
        let is_result = line.contains(r#""type":"result""#);
        if !is_assistant && !is_result {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if is_assistant {
            if let Some(m) = v.pointer("/message/model").and_then(Value::as_str)
                && !m.is_empty()
            {
                self.model = m.to_string();
            }
            return;
        }
        // result event:
        let ms = v.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
        let model = v
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.model.clone());
        let input = v
            .pointer("/usage/input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = v
            .pointer("/usage/output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if let Ok(mut m) = meta.lock() {
            let _ = m.record_turn(session_id, &model, input, output, ms);
        }
    }
}

/// Write a `control_response` line answering `request_id`. `updated_input` is
/// required by the CLI for `allow` (the original input, or the original plus
/// `AskUserQuestion`'s `answers`); `message` is shown to Claude on `deny`.
async fn write_control_response(
    stdin: &mut tokio::process::ChildStdin,
    request_id: &str,
    allow: bool,
    updated_input: Option<Value>,
    message: Option<String>,
) {
    let response = if allow {
        json!({ "behavior": "allow", "updatedInput": updated_input.unwrap_or_else(|| json!({})) })
    } else {
        json!({
            "behavior": "deny",
            "message": message.unwrap_or_else(|| "User denied this action".to_string()),
        })
    };
    let payload = json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": request_id, "response": response }
    });
    let mut line = payload.to_string();
    line.push('\n');
    let _ = stdin.write_all(line.as_bytes()).await;
    let _ = stdin.flush().await;
}

/// The keeper's background task: owns the process, pumps stdout to viewers, and
/// applies commands. Runs until the process exits or all handles are dropped.
#[allow(clippy::too_many_arguments)]
async fn run_actor(
    mut child: tokio::process::Child,
    mut stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    events: broadcast::Sender<String>,
    scrollback: Arc<Mutex<VecDeque<String>>>,
    finished: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    session_id: String,
    meta: Arc<Mutex<MetaStore>>,
) {
    let mut lines = BufReader::new(stdout).lines();
    // Caps from the most recent turn — gates which `control_request`s get
    // auto-approved (see below) vs. forwarded to the client as a real prompt.
    let mut current_caps = crate::agent::tools::Caps::default();
    // Original `input` of each forwarded (not auto-approved) control_request,
    // keyed by request_id, so answering it can echo that input back (plus
    // `AskUserQuestion`'s `answers`) without trusting the client to round-trip it.
    let mut pending_controls: HashMap<String, Value> = HashMap::new();
    // Per-turn model/token/duration attribution (see `TurnTracker`).
    let mut tracker = TurnTracker::default();

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::User { text, images, caps }) => {
                    current_caps = caps;
                    busy.store(true, Ordering::Release); // a turn is now in flight
                    // Echo the prompt (with any images) to all viewers, then feed it to Claude.
                    emit(
                        &scrollback,
                        &events,
                        json!({ "cwi": "user", "text": text, "images": images }).to_string(),
                    );
                    // Plain string when there are no images; otherwise an array of blocks.
                    let content = if images.is_empty() {
                        Value::String(text.clone())
                    } else {
                        let mut blocks: Vec<Value> = Vec::new();
                        if !text.is_empty() {
                            blocks.push(json!({ "type": "text", "text": text }));
                        }
                        for img in &images {
                            blocks.push(json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": img.media_type,
                                    "data": img.data
                                }
                            }));
                        }
                        Value::Array(blocks)
                    };
                    let payload = json!({
                        "type": "user",
                        "message": { "role": "user", "content": content }
                    });
                    let mut line = payload.to_string();
                    line.push('\n');
                    if stdin.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = stdin.flush().await;
                }
                Some(Cmd::Interrupt) => {
                    kill_process_tree(&mut child).await;
                }
                Some(Cmd::PermissionResponse { request_id, allow, answers, response, message }) => {
                    let original_input = pending_controls.remove(&request_id);
                    // Both are moved into `updated_input` below; keep copies for the
                    // broadcast so every viewer (not just whoever clicked) can show
                    // the actual outcome, not just "it's resolved".
                    let answers_json = answers.clone().map(Value::Object);
                    let response_json = response.clone();
                    let updated_input = if allow {
                        match (original_input, answers, response) {
                            (Some(Value::Object(mut obj)), _, Some(resp)) => {
                                // A freeform reply REPLACES the structured answer set
                                // entirely — a distinct protocol field, not another
                                // answer (see `Cmd::PermissionResponse`'s doc comment).
                                obj.remove("answers");
                                obj.insert("response".to_string(), Value::String(resp));
                                Some(Value::Object(obj))
                            }
                            (Some(Value::Object(mut obj)), Some(answers), None) => {
                                obj.insert("answers".to_string(), Value::Object(answers));
                                Some(Value::Object(obj))
                            }
                            (Some(input), _, _) => Some(input), // plain approval: input unchanged
                            (None, _, _) => Some(json!({})),
                        }
                    } else {
                        None
                    };
                    write_control_response(&mut stdin, &request_id, allow, updated_input, message).await;
                    emit(&scrollback, &events, json!({
                        "cwi": "permission_resolved",
                        "request_id": request_id,
                        "allow": allow,
                        "answers": answers_json,
                        "response": response_json,
                    }).to_string());
                }
                None => break, // no more handles referencing this keeper
            },

            line = lines.next_line() => match line {
                Ok(Some(l)) if !l.trim().is_empty() => {
                    match parse_control_request(&l) {
                        Some(req) if current_caps.allows(&req.tool_name) => {
                            // Auto-approved by the caps panel — answer immediately, no
                            // client round-trip, no visible prompt. Never true for
                            // `AskUserQuestion` (see `Caps::allows`) — that always
                            // falls through to the interactive branch below.
                            write_control_response(&mut stdin, &req.request_id, true, Some(req.input), None).await;
                        }
                        Some(req) => {
                            // Needs a human: the caps panel has this tool's group off,
                            // or it's an AskUserQuestion (always interactive).
                            // Stash clones before the originals move into the emit below.
                            pending_controls.insert(req.request_id.clone(), req.input.clone());
                            emit(&scrollback, &events, json!({
                                "cwi": "permission_request",
                                "request_id": req.request_id,
                                "tool_name": req.tool_name,
                                "input": req.input,
                            }).to_string());
                        }
                        None => {
                            // A `{"type":"result"}` frame ends the turn → clear busy.
                            if l.contains(r#""type":"result""#) {
                                busy.store(false, Ordering::Release);
                            }
                            tracker.observe(&l, &session_id, &meta);
                            emit(&scrollback, &events, l);
                        }
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    // Process exited.
                    emit(&scrollback, &events, json!({ "cwi": "exit" }).to_string());
                    break;
                }
            },
        }
    }

    busy.store(false, Ordering::Release); // process gone → no turn in flight
    finished.store(true, Ordering::Release);
    kill_process_tree(&mut child).await;
}

/// Kill a child process **and its descendants**. On Windows a wrapper process
/// (e.g. `cmd /C claude.cmd`, or `powershell -Command …`) means `start_kill()`
/// alone reaps only the wrapper and orphans the real work — the process would
/// keep running and Stop/timeout would appear to do nothing. `taskkill /T` kills
/// the whole tree. On Unix the child is the process itself, so `start_kill()`
/// suffices. Shared by the CLI keeper and the native Bash tool's timeout path.
pub(crate) async fn kill_process_tree(child: &mut tokio::process::Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    let _ = child.start_kill();
}

/// The native engine's background task: no child process — it drives the agent
/// loop and pushes the same event frames the CLI actor would. A turn runs while
/// the actor keeps polling for an interrupt so `Cmd::Interrupt` can stop it
/// between steps.
async fn run_native_actor(
    mut engine: crate::agent::Engine,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    events: broadcast::Sender<String>,
    scrollback: Arc<Mutex<VecDeque<String>>>,
    finished: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    meta: Arc<Mutex<MetaStore>>,
) {
    let session_id = engine.session_id.clone();

    // Emit closure: push to scrollback + broadcast, exactly-once like `emit`.
    let sb = scrollback.clone();
    let ev = events.clone();
    let sid = session_id.clone();
    let tracker = Arc::new(Mutex::new(TurnTracker::default()));
    let emitter = crate::agent::Emit::new(Arc::new(move |line: String| {
        if let Ok(mut t) = tracker.lock() {
            t.observe(&line, &sid, &meta);
        }
        emit(&sb, &ev, line);
    }));
    let interrupt = Arc::new(AtomicBool::new(false));

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Cmd::User { text, images, caps } => {
                emitter.line(json!({ "cwi": "user", "text": text, "images": images }).to_string());
                interrupt.store(false, Ordering::SeqCst);
                busy.store(true, Ordering::Release); // a turn is now in flight
                tracing::info!(session = %session_id, "agent thinking");
                let fut = engine.run_turn(text, images, caps, &emitter, &interrupt);
                tokio::pin!(fut);
                // Run the turn while still watching for interrupts / disconnect.
                loop {
                    tokio::select! {
                        _ = &mut fut => {
                            busy.store(false, Ordering::Release); // turn finished
                            tracing::info!(session = %session_id, "agent answered");
                            break;
                        }
                        c = cmd_rx.recv() => match c {
                            Some(Cmd::Interrupt) => {
                                tracing::info!(session = %session_id, "user interrupted");
                                interrupt.store(true, Ordering::SeqCst);
                            }
                            Some(Cmd::User { .. }) => {} // ignore prompts mid-turn
                            // Native engine never emits a control_request (no CLI
                            // subprocess), so nothing is ever waiting on this.
                            Some(Cmd::PermissionResponse { .. }) => {}
                            None => interrupt.store(true, Ordering::SeqCst),
                        },
                    }
                }
            }
            Cmd::Interrupt => {}                 // nothing running
            Cmd::PermissionResponse { .. } => {} // nothing running; see above
        }
    }

    finished.store(true, Ordering::Release);
}

/// Periodically kill and forget keepers that have had no viewers for a while,
/// plus any finished ones with no viewers.
fn spawn_reaper(mgr: Arc<SessionManager>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(REAP_INTERVAL).await;
            let mut to_kill: Vec<Arc<SessionKeeper>> = Vec::new();
            {
                let mut map = mgr.sessions.lock().unwrap();
                map.retain(|_, k| {
                    let no_viewers = k.subscribers.load(Ordering::Relaxed) == 0;
                    let idle_expired = k
                        .idle_since
                        .lock()
                        .unwrap()
                        .map(|t| t.elapsed() >= IDLE_TIMEOUT)
                        .unwrap_or(false);
                    let reap = no_viewers && (k.is_finished() || idle_expired);
                    if reap {
                        to_kill.push(k.clone());
                    }
                    !reap
                });
            }
            for k in to_kill {
                k.interrupt().await;
            }
        }
    });
}

#[cfg(test)]
mod control_request_tests {
    use super::parse_control_request;
    use serde_json::json;

    #[test]
    fn parses_a_can_use_tool_control_request() {
        let line = json!({
            "type": "control_request",
            "request_id": "req_1",
            "request": { "subtype": "can_use_tool", "tool_name": "Bash", "input": { "command": "ls" } },
        })
        .to_string();
        let req = parse_control_request(&line).expect("should parse");
        assert_eq!(req.request_id, "req_1");
        assert_eq!(req.tool_name, "Bash");
        assert_eq!(req.input, json!({ "command": "ls" }));
    }

    #[test]
    fn ignores_non_control_request_lines() {
        assert!(parse_control_request(r#"{"type":"assistant","message":{}}"#).is_none());
        assert!(parse_control_request("not json at all").is_none());
        // Right envelope, wrong subtype — must not be mistaken for can_use_tool.
        let other_subtype = json!({
            "type": "control_request", "request_id": "req_2",
            "request": { "subtype": "set_permission_mode" },
        })
        .to_string();
        assert!(parse_control_request(&other_subtype).is_none());
    }
}

#[cfg(test)]
mod party_hub_tests {
    use super::PartyHub;

    #[test]
    fn duplicate_names_get_numbered_and_are_freed_on_leave() {
        let hub = PartyHub::new();
        assert_eq!(hub.claim_name(1, "Антон"), "Антон");
        assert_eq!(hub.claim_name(2, "Антон"), "Антон 2"); // taken → numbered
        assert_eq!(hub.claim_name(3, "Антон"), "Антон 3");
        // Re-claiming your own name is a no-op (no self-collision).
        assert_eq!(hub.claim_name(2, "Антон 2"), "Антон 2");
        // Leaving frees the name for the next person.
        hub.release_name(1);
        assert_eq!(hub.claim_name(4, "Антон"), "Антон");
        assert_eq!(hub.name_of(4).as_deref(), Some("Антон"));
        assert_eq!(hub.name_of(1), None);
    }
}

#[cfg(test)]
mod room_tests {
    use super::*;
    use std::collections::VecDeque;

    /// A bare keeper with dummy channels — no actor task — enough to exercise the
    /// room (presence + wheel) logic in isolation.
    fn keeper() -> SessionKeeper {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Cmd>(1);
        let (events, _) = broadcast::channel::<String>(8);
        SessionKeeper {
            session_id: "t".into(),
            cmd_tx,
            events,
            scrollback: Arc::new(Mutex::new(VecDeque::new())),
            finished: Arc::new(AtomicBool::new(false)),
            busy: Arc::new(AtomicBool::new(false)),
            subscribers: AtomicUsize::new(0),
            idle_since: Mutex::new(None),
            room: Mutex::new(Room::default()),
        }
    }

    #[test]
    fn first_joiner_drives_rest_observe() {
        let k = keeper();
        assert_eq!(k.room_join(1, "Аня", Some("UA")), Some(Role::Driver));
        assert_eq!(k.room_join(2, "Макс", None), Some(Role::Observer));
        assert_eq!(k.room_role(1), Some(Role::Driver));
        assert_eq!(k.room_role(2), Some(Role::Observer));
        assert_eq!(k.room_role(99), None); // not a member
    }

    #[test]
    fn empty_name_gets_an_auto_guest_label() {
        let k = keeper();
        k.room_join(1, "   ", None);
        assert_eq!(k.room_name(1).as_deref(), Some("Гость 1"));
        // A later rename sticks; an empty rename is ignored.
        assert!(k.room_set_name(1, "Ким"));
        assert_eq!(k.room_name(1).as_deref(), Some("Ким"));
        assert!(k.room_set_name(1, "   "));
        assert_eq!(k.room_name(1).as_deref(), Some("Ким"));
    }

    #[test]
    fn room_is_capped() {
        let k = keeper();
        for i in 0..ROOM_CAP as u64 {
            assert!(k.room_join(i, "", None).is_some());
        }
        assert_eq!(k.room_join(9999, "", None), None); // full
    }

    #[test]
    fn release_and_take_hand_off_the_wheel() {
        let k = keeper();
        k.room_join(1, "d", None); // driver
        k.room_join(2, "o", None); // observer
        // An active driver holds the wheel — an observer can't grab it.
        assert!(!k.room_take(2));
        // Driver releases → the wheel is free → the observer takes it.
        assert!(k.room_release(1));
        assert!(k.room_take(2));
        assert_eq!(k.room_role(2), Some(Role::Driver));
        assert_eq!(k.room_role(1), Some(Role::Observer));
    }

    #[test]
    fn driver_leaving_frees_the_wheel() {
        let k = keeper();
        k.room_join(1, "d", None);
        k.room_join(2, "o", None);
        k.room_leave(1);
        assert_eq!(k.room_role(1), None);
        assert!(k.room_take(2), "wheel should be free after the driver left");
    }

    #[test]
    fn idle_driver_can_be_taken_over() {
        let k = keeper();
        k.room_join(1, "d", None);
        k.room_join(2, "o", None);
        assert!(!k.room_take(2), "active driver blocks takeover");
        // Age the driver's activity past the idle threshold.
        if let Some(past) =
            std::time::Instant::now().checked_sub(WHEEL_IDLE + Duration::from_secs(5))
        {
            k.room
                .lock()
                .unwrap()
                .members
                .get_mut(&1)
                .unwrap()
                .last_seen = past;
            assert!(k.room_take(2), "an idle driver can be taken over");
        }
    }

    #[test]
    fn sanitize_name_keeps_letters_digits_strips_the_rest() {
        assert_eq!(sanitize_name("Аня"), "Аня");
        assert_eq!(sanitize_name("Max_99"), "Max_99");
        assert_eq!(sanitize_name("  Ки  ро  "), "Ки ро"); // trimmed + collapsed
        // Emoji, zalgo (combining marks), and a bidi override are all dropped.
        assert_eq!(sanitize_name("Аня😀🔥"), "Аня");
        assert_eq!(sanitize_name("A\u{0301}\u{0489}B"), "AB"); // combining marks gone
        assert_eq!(sanitize_name("\u{202e}evil"), "evil"); // RTL override stripped
        // Nothing usable → empty (caller falls back to an auto name).
        assert_eq!(sanitize_name("🙂🙂"), "");
        // Capped at 40 chars.
        assert_eq!(sanitize_name(&"a".repeat(50)).chars().count(), 40);
    }

    #[test]
    fn roster_reports_names_driver_and_country() {
        let k = keeper();
        k.room_join(1, "Аня", Some("UA"));
        k.room_join(2, "Макс", None);
        let roster = k.roster();
        assert_eq!(roster.len(), 2);
        assert_eq!(k.driver_name().as_deref(), Some("Аня"));
        let driver: Vec<_> = roster
            .iter()
            .filter(|(_, d, _)| *d)
            .map(|(n, _, c)| (n.as_str(), c.as_deref()))
            .collect();
        assert_eq!(driver, vec![("Аня", Some("UA"))]);
    }
}

#[cfg(test)]
mod turn_duration_tests {
    use super::{MetaStore, TurnTracker};
    use std::sync::Mutex;

    fn temp_store(name: &str) -> Mutex<MetaStore> {
        let path = std::env::temp_dir().join(format!("cwi_session_test_{name}.json"));
        let _ = std::fs::remove_file(&path);
        Mutex::new(MetaStore::load(path))
    }

    #[test]
    fn accumulates_duration_from_a_result_line() {
        let meta = temp_store("accumulates");
        let mut t = TurnTracker::default();
        let line = r#"{"type":"result","duration_ms":1500,"num_turns":3}"#;
        t.observe(line, "sess-1", &meta);
        t.observe(line, "sess-1", &meta);
        assert_eq!(
            meta.lock().unwrap().get("sess-1").unwrap().duration_ms,
            3000
        );
    }

    #[test]
    fn ignores_lines_that_are_not_a_result_event() {
        let meta = temp_store("ignores");
        let mut t = TurnTracker::default();
        t.observe(r#"{"type":"assistant","message":{}}"#, "sess-2", &meta);
        t.observe("not json at all", "sess-2", &meta);
        // A result with no duration and no tokens must not create an entry.
        t.observe(r#"{"type":"result","num_turns":1}"#, "sess-2", &meta);
        assert!(meta.lock().unwrap().get("sess-2").is_none());
    }

    #[test]
    fn attributes_tokens_and_time_per_model() {
        let meta = temp_store("per_model");
        let mut t = TurnTracker::default();
        // Native-style: the model is stamped on the result event.
        t.observe(
            r#"{"type":"result","model":"gemini / gemini-pro-latest","usage":{"input_tokens":100,"output_tokens":20},"duration_ms":5000}"#,
            "s", &meta,
        );
        // CLI-style: the model comes from the preceding assistant event; the
        // result carries only stats. Simulates switching models mid-chat.
        t.observe(
            r#"{"type":"assistant","message":{"model":"claude-opus-4-8"}}"#,
            "s",
            &meta,
        );
        t.observe(
            r#"{"type":"result","usage":{"input_tokens":10,"output_tokens":5},"duration_ms":3000}"#,
            "s",
            &meta,
        );
        let m = meta.lock().unwrap();
        let e = m.get("s").unwrap();
        assert_eq!(e.duration_ms, 8000); // total across both models
        let g = e.models.get("gemini / gemini-pro-latest").unwrap();
        assert_eq!(
            (g.input_tokens, g.output_tokens, g.duration_ms),
            (100, 20, 5000)
        );
        let c = e.models.get("claude-opus-4-8").unwrap();
        assert_eq!(
            (c.input_tokens, c.output_tokens, c.duration_ms),
            (10, 5, 3000)
        );
    }
}
