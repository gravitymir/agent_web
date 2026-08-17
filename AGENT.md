# AGENT.md

## Agentron (Ag) — effort unit, not a speed metric

Deliberately **not** tokens/hour — that's throughput, not volume of work done. Modeled on kWh:
`1 Ag = 1 hour of active agent session × 1 million tokens`. **Two variants** are shown side by side,
differing only in whether cache re-sends count:

```
Agentron       = H × (Tᵢ + Tₒ) / 1_000_000                        # real new work only
Agentron·cache = H × (Tᵢ + Tₒ + T_cache_read + T_cache_creation) / 1_000_000   # full processed load
```

The re-sends usually dominate (~95 %+ — the whole context is re-fed each step), so `Agentron·cache`
reads far larger than the plain one on long agentic chats (a small conversation can still show a big
`·cache` figure).

Live, per-chat, in the running app — not a one-off computation:

- `Tᵢ`/`Tₒ` need no new storage: they're already summed straight from the on-disk `.jsonl`
  (`history.rs::ChatSummary.input_tokens`/`.tokens`, from each `assistant` line's **top-level**
  `usage` object — not the nested `iterations[]` breakdown some lines carry, which would
  double-count a multi-step turn). Cache tokens (`cache_read`/`cache_creation`) are tracked
  separately — excluded from the plain **Agentron**, added for **Agentron·cache**.
- `H` comes from **`duration_ms`, summed turn by turn** — not wall-clock time since chat creation
  (that would count idle gaps between sessions as "work"). The CLI's `result` event carries
  `duration_ms` for that one turn, but Claude Code never persists `result` lines to its own
  `.jsonl` (confirmed empirically: zero `"type":"result"` lines in real transcripts) — so there was
  nothing to retroactively sum. `session.rs::track_turn_duration` intercepts every line either actor
  (`run_actor` for CLI, `run_native_actor` for native — both frames are `{"type":"result",
  "duration_ms":...}`) forwards through `emit`, and on a match adds it to
  `titles.rs::MetaStore::add_duration`, persisted in `cwi_titles.json` (`ChatMeta.duration_ms`,
  cumulative) alongside title/icon. `MetaStore::set` (title/icon) preserves `duration_ms` even when
  clearing a title to blank — it only fully removes an entry when duration is *also* zero.
- `main.rs::list_chats` overlays `duration_ms` from `MetaStore` onto `ChatSummary` exactly like
  title/icon; the frontend picks it up into `state.chatUsage[id].duration_ms` alongside the existing
  token/turn fields (`ui.js::renderChatList`). `render.js::computeAgentron(durationMs, tokens)` /
  `fmtAgentron(ag)` do the actual math + formatting (tenths only; below 0.1 shows a flat `"0"`).
  Shown on the multi-line usage badge as split token rows (**tokens** = new input+output, plus a
  dimmed **tokens·cache**), a `time` row (the `H`), a `steps` row (model steps = tool-call
  round-trips, from `turns`), and the two Agentron rows (**Agentron** + dimmed **Agentron·cache**);
  the "Использование" detail panel breaks the tokens down further.
- Pre-existing chats (before this was added) simply have no `duration_ms` yet — Agentron starts
  accumulating from their next turn onward; nothing is backfilled.
- Beware noise when eyeballing `~/.claude/projects/<encoded>/`: the app's own `/api/usage` polling
  (`src/usage.rs`) spawns a throwaway `claude -p "/usage"` process per call, each logging its own
  near-empty session file with zero `usage` on any line — harmless, but there can be dozens of them.

## Context-window fill indicator

