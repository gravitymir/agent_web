//! Native agent engine: drives an Anthropic-compatible `/v1/messages` endpoint
//! through the agent loop (model → tool_use → execute → tool_result → repeat),
//! emitting events in the SAME shape Claude Code's stream-json produces so the
//! existing frontend/WebSocket layer work unchanged.

pub mod client;
pub mod gemini;
pub mod mcp;
pub mod prompt;
pub mod provider;
pub mod registry;
pub mod store;
pub mod tools;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde_json::{Value, json};

use crate::session::ImageData;
use provider::Provider;

/// A sink for outgoing event lines (pushed to scrollback + broadcast by the keeper).
#[derive(Clone)]
pub struct Emit(Arc<dyn Fn(String) + Send + Sync>);

impl Emit {
    pub fn new(f: Arc<dyn Fn(String) + Send + Sync>) -> Self {
        Self(f)
    }
    pub fn line(&self, s: String) {
        (self.0.as_ref())(s)
    }
}

/// One live conversation driven by the native engine.
pub struct Engine {
    pub session_id: String,
    pub provider: Provider,
    pub workspace: PathBuf,
    stored: store::Stored,
    mcp: Option<Arc<mcp::McpClient>>,
    http: reqwest::Client,
    /// Which tool groups are enabled — set per turn from the client's `caps`.
    caps: tools::Caps,
}

const MAX_STEPS: u64 = 1000; // safety cap on tool loops per turn
const MAX_RETRIES: u32 = 3; // retries on 429/5xx / network, before any tokens stream
const MAX_MESSAGES: usize = 80; // context cap: keep ~this many recent messages

impl Engine {
    pub fn new(
        session_id: String,
        provider: Provider,
        workspace: PathBuf,
        mcp: Option<Arc<mcp::McpClient>>,
    ) -> Self {
        let mut stored = store::load(&session_id);
        sanitize_messages(&mut stored.messages); // repair any dangling tool_use
        Self {
            session_id,
            provider,
            workspace,
            stored,
            mcp,
            http: reqwest::Client::new(),
            caps: tools::Caps::default(),
        }
    }

    async fn save(&self) {
        store::save_async(&self.session_id, &self.stored).await;
    }

