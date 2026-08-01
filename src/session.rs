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
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};

use crate::claude::{spawn_claude, Spawned};
use crate::config::Config;

const SCROLLBACK_MAX: usize = 3000;
const BROADCAST_CAP: usize = 2048;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// Commands sent to a keeper's actor task.
enum Cmd {
    User(String),
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
        self.finished.load(Ordering::SeqCst)
    }

    /// Send a user turn to Claude (echoed into the stream by the actor).
    pub async fn send_user_message(&self, text: String) {
        let _ = self.cmd_tx.send(Cmd::User(text)).await;
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
        self.subscribers.fetch_add(1, Ordering::SeqCst);
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
        let prev = self.keeper.subscribers.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            *self.keeper.idle_since.lock().unwrap() = Some(Instant::now());
        }
    }
}

/// Owns all live sessions.
pub struct SessionManager {
    config: Config,
    sessions: Mutex<HashMap<String, Arc<SessionKeeper>>>,
}

impl SessionManager {
    pub fn new(config: Config) -> Arc<Self> {
        let mgr = Arc::new(Self {
            config,
            sessions: Mutex::new(HashMap::new()),
        });
        spawn_reaper(mgr.clone());
        mgr
    }

    /// Return the live keeper for `id`, if one exists and is still running.
    pub fn get(&self, id: &str) -> Option<Arc<SessionKeeper>> {
        let map = self.sessions.lock().unwrap();
        map.get(id).filter(|k| !k.is_finished()).cloned()
    }

    /// Get the live keeper for this session, spawning a new process if there
    /// isn't one (or the previous one has finished).
    pub fn get_or_spawn(
        &self,
        session_id: Option<String>,
        resume: bool,
        model: Option<String>,
    ) -> Result<Arc<SessionKeeper>> {
        let mut map = self.sessions.lock().unwrap();

        if let Some(id) = &session_id {
            if let Some(k) = map.get(id) {
                if !k.is_finished() {
                    return Ok(k.clone());
                }
            }
        }

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

        tokio::spawn(run_actor(
            child,
            stdin,
            stdout,
            cmd_rx,
            events.clone(),
            scrollback.clone(),
            finished.clone(),
        ));

        let keeper = Arc::new(SessionKeeper {
            session_id: id.clone(),
            cmd_tx,
            events,
            scrollback,
            finished,
            subscribers: AtomicUsize::new(0),
            idle_since: Mutex::new(Some(Instant::now())),
        });
        map.insert(id, keeper.clone());
        Ok(keeper)
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
                Some(Cmd::User(text)) => {
                    // Echo the prompt to all viewers, then feed it to Claude.
                    emit(&scrollback, &events, json!({ "cwi": "user", "text": text }).to_string());
                    let payload = json!({
                        "type": "user",
                        "message": { "role": "user", "content": text }
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

    finished.store(true, Ordering::SeqCst);
    let _ = child.start_kill();
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
                    let no_viewers = k.subscribers.load(Ordering::SeqCst) == 0;
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