A small ring (fills clockwise, like the interactive CLI's own context gauge) showing how full the
model's context window is — distinct from Agentron/token totals, which are cumulative *sums*; this
is a point-in-time *fill level* instead.

- `history.rs::last_context_tokens` is deliberately **not** summed like `tokens`/`input_tokens` — each
  `assistant` line's usage **overwrites** a running variable instead of adding to it, so after the
  parse loop it holds only the *most recent* API call's `input + cache_read + cache_creation`. That
  figure only grows within a chat (context never shrinks), so "last" is always "current."
  `ws.js`'s `case "assistant"` updates it live, once per tool-loop step — not just at turn end, since
  a single turn can span several.
- `render.js::CONTEXT_WINDOW` is a flat `200_000`-token constant — no API exposes a model's actual
  context limit, so a model with a larger window just under-reports fill. `contextPercent`/
  `contextRing` do the math + build the ring SVG (a rotated, partially-dashed circle — the standard
  "progress ring" trick; `stroke-dashoffset` shrinks as `pct` grows).
- Shown twice: compact (14px) as a 6th badge line, and bigger (28px) with the raw numbers in the
  "Использование" panel's own "Контекст чата" section.

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
  **delete** (both live in the right **chat-actions drawer**: `ui.js::setChatActions`, badge
  `#chat-actions-badge` one slot below the gear — it replaced the floating `#chat-controls` chip that
  overlapped the transcript), **syntax highlighting** (`state.js::highlightCode` — a single-regex, language-agnostic
  tokenizer over already-escaped code), a sandboxed-iframe **HTML preview** button on HTML code
  blocks (`state.js::previewCode`), and **file attachments** (📎 / drag-drop; text files are inlined
  into the prompt via `ui.js::filesToPrompt`, images route to `pendingImages`, binaries rejected).
- Frontend is dependency-free vanilla JS as **ES modules** in `static/js/`, loaded via
  `<script type="module" src="/js/main.js">`: `state.js` (state, DOM refs, helpers, self-contained
  Markdown renderer — the leaf, no imports), `render.js` (all message/thinking/tool/scrollbar view
  code), `ws.js` (WebSocket + event dispatch), `ui.js` (composer, dictation, modal, settings, chat
  list + `init()`), `ios-icons.js` (monochrome SVG icon set + `iIcon()` helper), `files.js` (the
  "Файлы" drawer), `main.js` (entry: loads modules then calls `init()`). Names are unique across
  modules; circular imports are fine
  (function bindings are hoisted, calls happen at runtime). The pre-split monolith
  (`static/app.js.bak`) was deleted in `66e4011` — git history is the rollback path. There is no
  build step, so CI's only frontend gate is a `node --check` parse of every module (`ci.yml`, job
  `frontend`).
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

## Drawer layout (three left, three right)

Every panel is a fixed-position `aside` + its own overlay, opened by a badge that rides the panel's
edge instead of hiding under it. **Left** (bottom-up: chats, admin, files) are mutually exclusive via
`setSidebar`/`setAdminDrawer`/`setFilesDrawer`; **right** (settings at `20vh`, chat actions one slot
below at `20vh - 56px`, usage above the gear) via `setSettings`/`setChatActions`/`setUsage`. Each
setter closes the other two on its side — that is the whole coordination mechanism, so a new panel
means adding one line to each sibling.

Two gotchas worth keeping: the chat-actions badge sits *below* the gear rather than above because the
usage badge grows upward in its `.multi` form and would collide; and the badges are `display:flex`,
so any badge that can be hidden needs its own `[hidden] { display: none }` rule.

**Dictation (`ui.js`) — never iterate from `e.resultIndex`.** Per spec `results` accumulates the
whole session and `resultIndex` marks the first changed entry, so iterating from it should yield only
new words. Chrome on Android with `continuous: true` re-delivers already-final results with the index
back near 0, and iterating from it re-inserted the entire phrase on every event (the observed
"смотри / смотри ещё / смотри ещё какие-то …" pile-up). `takeFinalDelta` instead diffs against
`dictation.finalText` — the concatenation of everything already committed — and falls back to "all of
it is new" if the incoming text isn't a continuation, so an engine that restarts its result list
after a pause doesn't lose words either.

The title capsule (`.title-capsule`) is a **row**: a round reload button, then a column holding the
chat name and the guest countdown. The button is there for mobile — the transcript scrolls inside
`#messages`, never the page, so the browser's pull-to-refresh is unreachable without scrolling the
whole chat back to the top. Reloading mid-turn is safe (the keeper owns the session; the answer
replays on reconnect).

## File explorer drawer (`src/files.rs`, `static/js/files.js`)

Third left badge (topmost of chats / admin / files), opening a **read-only** explorer over the
workspace. Deliberately small in scope for a first pass — it browses and previews, nothing else.

- **Backend:** `GET /api/fs/list?path=` and `GET /api/fs/read?path=`. `path` is always
  workspace-relative (`""` = root) and goes through `agent::tools::resolve` — the *same* sandbox the
  agent's file tools use (`..` normalization + symlink guard), which is why that function is
  `pub(crate)` rather than duplicated. Listing: dirs before files, case-insensitive, capped at
  `MAX_ENTRIES` with a `truncated` flag. Read: capped at `MAX_PREVIEW_BYTES` (512 KB) via a
  `take()`-bounded read, binary sniffed by a NUL byte in the first 8 KB (reported as
  `binary: true`, no content). Both run their filesystem work on `spawn_blocking`.