    fn user_message(&self, text: &str, images: &[ImageData]) -> Value {
        // Always use the content-block *array* form, never a bare string. Anthropic
        // / Kimi / GLM accept both, but stricter Anthropic-compatible endpoints
        // (Alibaba's Qwen/DashScope) reject string content with a 400
        // ("content should be a valid list"). The array form is universal.
        let mut blocks: Vec<Value> = Vec::new();
        if !text.is_empty() {
            blocks.push(json!({ "type": "text", "text": text }));
        }
        for img in images {
            blocks.push(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": img.media_type, "data": img.data }
            }));
        }
        json!({ "role": "user", "content": blocks })
    }

    /// Keep the conversation from growing past the model's context: drop the
    /// oldest messages, then advance to the next real user prompt so we never
    /// start mid-turn (which would dangle a tool_use without its result).
    fn compact(&mut self) {
        if self.stored.messages.len() <= MAX_MESSAGES {
            return;
        }
        let mut drop_to = self.stored.messages.len() - MAX_MESSAGES;
        while drop_to < self.stored.messages.len()
            && !is_user_prompt(&self.stored.messages[drop_to])
        {
            drop_to += 1;
        }
        if drop_to > 0 && drop_to < self.stored.messages.len() {
            self.stored.messages.drain(0..drop_to);
        }
    }

    /// Enabled built-in tools + any tools exposed by connected MCP servers —
    /// shared by both wire formats (`build_body` renames nothing further for
    /// Anthropic; `gemini::stream` renames `input_schema` -> `parameters`).
    fn tool_list(&self) -> Vec<Value> {
        let mut tool_list = tools::schemas(&self.caps);
        if let Some(m) = &self.mcp {
            tool_list.extend(m.tool_schemas());
        }
        tool_list
    }

    fn build_body(&self) -> Value {
        let mut body = json!({
            "model": self.provider.model,
            "max_tokens": self.provider.max_tokens,
            "system": prompt::system_prompt(&self.workspace),
            "messages": self.stored.messages,
            "tools": self.tool_list(),
        });
        if self.provider.thinking {
            if self.provider.auth == provider::Auth::XApiKey {
                // Anthropic first-party: adaptive thinking with a summarized display.
                body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
            } else if self.provider.max_tokens > 1024 {
                // Anthropic-*compatible* third parties (Kimi / GLM / Qwen) only
                // accept the standard enabled form; the "adaptive"/"summarized"
                // shape makes strict endpoints (Qwen/DashScope) 400 the whole
                // request (reported confusingly as a `messages` error). budget must
                // be >= 1024 and strictly < max_tokens.
                let budget = (self.provider.max_tokens / 2)
                    .max(1024)
                    .min(self.provider.max_tokens - 1);
                body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
            }
            // else (3rd-party, max_tokens <= 1024): omit — no room for a thinking budget.
        }
        body
    }

    /// Process one user turn: append the message, then loop model↔tools until the
    /// model stops requesting tools (or interrupted / step cap hit).
    pub async fn run_turn(
        &mut self,
        text: String,
        images: Vec<ImageData>,
        caps: tools::Caps,
        emit: &Emit,
        interrupt: &AtomicBool,
    ) {
        self.caps = caps; // gate which tools are advertised + executable this turn
        if !self.provider.has_key() {
            emit.line(json!({
                "cwi": "error",
                "message": "No API key set. Configure CWI_AGENT_API_KEY (and CWI_AGENT_PROVIDER)."
            }).to_string());
            emit.line(json!({ "type": "result", "num_turns": 0 }).to_string());
            return;
        }

        self.compact(); // trim old history before adding the new turn
        self.stored.messages.push(self.user_message(&text, &images));
        if self.stored.title.is_empty() {
            let t = text.trim();
            if !t.is_empty() {
                self.stored.title = t.chars().take(120).collect();
            }
        }
        self.stored.model = format!("{} / {}", self.provider.name, self.provider.model);

        let started = Instant::now();
        let mut turn_tokens = 0u64;
        let mut turn_input = 0u64;
        let mut steps = 0u64;

        tracing::info!(session = %self.session_id, "agent started answering");

        loop {
            if interrupt.load(Ordering::SeqCst) {
                break;
            }
            steps += 1;
            if steps > MAX_STEPS {
                emit.line(
                    json!({
                        "cwi": "error",
                        "message": format!("Reached the {MAX_STEPS}-step tool loop cap.")
                    })
                    .to_string(),
                );
                break;
            }

            let is_gemini = self.provider.kind == provider::Kind::Gemini;
            // Gemini builds its own request shape from the raw pieces (see
            // agent::gemini) — no point assembling the Anthropic body for it.
            let body = if is_gemini {
                Value::Null
            } else {
                self.build_body()
            };
            let system = prompt::system_prompt(&self.workspace);
            let tool_list = self.tool_list();
            let mut acc: Accumulator;
            let mut stream_failed = false;
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                acc = Accumulator::default();
                let mut think_start: Option<Instant> = None;
                let mut think_emitted = false;
                let mut got_event = false;
                let emit_c = emit.clone();
                // Both branches feed identical per-event bookkeeping into the SAME
                // Accumulator via `handle_stream_event` — only how the bytes get
                // produced differs (see `provider::Kind`'s doc comment).
                let res = if is_gemini {
                    gemini::stream(
                        &self.provider,
                        &self.http,
                        &self.stored.messages,
                        &tool_list,
                        &system,
                        self.provider.max_tokens,
                        self.provider.thinking,
                        |ev| {
                            handle_stream_event(
                                ev,
                                &emit_c,
                                &mut acc,
                                &mut think_start,
                                &mut think_emitted,
                                &mut got_event,
                            )
                        },
                        interrupt,
                    )
                    .await
                } else {
                    client::stream(
                        &self.provider,
                        &self.http,
                        body.clone(),
                        |ev| {
                            handle_stream_event(
                                ev,
                                &emit_c,
                                &mut acc,
                                &mut think_start,
                                &mut think_emitted,
                                &mut got_event,
                            )
                        },
                        interrupt,
                    )
                    .await
                };

                match res {
                    Ok(()) => break,
                    Err(e) => {
                        // Retry only if nothing streamed yet (no duplicate output).
                        if !got_event && attempt <= MAX_RETRIES && is_retryable(&e) {
                            let backoff = retry_delay(&e, attempt);
                            tracing::warn!(
                                "agent: retryable error (attempt {attempt}/{MAX_RETRIES}), \
                                 backing off {:?}: {e}",
                                backoff
                            );
                            tokio::time::sleep(backoff).await;
                            continue;
                        }
                        log_turn_error(&self.session_id, &self.provider.model, &e);
                        emit.line(
                            json!({ "cwi": "error", "message": format!("agent: {e}") }).to_string(),
                        );
                        stream_failed = true;
                        break;
                    }
                }
            }
            if stream_failed {
                break;
            }

            turn_tokens += acc.output_tokens;
            turn_input += acc.input_tokens;
            self.stored.output_tokens += acc.output_tokens;
            // Input / cache accumulate like the CLI's per-turn usage; last_context
            // is the CURRENT window fill (this call's prompt), overwritten each step.
            self.stored.input_tokens += acc.input_tokens;
            self.stored.cache_read += acc.cache_read;
            self.stored.cache_creation += acc.cache_creation;
            self.stored.last_context_tokens =
                acc.input_tokens + acc.cache_read + acc.cache_creation;
            // Split output tokens into thinking vs answer by text volume (estimate).
            let total_chars = acc.thinking_chars + acc.answer_chars;
            let think_tok = if total_chars > 0 {
                acc.output_tokens * acc.thinking_chars as u64 / total_chars as u64
            } else {
                0
            };
            self.stored.thinking_tokens += think_tok;
            self.stored.answer_tokens += acc.output_tokens - think_tok;

            let content = acc.assistant_content();
            // Complete assistant message → frontend renders tool cards from this.
            emit.line(
                json!({
                    "type": "assistant",
                    "message": { "role": "assistant", "content": content }
                })
                .to_string(),
            );
            self.stored
                .messages
                .push(json!({ "role": "assistant", "content": content }));
            self.save().await;

            // Execute whenever the model emitted tool_use blocks — do NOT gate on
            // stop_reason (providers like Kimi report it differently). Every
            // tool_use MUST get a matching tool_result or the next request 400s.
            let tool_uses = acc.tool_uses();
            if tool_uses.is_empty() {
                break; // no tools requested → the turn is done
            }

            let mut results: Vec<Value> = Vec::new();
            for (id, name, input) in tool_uses {
                let (content, is_error) = if interrupt.load(Ordering::SeqCst) {
                    ("Interrupted by user".to_string(), true)
                } else if name.starts_with("mcp__") {
                    match &self.mcp {
                        Some(m) => m.call(&name, &input).await,
                        None => (format!("No MCP server for {name}"), true),
                    }
                } else if !self.caps.allows(&name) {
                    (
                        format!("Tool '{name}' is disabled in the user's settings."),
                        true,
                    )
                } else {
                    let out = tools::execute(&name, &input, &self.workspace).await;
                    (out.content, out.is_error)
                };
                results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    // Block-array form (not a bare string) for the same reason as
                    // `user_message` — strict endpoints (Qwen/DashScope) require it.
                    "content": [{ "type": "text", "text": content }],
                    "is_error": is_error
                }));
            }
            // Tool results come back as a user message (frontend counts these).
            emit.line(
                json!({
                    "type": "user",
                    "message": { "role": "user", "content": results }
                })
                .to_string(),
            );
            self.stored
                .messages
                .push(json!({ "role": "user", "content": results }));
            self.save().await;
        }

        emit.line(
            json!({
                "type": "result",
                "model": self.stored.model,
                "usage": { "input_tokens": turn_input, "output_tokens": turn_tokens },
                "duration_ms": started.elapsed().as_millis() as u64,
                "num_turns": steps
            })
            .to_string(),
        );

        tracing::info!(
            session = %self.session_id,
            duration_ms = started.elapsed().as_millis() as u64,
            output_tokens = turn_tokens,
            "agent finished turn"
        );
    }
}

