# AGENT.md

Web interface (Rust + Axum), branded **"Agent Web"**, that drives the **Claude Code CLI** as a
subprocess and mirrors the desktop experience in a browser: live streaming chat, chat list, history,
resume. An alternative **native engine** (`CWI_ENGINE=native`) can drive the same UI by talking to
`/v1/messages` directly instead of the CLI (see **Native engine** below).

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
- `src/history.rs` reads `<claude-config-dir>/projects/<encoded-workspace>/<id>.jsonl` for the chat
  list and transcripts. The workspace path is encoded by replacing every non-alphanumeric char with
  `-` (`src/config.rs::encode_project_dir`). The config dir defaults to `~/.claude` but is
  overridable — see **Isolation** below. `list_chats`/`load_chat` **always read both** the CLI
  `.jsonl` store and the native `cwi_native/*.json` store (each `ChatSummary` carries an `engine`
  tag), so the sidebar is stable across an engine switch — see **Cross-engine chats** below.
- `DELETE /api/chats/{id}` removes a chat: it kills any live keeper
  (`SessionManager::remove`), then deletes the `.jsonl`, the native `cwi_native/<id>.json`, and any
  custom title/icon (`MetaStore::remove`). The id is validated against path traversal.
- Chat-management + rendering features live entirely in the frontend: sidebar **search** (client-side
  title filter), per-chat **export to Markdown** (built in `ui.js::transcriptToMarkdown`) and
  **delete**, **syntax highlighting** (`state.js::highlightCode` — a single-regex, language-agnostic
  tokenizer over already-escaped code), a sandboxed-iframe **HTML preview** button on HTML code
  blocks (`state.js::previewCode`), and **file attachments** (📎 / drag-drop; text files are inlined
  into the prompt via `ui.js::filesToPrompt`, images route to `pendingImages`, binaries rejected).
- Frontend is dependency-free vanilla JS as **ES modules** in `static/js/`, loaded via
  `<script type="module" src="/js/main.js">`: `state.js` (state, DOM refs, helpers, self-contained
  Markdown renderer — the leaf, no imports), `render.js` (all message/thinking/tool/scrollbar view
  code), `ws.js` (WebSocket + event dispatch), `ui.js` (composer, dictation, modal, settings, chat
  list + `init()`), `ios-icons.js` (monochrome SVG icon set + `iIcon()` helper), `main.js` (entry:
  loads modules then calls `init()`). Names are unique across modules; circular imports are fine
  (function bindings are hoisted, calls happen at runtime). `static/app.js.bak` is the pre-split
  monolith kept for rollback.
- **Boot:** a full-screen `#boot` overlay (a rotating, colour-shifting rounded-square orb) hides the
  UI while `init()` resolves the chat list, active engine, and last-open chat behind the scenes; then
  it fades out and the UI reveals in one staggered pass (`body.booted`), so the empty state never
  flashes. `init()` is `async` with a safety-timeout fallback so a hung fetch can't leave the app
  hidden. **Never animate `transform` on `#sidebar`** in the reveal — it owns `translateX` for its
  open/close slide, and a persisting `transform` jams it open.
- **Favicon** (`static/favicon.js`) is a canvas-drawn rounded square whose colour/animation encodes
  the chat state (idle green / thinking spins / tool pulses red / output blue); `static/favicon.svg`
  is the matching static fallback.

## Isolation from desktop/terminal Claude (`CLAUDE_CONFIG_DIR`)

Claude Code keys its on-disk sessions by working directory under `<config>/projects/<encoded-cwd>/`,
and `<config>` defaults to `~/.claude` — the **same** store the user's desktop/terminal `claude`
uses. So by default the web app and a terminal `claude` session in the same workspace show up in each
other's chat lists.

- `src/config.rs::claude_config_dir()` is the single source of truth: it returns `CLAUDE_CONFIG_DIR`
  if set (the very variable the `claude` CLI itself honors), else `~/.claude`. Everything on-disk is
  routed through it: `projects_root` + `cwi_titles.json` (`config.rs`, and `main.rs` derives the meta
  path from `projects_root.parent()`), the models cache + credentials read (`models.rs`), the native
  store `cwi_native/` (`agent/store.rs`), and `cwi_mcp.json` (`agent/mcp.rs`).