- **`parent` comes from the server**, not from JS string-splitting: at the root it is `null`, which
  is what disables the UI's "up" button. The sandbox edge is expressed once, in the data.
- **Admin-only** (`state.admin`): on the disposable executor a guest would otherwise be able to walk
  that VM's filesystem. The badge/drawer carry `.admin-only[hidden]`, un-hidden by `links.js`'s
  existing `/api/providers` check — one admin probe for all three left drawers.
- **Frontend:** `files.js` owns the listing/preview; `ui.js::setFilesDrawer` owns only open/close
  (mirroring `setSidebar`/`setAdminDrawer`, all three mutually exclusive) and fires `cwi-files-open`
  on first open. The preview *replaces* the listing in the same column rather than splitting it, and
  file contents are written with `textContent` — never `innerHTML`.
- **Not implemented on purpose:** running files (the user asked to defer it), any mutation
  (create/rename/delete/upload), and browsing the guest VM's files in sandbox mode — there the
  model's files live on the executor over SSH, not in the local workspace (see `download_workspace`
  for how that case is handled today).

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
  badge shows four lines (session % / week % / Fable % / chat tokens); the "Использование" panel
  shows the limits block + per-metric token rows with icons.

## Interactive tool permissions + `AskUserQuestion` (CLI engine)

`--print` mode has no TTY to answer permission prompts, but Claude Code exposes the same decision
as a JSON control protocol over stdio when started with `--permission-prompt-tool stdio`
(`claude.rs::spawn_claude` always passes it). This is used to give the web UI a real interactive
Allow/Deny prompt — including `AskUserQuestion` — instead of the previous all-or-nothing
`--permission-mode`.

- **Wire protocol** (CLI stdout, interleaved with normal `stream-json` events):
  `{"type":"control_request","request_id":"…","request":{"subtype":"can_use_tool","tool_name":"…","input":{…}}}`.
  The reply on stdin: `{"type":"control_response","response":{"subtype":"success","request_id":"…",
  "response":{"behavior":"allow"|"deny","updatedInput":{…}}}}`. `session.rs::parse_control_request` /
  `write_control_response` are the two ends of this.
- `run_actor` tracks `current_caps` (refreshed from every `Cmd::User`'s `Caps`, sent alongside each
  turn) and a `pending_controls: HashMap<request_id, original_input>` for requests still awaiting a
  human. On each `control_request` line: if `current_caps.allows(tool_name)` it's auto-approved
  immediately (no round-trip to the browser) — otherwise the request is stashed in `pending_controls`
  and emitted to the browser as `{"cwi":"permission_request",...}`.
- This whole mechanism only sees a tool at all if Claude Code itself decides it needs a decision.
  `config.rs::Config::permission_mode` (`CWI_PERMISSION_MODE`, default `default`) **must not** be
  `acceptEdits`/`bypassPermissions` — those auto-approve Write/Edit (or everything) at the CLI level,
  so the caps panel's "Изменение файлов" toggle would silently do nothing for them (confirmed live:
  `acceptEdits` let Edit through instantly with `modify` off, no permission card). `Read`/`Glob`/`Grep`
  are a separate story — Claude Code never asks permission for those in **any** mode, so the "Чтение
  файлов" toggle is inherently a no-op for CLI-engine gating (it still filters the native engine's
  tool schema).
- `Caps::allows` (`agent/tools.rs`) hardcodes `"AskUserQuestion" => false` — unlike every other group
  (`read`/`modify`/`run`/`web_fetch`/`web_search`, all on by default and user-toggleable), there is no
  setting for it and it is **never** auto-approved: silently picking an answer to a clarifying
  question defeats the point of asking, so it always goes to the interactive card. Also fixed
  `MultiEdit`/`NotebookEdit` being ungated (now part of the `modify` group).
- The browser answers via `{"type":"permission_response","request_id","allow",
  "answers"|"response"}` (`ws.rs::ClientMsg::PermissionResponse` → `SessionKeeper::send_permission_response`
  → `Cmd::PermissionResponse`). `answers` is `{question_text: chosen_label}` (built from the
  `AskUserQuestion` option buttons); `response` is a distinct, whole-card **freeform** fallback (the
  protocol supports replacing the entire structured answer with plain text) — the two are mutually
  exclusive, `response` taking priority when both would otherwise apply. A `{"cwi":"permission_resolved",
  ...}` frame lets every viewer (not just the one who answered) mark the card resolved.
