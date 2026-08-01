# CLAUDE.md

Web interface (Rust + Axum) that drives the **Claude Code CLI** as a subprocess and mirrors the
desktop experience in a browser: live streaming chat, chat list, history, resume.

## Run

```bash
cargo run          # serves http://127.0.0.1:8787
```

Requires an installed, authenticated `claude` on PATH. On Windows it ships as `claude.cmd` and is
launched via `cmd /C` (see `base_command` in `src/claude.rs`).

## How it works

- `src/claude.rs::spawn_claude` spawns `claude --print --output-format stream-json --input-format
  stream-json --include-partial-messages --verbose`. User turns → stdin as `stream-json`; events read
  from stdout line by line.
- `src/session.rs` owns the process in a **`SessionKeeper`** (one per session), decoupled from any
  WebSocket. An actor task pumps stdout to a `tokio::broadcast` channel + a bounded scrollback
  `VecDeque`, and applies commands (`User(text)` / `Interrupt`) from an mpsc. The keeper survives
  client disconnects; a reaper kills keepers idle >30 min with no viewers. `SessionManager` maps
  `session_id → Arc<SessionKeeper>`.
- `src/ws.rs` attaches a browser to a keeper: `{type:"send"}` spawns/gets the keeper then attaches;
  `{type:"attach"}` re-attaches to a live one; `attach()` replays scrollback then streams the
  broadcast. Server frames are raw Claude events plus `cwi`-tagged control frames
  (`session`+`replay`, `user`, `exit`, `no_session`, `error`).
- User prompts are **echoed by the keeper** as `{"cwi":"user"}` (not rendered locally), so every
  viewer and every replay sees prompts consistently.
- New chat → `--session-id <uuid>`; opening a past chat → `--resume <id>`.
- `src/history.rs` reads `~/.claude/projects/<encoded-workspace>/<id>.jsonl` for the chat list and
  transcripts. The workspace path is encoded by replacing every non-alphanumeric char with `-`
  (`src/config.rs::encode_project_dir`).
- Frontend is dependency-free vanilla JS in `static/` (incl. a small self-contained Markdown
  renderer in `app.js`).

## Conventions / gotchas

- The turn ends on the `result` event, NOT on stdout EOF — the process stays alive between turns in
  streaming-input mode, so `cwi:exit` only fires on process exit (interrupt/error), not on WS close.
- Closing the WebSocket only detaches a viewer; the keeper (and its `claude` process) keeps running.
- `subscribe()` snapshots scrollback + subscribes under one lock, and the actor emits (scrollback
  push + broadcast send) under the same lock, so each event is delivered exactly once.
- On `{cwi:"session", replay:true}` the frontend wipes the transcript and rebuilds from the
  scrollback that follows — so scrollback (since keeper spawn) can be shorter than full `.jsonl`
  history for very long-lived sessions; that's an accepted v1 tradeoff.
- `canonicalize` on Windows yields a `\\?\` verbatim prefix; `encode_project_dir` strips it so the
  session-dir name matches Claude Code's.
- Config is env-driven (`CWI_*`, see README). Default permission mode is `acceptEdits`.

## Stack choices (already decided)

Vanilla JS + Axum static (not a bundler/SPA, not Askama) so the live token stream stays central
rather than fought against. WebSocket (not SSE) for bidirectional prompt/stream. Native Claude Code
sessions are reused rather than a custom DB.