/// Per-stream-event bookkeeping shared by both wire formats: forward the raw
/// (already Anthropic-shaped, real or synthetic) event to the browser, track
/// the reasoning timer, and feed the [`Accumulator`]. Extracted to a free
/// function so both the Anthropic and Gemini branches in `run_turn` can build
/// an identical closure without either capturing the other's stream call.
fn handle_stream_event(
    ev: Value,
    emit_c: &Emit,
    acc: &mut Accumulator,
    think_start: &mut Option<Instant>,
    think_emitted: &mut bool,
    got_event: &mut bool,
) {
    *got_event = true;
    // Pass the raw stream event through, wrapped like Claude Code's.
    emit_c.line(json!({ "type": "stream_event", "event": &ev }).to_string());

    // Measure reasoning wall-clock and emit it once as a `cwi:think` frame when
    // thinking ends (a non-thinking block starts), so the block's timer
    // survives replay/reconnect (which streams instantly).
    if ev
        .get("delta")
        .and_then(|d| d.get("type"))
        .and_then(Value::as_str)
        == Some("thinking_delta")
        && think_start.is_none()
    {
        *think_start = Some(Instant::now());
    }
    if !*think_emitted
        && ev.get("type").and_then(Value::as_str) == Some("content_block_start")
        && ev["content_block"].get("type").and_then(Value::as_str) != Some("thinking")
        && let Some(s) = *think_start
    {
        emit_c.line(json!({ "cwi": "think", "ms": s.elapsed().as_millis() as u64 }).to_string());
        *think_emitted = true;
    }
    acc.on_event(&ev);
}