- Frontend (`render.js`): `renderPermissionRequest`/`buildToolApprovalBody`/`buildQuestionBody`/
  `markPermissionResolved`. The question card tracks an explicit `activeMode: "options"|"freeform"|null`
  rather than inferring intent from field contents — clicking an option never clears a drafted
  freeform answer (and vice versa); focusing a non-empty freeform textarea revives it back to
  "freeform" mode. Rendered regardless of `state.replayMode` — an unanswered request is live,
  actionable state that must survive a page reload, not a one-off toast.
- The settings drawer's "Инструменты и разрешения" panel (`index.html`) only lists the five
  toggleable groups (`ui.js::CAP_KEYS`) — no toggle for `AskUserQuestion`, since it's always
  interactive regardless of caps.
- The tools/permissions panel closes on **any** click outside it, anywhere on the page — not just
  within the composer — since `setToolsModal(true)` reparents it to `<body>` while open. The listener
  is on `document`, guarded against the tools-button's own click (which toggles it separately and
  already `stopPropagation()`s).

## Native engine (`src/agent/`, experimental)

An alternative "brain" that talks **directly to an Anthropic-compatible `/v1/messages`** endpoint
instead of shelling out to the Claude Code CLI — for full control over tools/behaviour/UI and to
swap providers (Anthropic / Kimi / GLM / Gemini — the last via a translation adapter, since it isn't
Anthropic-compatible; see **Gemini adapter** below). Enabled with `CWI_ENGINE=native`.

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
  `WebFetch` (HTTP(S) → HTML-stripped text) and `WebSearch` (DuckDuckGo HTML). File tools are
  sandboxed to the workspace (paths that escape are rejected), with a Bash timeout and output caps.
  **WebFetch SSRF contour** (`fetch_hop`): host blocklist → own DNS resolve that rejects the host if
  *any* address is private → `ClientBuilder::resolve` pins the connection to the verified address
  (no TOCTOU). reqwest's redirect following is OFF: the pin only covers the host its client was
  built for, so an auto-followed `302` to another name would resolve freely — each hop instead
  re-enters `fetch_hop` (`MAX_REDIRECTS` = 5, non-http(s) targets refused). Status of every
  hardening item: `docs/HARDENING.md`.
- `agent/store.rs` — own session format under `<config-dir>/cwi_native/<id>.json` (raw `messages`
  array + title/model/tokens; `<config-dir>` is `claude_config_dir()`). Separate from Claude Code's
  `.jsonl`.
- Env: `CWI_ENGINE=native`, `CWI_AGENT_PROVIDER=anthropic|kimi|glm`, `CWI_AGENT_API_KEY=...`,
  optional `CWI_AGENT_MODEL`, `CWI_AGENT_BASE_URL`, `CWI_AGENT_MAX_TOKENS`, `CWI_AGENT_THINKING`.
  Anthropic first-party needs a **console API key** (subscription OAuth returns 401 on messages).
- Native chats **are** listed in the sidebar now (unified cross-engine list; see **Cross-engine
  chats**). MCP, the web tools, and retry/rate-limit handling are implemented (see above). Still
  deferred: context compaction.

### Gemini adapter (`agent/gemini.rs`)

Kimi/GLM are just `provider.rs` presets because they **speak** `/v1/messages` — same request/response
shape as Anthropic, different URL and key. Gemini doesn't: different request body
(`contents[].parts[]`, not `messages[].content[]`), different streaming chunks (no explicit
block-start/stop events), and **no stable id** on a function call (Anthropic's `tool_use.id` /
`tool_result.tool_use_id` matching has no Gemini equivalent — it matches by name/position). Bending
`Engine::run_turn`/`Accumulator`/`store.rs` to a second wire format wasn't worth it, so instead:

- `provider::Kind` (`AnthropicMessages` | `Gemini`) tells `run_turn` which of `client::stream` /
  `gemini::stream` to call; both take/produce the same shapes (a `Value` body vs. raw
  messages+tools+system for Gemini in, a stream of already-Anthropic-shaped `Value` events out via
  `on_event`) — so `Accumulator`, tool execution, and `store.rs` need zero Gemini-awareness.
  `handle_stream_event` (the reasoning-timer + forward-to-browser + `Accumulator` bookkeeping) is one
  function shared by both branches, not duplicated per provider.
- `client::response_to_sse_events` was factored out of `client::stream` so `gemini::stream` (which
  builds its own request — different URL shape, `x-goog-api-key` header, no `anthropic-version`) can
  still reuse the exact same chunked-line SSE parser (Gemini's `alt=sse` framing is byte-identical:
  `data: {...}\n\n`).