- **To fully isolate** (own chats/history, never mixing with desktop Claude), set `CLAUDE_CONFIG_DIR`
  to a dedicated dir (e.g. `~/.agent_web`). The spawned `claude` inherits the process env (`.env` is
  loaded into it by `main.rs::load_env_file`, and `claude.rs::base_command` doesn't clear env), so no
  `claude.rs` change is needed — the CLI writes its sessions into the isolated dir too.
- **Auth caveat:** relocating the config dir means the isolated dir has no saved login. Provide the
  subscription token via `CLAUDE_CODE_OAUTH_TOKEN` (from a one-time `claude setup-token`, ~1yr life).
  `models.rs::read_oauth_token` prefers that env var, then falls back to `<config-dir>/.credentials.json`.
  There is **no** built-in way to relocate only session storage while keeping the original login
  (Claude Code limitation), so the token env is the clean path for subscription + isolation.
- Leaving `CLAUDE_CONFIG_DIR` unset keeps the legacy shared behavior (`~/.claude`). Existing chats in
  the old location are not moved — they simply aren't visible from the isolated dir.

## Cross-engine chats (frozen)

The sidebar lists **all** chats — both CLI (`.jsonl`) and native (`cwi_native/`) — in **every** mode,
so switching `CWI_ENGINE` never changes what you see. But each chat can only be *driven* by the
engine that created it (different on-disk formats + auth), so a chat whose `engine` ≠ the active one
is shown **read-only ("frozen")**:

- Backend: `ChatSummary::engine` is `"cli"` or `"native"`; `list_chats`/`load_chat` read both stores
  (`main.rs` passes `agent::store::dir()` unconditionally). `ws.rs::chat_is_frozen` refuses a `send`
  to a chat that lives only in the *other* engine's store (defence-in-depth; viewing is a separate GET).
- Frontend: `state.js::chatFrozen(id)` compares the chat's engine (`state.chatEngine`) to the active
  one (`state.engineNative`, from `/api/providers`). A frozen chat gets a 🔒 lock icon in the list
  and, when opened, the whole composer input row is hidden (`composer.readonly`) with a banner
  explaining it's read-only until you switch `CWI_ENGINE`. `render.js::redecorateChatList` re-marks
  the list once the active engine resolves.

## Subscription usage / limits (`/api/usage`)

There's no documented API for Claude subscription quota, but `claude -p "/usage" --output-format
json` prints the same data the interactive `/usage` shows — the 5-hour ("session") window, weekly,
and weekly-Fable percentages plus reset times — in the envelope's `result`. It **costs nothing**
(`num_turns: 0`, zero tokens), so it's safe to poll behind a cache.

- `src/usage.rs::usage_json` spawns the CLI (like the keeper: `cmd /C claude …` on Windows), parses
  the percentages/reset lines from `result`, and adds plan/email from `claude auth status --json`.
  Cached ~20 s. `GET /api/usage` returns it; `{available:false}` in native mode (limits reflect the
  subscription the CLI engine uses, not a native provider).
- **Caveat:** the isolated `CLAUDE_CONFIG_DIR` + `setup-token` auth returns only the header line of
  `/usage` (no percentages); the primary desktop login returns the full breakdown. So `run_claude`
  **strips** `CLAUDE_CONFIG_DIR`/`CLAUDE_CODE_OAUTH_TOKEN` from the child env for these read-only meta
  queries and uses the default `~/.claude` login. This creates no chats there, so chat isolation holds.
- Frontend: `render.js::loadUsage` stores `state.usage` and re-renders. Refreshed **rarely** — at
  turn end (`finalizeTurn`), when the usage panel opens (`setUsage`), and once at startup. The token
  badge shows four lines (session % / week % / Fable % / chat tokens); the "Использование чата" panel
  shows the limits block + per-metric token rows with icons.

## Native engine (`src/agent/`, experimental)

An alternative "brain" that talks **directly to an Anthropic-compatible `/v1/messages`** endpoint
instead of shelling out to the Claude Code CLI — for full control over tools/behaviour/UI and to
swap providers (Anthropic / Kimi / GLM). Enabled with `CWI_ENGINE=native`.

- Key design: the engine **emits the exact same event frames the CLI produces** (`stream_event`,
  `assistant`, `user`, `result`, `cwi:*`), so `ws.rs`, the keeper, and the whole frontend work
  **unchanged**. `session.rs::run_native_actor` is a CLI-free keeper actor that drives the loop and
  pushes frames via the same `emit()`.
- `agent/provider.rs` — provider presets/config from `CWI_AGENT_*` env (base URL, key, model, auth
  header `x-api-key` vs `Bearer`, summarized-thinking flag).
- `agent/client.rs` — streaming SSE client for `POST /v1/messages`. Non-2xx / transport failures
  return a typed `ApiError { status, retry_after, message }` so the loop can classify + back off.
- `agent/mod.rs::Engine::run_turn` — the agent loop (model → `tool_use` → execute → `tool_result` →
  repeat), an `Accumulator` that rebuilds assistant content from stream events, `MAX_STEPS` cap.
  Thinking blocks are emitted for display but omitted from the stored round-trip (avoids signature
  requirements). Retries (up to `MAX_RETRIES`, only when nothing streamed yet) cover 429 / **529
  overloaded** / 5xx / network; `retry_delay` honors a server `Retry-After` (capped at
  `MAX_BACKOFF_SECS`), else exponential backoff.
- `agent/tools.rs` — Bash (PowerShell on Windows) / Read / Write / Edit / Glob / Grep, plus
  `WebFetch` (HTTP(S) → HTML-stripped text, with an SSRF host block) and `WebSearch` (DuckDuckGo
  HTML). File tools are sandboxed to the workspace (paths that escape are rejected), with a Bash
  timeout and output caps.
- `agent/store.rs` — own session format under `<config-dir>/cwi_native/<id>.json` (raw `messages`
  array + title/model/tokens; `<config-dir>` is `claude_config_dir()`). Separate from Claude Code's
  `.jsonl`.
- Env: `CWI_ENGINE=native`, `CWI_AGENT_PROVIDER=anthropic|kimi|glm`, `CWI_AGENT_API_KEY=...`,
  optional `CWI_AGENT_MODEL`, `CWI_AGENT_BASE_URL`, `CWI_AGENT_MAX_TOKENS`, `CWI_AGENT_THINKING`.
  Anthropic first-party needs a **console API key** (subscription OAuth returns 401 on messages).
- Native chats **are** listed in the sidebar now (unified cross-engine list; see **Cross-engine
  chats**). MCP, the web tools, and retry/rate-limit handling are implemented (see above). Still
  deferred: context compaction.

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
- Config is env-driven (`CWI_*`, see README). Default permission mode is `acceptEdits`. `CLAUDE_CONFIG_DIR`
  + `CLAUDE_CODE_OAUTH_TOKEN` isolate all on-disk state from desktop Claude — see **Isolation** above.

## Stack choices (already decided)

Vanilla JS + Axum static (not a bundler/SPA, not Askama) so the live token stream stays central
rather than fought against. WebSocket (not SSE) for bidirectional prompt/stream. Native Claude Code
sessions are reused rather than a custom DB.