/// Log a failed turn to the terminal in a readable form. Provider error bodies
/// are JSON like `{"code":"AccessDenied","message":"…","request_id":"…"}` wrapped
/// as `HTTP <status>: <body>` — pull those apart into structured fields instead of
/// dumping one long blob, and fall back to the raw string when it isn't JSON.
fn log_turn_error(session_id: &str, model: &str, err: &anyhow::Error) {
    let raw = err.to_string();
    let status = raw
        .split("HTTP ")
        .nth(1)
        .and_then(|s| s.split([':', ' ']).next())
        .unwrap_or("-");
    if let Some(body) = raw
        .find('{')
        .and_then(|i| serde_json::from_str::<Value>(&raw[i..]).ok())
    {
        let field = |k: &str| {
            body.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        tracing::error!(
            session = %session_id,
            model = %model,
            status = %status,
            code = %field("code"),
            request_id = %field("request_id"),
            "agent turn failed: {}",
            field("message")
        );
    } else {
        tracing::error!(session = %session_id, model = %model, "agent turn failed: {raw}");
    }
}

/// Rewrite any string `content` into the block-array form Anthropic-compatible
/// endpoints accept universally. Strict endpoints (Alibaba's Qwen/DashScope)
/// reject bare-string content with a 400 ("content should be a valid list").
/// Applies to message content AND to `tool_result` blocks' inner content — so
/// legacy chats written before this normalization keep working after resume.
fn normalize_content(messages: &mut [Value]) {
    for m in messages.iter_mut() {
        match m.get("content") {
            Some(Value::String(s)) => {
                let s = s.clone();
                m["content"] = json!([{ "type": "text", "text": s }]);
            }
            Some(Value::Array(_)) => {
                if let Some(arr) = m["content"].as_array_mut() {
                    for b in arr.iter_mut() {
                        if b.get("type").and_then(Value::as_str) == Some("tool_result")
                            && let Some(Value::String(s)) = b.get("content")
                        {
                            let s = s.clone();
                            b["content"] = json!([{ "type": "text", "text": s }]);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Repair a stored conversation so every assistant `tool_use` is followed by a
/// matching `tool_result` — a corrupted/legacy store (e.g. an interrupted turn)
/// otherwise makes the provider reject every future request with a 400.
fn sanitize_messages(messages: &mut Vec<Value>) {
    normalize_content(messages); // legacy string content → block-array form
    let mut i = 0;
    while i < messages.len() {
        let tool_ids = tool_use_ids(&messages[i]);
        if !tool_ids.is_empty() {
            let answered: std::collections::HashSet<String> = messages
                .get(i + 1)
                .map(tool_result_ids)
                .unwrap_or_default()
                .into_iter()
                .collect();
            let missing: Vec<String> = tool_ids
                .into_iter()
                .filter(|id| !answered.contains(id))
                .collect();
            if !missing.is_empty() {
                let results: Vec<Value> = missing
                    .iter()
                    .map(|id| {
                        json!({
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": [{ "type": "text", "text": "(no result recorded — recovered)" }],
                            "is_error": true
                        })
                    })
                    .collect();
                messages.insert(i + 1, json!({ "role": "user", "content": results }));
            }
        }
        i += 1;
    }
}

fn tool_use_ids(m: &Value) -> Vec<String> {
    m.get("content")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                .filter_map(|b| b.get("id").and_then(Value::as_str).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn tool_result_ids(m: &Value) -> Vec<String> {
    m.get("content")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                .filter_map(|b| {
                    b.get("tool_use_id")
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A real user prompt (text), not a tool-result or assistant message — used as a
/// safe boundary for context compaction.
fn is_user_prompt(m: &Value) -> bool {
    if m.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match &m["content"] {
        Value::String(_) => true,
        Value::Array(a) => a
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("text")),
        _ => false,
    }
}

/// Longest we'll wait between retries, even if the server asks for more.
const MAX_BACKOFF_SECS: u64 = 30;

/// Whether a stream error is worth retrying: rate limit (429), overloaded (529),
/// server error, or a transient network/timeout error. Structured [`ApiError`]s
/// classify by status; other errors (e.g. a mid-stream transport drop) fall back
/// to string matching, treating anything without an "HTTP nnn:" prefix as network.
fn is_retryable(e: &anyhow::Error) -> bool {
    if let Some(api) = e.downcast_ref::<client::ApiError>() {
        return api.is_retryable();
    }
    let m = e.to_string();
    m.contains("HTTP 429")
        || m.contains("HTTP 500")
        || m.contains("HTTP 502")
        || m.contains("HTTP 503")
        || m.contains("HTTP 504")
        || m.contains("HTTP 529")
        || !m.contains("HTTP ")
}

/// How long to wait before the next retry. Honors a server-provided
/// `Retry-After` (capped), else exponential backoff (0.5s, 1s, 2s, …).
fn retry_delay(e: &anyhow::Error, attempt: u32) -> std::time::Duration {
    if let Some(api) = e.downcast_ref::<client::ApiError>()
        && let Some(secs) = api.retry_after
    {
        return std::time::Duration::from_secs(secs.min(MAX_BACKOFF_SECS));
    }
    let ms = (500u64 * 2u64.pow(attempt.saturating_sub(1))).min(MAX_BACKOFF_SECS * 1000);
    std::time::Duration::from_millis(ms)
}

// ---------------------------------------------------------------------------
// Streaming accumulator: rebuilds the final assistant content from stream events.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Block {
    kind: String, // "text" | "thinking" | "tool_use"
    text: String,
    tool_id: String,
    tool_name: String,
    input_json: String,
    // Gemini only: the function-call part's `thoughtSignature`, round-tripped so
    // multi-round tool calls keep working (empty for other providers).
    tool_signature: String,
}

#[derive(Default)]
struct Accumulator {
    blocks: BTreeMap<usize, Block>,
    stop_reason: String,
    output_tokens: u64,
    // Input / prompt-cache tokens: Anthropic sends them on `message_start`, Gemini
    // on its synthetic `message_delta` (see `gemini::Translator::finish`).
    input_tokens: u64,
    cache_read: u64,
    cache_creation: u64,
    thinking_chars: usize,
    answer_chars: usize,
}

impl Accumulator {
    fn on_event(&mut self, ev: &Value) {
        match ev.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let idx = ev.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let cb = &ev["content_block"];
                let kind = cb
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("text")
                    .to_string();
                let mut b = Block {
                    kind: kind.clone(),
                    ..Default::default()
                };
                if kind == "tool_use" {
                    b.tool_id = cb
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    b.tool_name = cb
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    b.tool_signature = cb
                        .get("_gemini_signature")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                }
                self.blocks.insert(idx, b);
            }
            Some("content_block_delta") => {
                let idx = ev.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let d = &ev["delta"];
                let mut think_add = 0usize;
                let mut answer_add = 0usize;
                if let Some(b) = self.blocks.get_mut(&idx) {
                    match d.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            let t = d.get("text").and_then(Value::as_str).unwrap_or("");
                            b.text.push_str(t);
                            answer_add = t.chars().count();
                        }
                        Some("thinking_delta") => {
                            let t = d.get("thinking").and_then(Value::as_str).unwrap_or("");
                            b.text.push_str(t);
                            think_add = t.chars().count();
                        }
                        Some("input_json_delta") => b
                            .input_json
                            .push_str(d.get("partial_json").and_then(Value::as_str).unwrap_or("")),
                        _ => {}
                    }
                }
                self.thinking_chars += think_add;
                self.answer_chars += answer_add;
            }
            Some("message_start") => {
                // Anthropic reports input / prompt-cache tokens here.
                if let Some(u) = ev.get("message").and_then(|m| m.get("usage")) {
                    self.capture_input_usage(u);
                }
            }
            Some("message_delta") => {
                if let Some(sr) = ev["delta"].get("stop_reason").and_then(Value::as_str) {
                    self.stop_reason = sr.to_string();
                }
                if let Some(ot) = ev["usage"].get("output_tokens").and_then(Value::as_u64) {
                    self.output_tokens = ot;
                }
                self.capture_input_usage(&ev["usage"]); // Gemini reports input/cache here
            }
            _ => {}
        }
    }

    /// Capture input / prompt-cache tokens from a usage object, overwriting only
    /// the fields present — so Anthropic's `message_start` and Gemini's synthetic
    /// `message_delta` each fill in whatever they carry.
    fn capture_input_usage(&mut self, u: &Value) {
        if let Some(v) = u.get("input_tokens").and_then(Value::as_u64) {
            self.input_tokens = v;
        }
        if let Some(v) = u.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.cache_read = v;
        }
        if let Some(v) = u.get("cache_creation_input_tokens").and_then(Value::as_u64) {
            self.cache_creation = v;
        }
    }

    /// Content array for storage/echo — text + tool_use only. Thinking blocks are
    /// emitted for display but omitted from the round-trip (avoids signature reqs).
    fn assistant_content(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for b in self.blocks.values() {
            match b.kind.as_str() {
                "text" if !b.text.is_empty() => out.push(json!({ "type": "text", "text": b.text })),
                "tool_use" => {
                    let input: Value =
                        serde_json::from_str(&b.input_json).unwrap_or_else(|_| json!({}));
                    let mut tu = json!({ "type": "tool_use", "id": b.tool_id, "name": b.tool_name, "input": input });
                    // Preserve Gemini's thoughtSignature for the round-trip (ignored
                    // by other providers, which never set it).
                    if !b.tool_signature.is_empty() {
                        tu["_gemini_signature"] = json!(b.tool_signature);
                    }
                    out.push(tu);
                }
                _ => {}
            }
        }
        out
    }

    fn tool_uses(&self) -> Vec<(String, String, Value)> {
        self.blocks
            .values()
            .filter(|b| b.kind == "tool_use")
            .map(|b| {
                let input: Value =
                    serde_json::from_str(&b.input_json).unwrap_or_else(|_| json!({}));
                (b.tool_id.clone(), b.tool_name.clone(), input)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Accumulator, MAX_BACKOFF_SECS, is_retryable, normalize_content, retry_delay,
        sanitize_messages,
    };
    use serde_json::{Value, json};

    #[test]
    fn normalize_rewrites_string_content_to_blocks() {
        let mut msgs: Vec<Value> = vec![
            json!({"role":"user","content":"hi"}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}),
            json!({"role":"assistant","content":[{"type":"text","text":"already blocks"}]}),
        ];
        normalize_content(&mut msgs);
        // Bare string → [{type:text,text}]
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "hi");
        // tool_result inner string content → [{type:text,text}]
        assert_eq!(msgs[1]["content"][0]["content"][0]["type"], "text");
        assert_eq!(msgs[1]["content"][0]["content"][0]["text"], "ok");
        // Already-block content is left untouched.
        assert_eq!(msgs[2]["content"][0]["text"], "already blocks");
    }

    #[test]
    fn sanitize_inserts_missing_tool_result() {
        let mut msgs: Vec<Value> = vec![
            json!({"role":"user","content":"hi"}),
            json!({"role":"assistant","content":[{"type":"tool_use","id":"Bash:0","name":"Bash","input":{}}]}),
            json!({"role":"user","content":"столица?"}), // NOT a tool_result → dangling
        ];
        sanitize_messages(&mut msgs);
        assert_eq!(msgs.len(), 4); // a synthetic tool_result was inserted
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "Bash:0");
    }

    #[test]
    fn sanitize_leaves_valid_pairs_alone() {
        let mut msgs: Vec<Value> = vec![
            json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}),
        ];
        sanitize_messages(&mut msgs);
        assert_eq!(msgs.len(), 2); // unchanged
    }

    #[test]
    fn accumulates_text_and_tokens() {
        let mut acc = Accumulator::default();
        acc.on_event(
            &json!({"type":"content_block_start","index":0,"content_block":{"type":"text"}}),
        );
        acc.on_event(&json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Привет"}}));
        acc.on_event(&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}));
        assert_eq!(acc.output_tokens, 7);
        assert_eq!(acc.answer_chars, 6); // 6 chars
        let content = acc.assistant_content();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "Привет");
    }

    #[test]
    fn accumulates_tool_use_input() {
        let mut acc = Accumulator::default();
        acc.on_event(&json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"Read"}}));
        acc.on_event(&json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":"}}));
        acc.on_event(&json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}));
        let tools = acc.tool_uses();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].1, "Read");
        assert_eq!(tools[0].2["file_path"], "a.rs");
    }

    #[test]
    fn captures_input_and_cache_tokens() {
        let mut acc = Accumulator::default();
        // Anthropic reports input/cache on message_start.
        acc.on_event(&json!({"type":"message_start","message":{"usage":
            {"input_tokens":120,"cache_read_input_tokens":40,"cache_creation_input_tokens":10}}}));
        acc.on_event(&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":33}}));
        assert_eq!(acc.input_tokens, 120);
        assert_eq!(acc.cache_read, 40);
        assert_eq!(acc.cache_creation, 10);
        assert_eq!(acc.output_tokens, 33);

        // Gemini reports them on the (synthetic) message_delta instead.
        let mut g = Accumulator::default();
        g.on_event(
            &json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
            "usage":{"output_tokens":10,"input_tokens":70,"cache_read_input_tokens":30}}),
        );
        assert_eq!(g.input_tokens, 70);
        assert_eq!(g.cache_read, 30);
    }

    #[test]
    fn preserves_gemini_thought_signature_on_tool_use() {
        let mut acc = Accumulator::default();
        acc.on_event(
            &json!({"type":"content_block_start","index":0,"content_block":
            {"type":"tool_use","id":"gemini-call-1","name":"Read","_gemini_signature":"SIG123"}}),
        );
        let content = acc.assistant_content();
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["_gemini_signature"], "SIG123"); // survives into storage
    }

    #[test]
    fn thinking_counts_separately() {
        let mut acc = Accumulator::default();
        acc.on_event(
            &json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}),
        );
        acc.on_event(&json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}));
        assert_eq!(acc.thinking_chars, 3);
        // thinking blocks are NOT part of the round-trip content
        assert_eq!(acc.assistant_content().len(), 0);
    }

    #[test]
    fn retryable_classification() {
        assert!(is_retryable(&anyhow::anyhow!("HTTP 429: rate limited")));
        assert!(is_retryable(&anyhow::anyhow!("HTTP 503: unavailable")));
        assert!(is_retryable(&anyhow::anyhow!("HTTP 529: overloaded")));
        assert!(is_retryable(&anyhow::anyhow!("connection reset"))); // network, no HTTP
        assert!(!is_retryable(&anyhow::anyhow!("HTTP 400: bad request")));
        assert!(!is_retryable(&anyhow::anyhow!("HTTP 401: invalid key")));
    }

    #[test]
    fn api_error_classification_and_retry_after() {
        use crate::agent::client::ApiError;
        use std::time::Duration;

        let e429 = anyhow::Error::from(ApiError {
            status: Some(429),
            retry_after: Some(7),
            message: "rate".into(),
        });
        assert!(is_retryable(&e429));
        // A server Retry-After wins over exponential backoff.
        assert_eq!(retry_delay(&e429, 1), Duration::from_secs(7));

        // Overloaded is retryable; with no Retry-After we fall back to backoff.
        let e529 = anyhow::Error::from(ApiError {
            status: Some(529),
            retry_after: None,
            message: "busy".into(),
        });
        assert!(is_retryable(&e529));
        assert_eq!(retry_delay(&e529, 1), Duration::from_millis(500));

        // A huge Retry-After is capped.
        let big = anyhow::Error::from(ApiError {
            status: Some(429),
            retry_after: Some(9999),
            message: "x".into(),
        });
        assert_eq!(retry_delay(&big, 1), Duration::from_secs(MAX_BACKOFF_SECS));

        // 4xx (other than 408/409/429) is fatal.
        let e400 = anyhow::Error::from(ApiError {
            status: Some(400),
            retry_after: None,
            message: "bad".into(),
        });
        assert!(!is_retryable(&e400));
    }
}