- Request translation: our stored `tool_use`/`tool_result` blocks keep their Anthropic `id` in
  memory (needed so a `tool_result` can look up its call's `name` — Gemini's `functionResponse` is
  keyed by name, not id) but the id itself is **never sent** to Gemini. Synthesized ids
  (`gemini-call-{n}`) go the other way, purely so returned tool calls satisfy `Accumulator`'s (every
  provider's) assumption that a `tool_use` block has an id — never round-tripped back to Gemini either.
- Response translation (`gemini::Translator`): Gemini's streaming chunks carry a portion of
  `candidates[0].content.parts[]` with no block-start/stop markers, so block boundaries are inferred
  from when a part's kind (text / `thought` / `functionCall`) changes; each inferred boundary replays
  as a synthetic `content_block_start`, each chunk of text as a `content_block_delta` — exactly what
  `Accumulator::on_event` already expects from a real Anthropic stream. A `functionCall` part is
  always emitted as one complete `input_json_delta` (Gemini doesn't stream partial tool-call JSON the
  way Anthropic does). No real `message_stop`/`message_delta` exists on the wire either — `Translator
  ::finish` synthesizes one from `usageMetadata` once the SSE stream ends.
- **Least-confident spots** (Gemini's API + model lineup both churn fast — recheck here first if
  thinking or tool calls come back empty/malformed): the exact shape of a "thought" part
  (`{"thought": "…"}` vs. `{"text": "…", "thought": true}` — handled defensively, both recognized) and
  `generationConfig.thinkingConfig`'s exact field names.
- Model default is `gemini-pro-latest` (a Google "-latest" alias that auto-tracks the current
  recommended release) rather than a pinned version — Gemini model ids have been deprecated on the
  order of months, not years.
- The registry (`agent/registry.rs`) lists Gemini for the settings dropdown with a static model
  fallback — its `/v1beta/models` list endpoint returns a different shape (`models[].name`/
  `displayName`) than `models.rs::parse_models` expects (`data[].id`), so the live fetch silently
  returns empty and falls back; not fixed, since the fallback path already works fine.

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
- Config is env-driven (`CWI_*`, see README). Default permission mode is `default` — **not**
  `acceptEdits`/`bypassPermissions`, which would auto-approve at the CLI level before a decision ever
  reaches our `control_request` handler, silently defeating the caps panel for whatever they
  auto-approve (found live: with `acceptEdits`, Write/Edit executed instantly with caps' "modify"
  group off, no permission card — see **Interactive tool permissions** above). `CLAUDE_CONFIG_DIR`
  + `CLAUDE_CODE_OAUTH_TOKEN` isolate all on-disk state from desktop Claude — see **Isolation** above.
- **Send is held, not queued, across a dead connection.** `ws.send()` can look successful
  (`readyState === OPEN`) for a moment after the server process has actually died — the browser just
  hasn't noticed the socket is gone yet. Rather than auto-queue-and-resend on reconnect (tried and
  discarded: any resend risks a duplicate turn if the first one *did* land), `ui.js::submit()` leaves
  the typed text and attachments in place and locks the composer (`lockComposerForConfirmation`,
  disables `#input` + attachment remove buttons) until the send is confirmed. Confirmation is the
  keeper's own `{"cwi":"user"}` echo (`ws.js`'s `awaitingConfirmation` flag →
  `ui.js::confirmSentMessage`, which is what actually clears the composer); if the socket closes
  first, `restoreUnsentMessage` just unlocks — nothing was ever cleared, so there's nothing to
  restore. If `sendWs()` sees the socket already `CLOSED` synchronously, it fails immediately without
  ever locking — the user notices right away and can just retry once reconnected.
- **Turn-complete chime/notification** (`render.js::finalizeTurn({notify=true})`,
  `static/notify.js`) fires only for a turn that actually finished live — `notify: false` is passed
  explicitly on `{"cwi":"exit"}` (interrupt / crash / server shutting down for a restart all end a
  turn without anyone having answered anything) and the sound/notify block is also skipped whenever
  `state.replayMode` is true (a reconnect replaying old history isn't a new answer). The desktop
  notification's icon switches to a "?" glyph when the answer text ends in one — a cheap signal that
  the agent is waiting on the user specifically.

## Stack choices (already decided)

Vanilla JS + Axum static (not a bundler/SPA, not Askama) so the live token stream stays central
rather than fought against. WebSocket (not SSE) for bidirectional prompt/stream. Native Claude Code
sessions are reused rather than a custom DB.
