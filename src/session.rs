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
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};

use crate::claude::{spawn_claude, Spawned};
use crate::config::Config;

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
    User { text: String, images: Vec<ImageData>, caps: crate::agent::tools::Caps },
    Interrupt,
}

/// A live conversation and its process, shared behind an `Arc`.
pub struct SessionKeeper {
    pub session_id: String,
    cmd_tx: mpsc::Sender<Cmd>,
    events: broadcast::Sender<String>,
    scrollback: Arc<Mutex<VecDeque<String>>>,
    finished: Arc<AtomicBool>,
    subscribers: AtomicUsize,
    idle_since: Mutex<Option<Instant>>,
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
}

impl SessionManager {
    pub fn new(config: Config, mcp: Option<Arc<crate::agent::mcp::McpClient>>) -> Arc<Self> {
        let mgr = Arc::new(Self {
            config,
            sessions: Mutex::new(HashMap::new()),
            mcp,
        });
        spawn_reaper(mgr.clone());
        mgr
    }

    /// Return the live keeper for `id`, if one exists and is still running.
    pub fn get(&self, id: &str) -> Option<Arc<SessionKeeper>> {
        let map = self.sessions.lock().unwrap();
        map.get(id).filter(|k| !k.is_finished()).cloned()
    }

    /// Get the live keeper for this session, spawning one if there isn't one (or
    /// the previous one finished). The (potentially slow) spawn happens WITHOUT
    /// holding the sessions lock, so other sessions aren't blocked.
    pub fn get_or_spawn(
        &self,
        session_id: Option<String>,
        resume: bool,
        model: Option<String>,
        provider: Option<String>,
    ) -> Result<Arc<SessionKeeper>> {
        // Fast path: return an existing live keeper (brief lock, no spawn).
        if let Some(id) = &session_id {
            let map = self.sessions.lock().unwrap();
            if let Some(k) = map.get(id) {
                if !k.is_finished() {
                    return Ok(k.clone());
                }
            }
        }

        // Build the keeper outside the lock.
        let keeper = if self.config.native_engine {
            self.build_native_keeper(session_id, model, provider)
        } else {
            self.build_cli_keeper(session_id, resume, model)?
        };
        let id = keeper.session_id.clone();

        // Insert under the lock; if someone raced us to the same id, use theirs
        // and drop ours (its actor stops when the keeper's cmd channel closes).
        let mut map = self.sessions.lock().unwrap();
        if let Some(existing) = map.get(&id) {
            if !existing.is_finished() {
                return Ok(existing.clone());
            }
        }
        map.insert(id, keeper.clone());
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
        tokio::spawn(run_native_actor(
            engine,
            cmd_rx,
            events.clone(),
            scrollback.clone(),
            finished.clone(),
        ));
        Arc::new(SessionKeeper {
            session_id: id,
            cmd_tx,
            events,
            scrollback,
            finished,
            subscribers: AtomicUsize::new(0),
            idle_since: Mutex::new(Some(Instant::now())),
        })
    }

    /// CLI keeper: spawns the `claude` child process and pumps its stdout.
    fn build_cli_keeper(
        &self,
        session_id: Option<String>,
        resume: bool,
        model: Option<String>,
    ) -> Result<Arc<SessionKeeper>> {
        let Spawned { child, stdin, stdout, session_id: id } =
            spawn_claude(&self.config, session_id, resume, model)?;
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(64);
        let (events, _) = broadcast::channel::<String>(BROADCAST_CAP);
        let scrollback = Arc::new(Mutex::new(VecDeque::new()));
        let finished = Arc::new(AtomicBool::new(false));
        tokio::spawn(run_actor(
            child,
            stdin,
            stdout,
            cmd_rx,
            events.clone(),
            scrollback.clone(),
            finished.clone(),
        ));
        Ok(Arc::new(SessionKeeper {
            session_id: id,
            cmd_tx,
            events,
            scrollback,
            finished,
            subscribers: AtomicUsize::new(0),
            idle_since: Mutex::new(Some(Instant::now())),
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
fn emit(scrollback: &Arc<Mutex<VecDeque<String>>>, events: &broadcast::Sender<String>, line: String) {
    let mut sb = scrollback.lock().unwrap();
    sb.push_back(line.clone());
    while sb.len() > SCROLLBACK_MAX {
        sb.pop_front();
    }
    let _ = events.send(line);
}

/// The keeper's background task: owns the process, pumps stdout to viewers, and
/// applies commands. Runs until the process exits or all handles are dropped.
async fn run_actor(
    mut child: tokio::process::Child,
    mut stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    events: broadcast::Sender<String>,
    scrollback: Arc<Mutex<VecDeque<String>>>,
    finished: Arc<AtomicBool>,
) {
    let mut lines = BufReader::new(stdout).lines();

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::User { text, images, caps: _ }) => {
                    // (CLI keeper: caps are a native-engine concept, ignored here.)
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
                    let _ = child.start_kill();
                }
                None => break, // no more handles referencing this keeper
            },

            line = lines.next_line() => match line {
                Ok(Some(l)) if !l.trim().is_empty() => {
                    emit(&scrollback, &events, l);
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

    finished.store(true, Ordering::Release);
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
) {
    // Emit closure: push to scrollback + broadcast, exactly-once like `emit`.
    let sb = scrollback.clone();
    let ev = events.clone();
    let emitter = crate::agent::Emit::new(Arc::new(move |line: String| {
        emit(&sb, &ev, line);
    }));
    let interrupt = Arc::new(AtomicBool::new(false));

    let session_id = engine.session_id.clone();

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Cmd::User { text, images, caps } => {
                emitter.line(
                    json!({ "cwi": "user", "text": text, "images": images }).to_string(),
                );
                interrupt.store(false, Ordering::SeqCst);
                tracing::info!(session = %session_id, "agent thinking");
                let fut = engine.run_turn(text, images, caps, &emitter, &interrupt);
                tokio::pin!(fut);
                // Run the turn while still watching for interrupts / disconnect.
                loop {
                    tokio::select! {
                        _ = &mut fut => {
                            tracing::info!(session = %session_id, "agent answered");
                            break;
                        }
                        c = cmd_rx.recv() => match c {
                            Some(Cmd::Interrupt) => {
                                tracing::info!(session = %session_id, "user interrupted");
                                interrupt.store(true, Ordering::SeqCst);
                            }
                            Some(Cmd::User { .. }) => {} // ignore prompts mid-turn
                            None => interrupt.store(true, Ordering::SeqCst),
                        },
                    }
                }
            }
            Cmd::Interrupt => {} // nothing running
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
