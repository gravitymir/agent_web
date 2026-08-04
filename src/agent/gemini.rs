//! Gemini adapter: translates between our internal Anthropic-shaped messages/
//! tools/stream-events (used by `Engine::run_turn`, `Accumulator`, `store.rs`)
//! and Google's `:streamGenerateContent` wire format, so the rest of the native
//! engine never has to know a second provider shape exists.
//!
//! Wire-format differences that force this to be a real adapter, not just a
//! new [`provider::Provider`] preset:
//! - Request: `contents[].parts[]` (`text`/`inlineData`/`functionCall`/
//!   `functionResponse`) instead of Anthropic's `messages[].content[]` blocks;
//!   `tools[].functionDeclarations[].parameters` instead of `input_schema`;
//!   model id is part of the URL path, not the request body.
//! - Function calls have **no stable id** on the wire (unlike Anthropic's
//!   `tool_use.id`/`tool_result.tool_use_id`) — matched by name/position
//!   instead. We still synthesize an id (`gemini-call-{n}`) purely so the
//!   *existing* Accumulator/tool-loop code (which requires one) works
//!   unmodified; it never round-trips back to Gemini.
//! - Streaming sends incremental `GenerateContentResponse` chunks with no
//!   explicit block-start/stop markers — block boundaries are inferred here
//!   from when a part's kind (text/thought/functionCall) changes, then
//!   replayed as synthetic Anthropic `content_block_start`/`_delta` events
//!   into the same [`super::Accumulator`] used for every other provider.
//!
//! Least-confident spots (verified against Google's docs at write time, but
//! Gemini's API has churned quickly — recheck here first if thinking/tool
//! calls come back empty or malformed): the exact `thought` part shape
//! (`{"thought": "..."}` vs. `{"text": "...", "thought": true}` — handled
//! defensively, both are recognized) and `generationConfig.thinkingConfig`'s
//! field names.

use std::sync::atomic::AtomicBool;

use anyhow::Result;
use serde_json::{json, Value};

use crate::agent::client::{response_to_sse_events, ApiError};
use crate::agent::provider::Provider;

/// POST `:streamGenerateContent` and replay it as synthetic Anthropic stream
/// events through `on_event` — same contract as `client::stream`, so
/// `Engine::run_turn`'s retry loop and `Accumulator` need no branching beyond
/// picking which of the two functions to call (see `provider::Kind`).
#[allow(clippy::too_many_arguments)]
pub async fn stream(
    provider: &Provider,
    client: &reqwest::Client,
    messages: &[Value],
    tools: &[Value],
    system: &str,
    max_tokens: u32,
    thinking: bool,
    mut on_event: impl FnMut(Value),
    interrupt: &AtomicBool,
) -> Result<()> {
    let body = build_body(messages, tools, system, max_tokens, thinking);
    let url = format!(
        "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
        provider.base_url, provider.model
    );

    let resp = match client
        .post(url)
        .header("x-goog-api-key", &provider.api_key)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Err(ApiError::network_for_gemini(e.to_string()).into()),
    };

    let mut xlate = Translator::default();
    response_to_sse_events(
        resp,
        |chunk| xlate.on_chunk(&chunk, &mut on_event),
        interrupt,
    )
    .await?;
    xlate.finish(&mut on_event);
    Ok(())
}

// ---------------------------------------------------------------------------
// Request building: our stored Anthropic-shaped messages/tools → Gemini's body
// ---------------------------------------------------------------------------

fn build_body(messages: &[Value], tools: &[Value], system: &str, max_tokens: u32, thinking: bool) -> Value {
    // tool_use.id -> name, so a later tool_result (which only carries the id)
    // can be translated into Gemini's name-keyed functionResponse.
    let mut call_names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in messages {
        if let Some(arr) = m.get("content").and_then(Value::as_array) {
            for b in arr {
                if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let id = b.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                    let name = b.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                    call_names.insert(id, name);
                }
            }
        }
    }

    let contents: Vec<Value> = messages.iter().map(|m| to_gemini_content(m, &call_names)).collect();

    let mut body = json!({
        "contents": contents,
        "systemInstruction": { "parts": [{ "text": system }] },
        "generationConfig": { "maxOutputTokens": max_tokens },
    });
    if !tools.is_empty() {
        body["tools"] = json!([{ "functionDeclarations": tools.iter().map(to_function_declaration).collect::<Vec<_>>() }]);
    }
    if thinking {
        body["generationConfig"]["thinkingConfig"] = json!({ "includeThoughts": true });
    }
    body
}

/// One stored `{role, content}` message -> Gemini's `{role, parts}`.
fn to_gemini_content(m: &Value, call_names: &std::collections::HashMap<String, String>) -> Value {
    let role = if m.get("role").and_then(Value::as_str) == Some("assistant") { "model" } else { "user" };
    let content = &m["content"];
    let parts: Vec<Value> = match content {
        Value::String(s) => vec![json!({ "text": s })],
        Value::Array(blocks) => blocks.iter().filter_map(|b| to_gemini_part(b, call_names)).collect(),
        _ => vec![],
    };
    json!({ "role": role, "parts": parts })
}

fn to_gemini_part(b: &Value, call_names: &std::collections::HashMap<String, String>) -> Option<Value> {
    match b.get("type").and_then(Value::as_str) {
        Some("text") => Some(json!({ "text": b.get("text").and_then(Value::as_str).unwrap_or("") })),
        Some("image") => {
            let src = b.get("source")?;
            Some(json!({
                "inlineData": {
                    "mimeType": src.get("media_type").and_then(Value::as_str).unwrap_or("image/png"),
                    "data": src.get("data").and_then(Value::as_str).unwrap_or(""),
                }
            }))
        }
        Some("tool_use") => {
            let mut part = json!({
                "functionCall": {
                    "name": b.get("name").and_then(Value::as_str).unwrap_or(""),
                    "args": b.get("input").cloned().unwrap_or_else(|| json!({})),
                }
            });
            // Echo back the thoughtSignature captured on the response part, or Gemini
            // rejects the follow-up request (see `Translator::on_part`).
            if let Some(sig) = b.get("_gemini_signature").and_then(Value::as_str) {
                part["thoughtSignature"] = json!(sig);
            }
            Some(part)
        }
        Some("tool_result") => {
            let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or_default();
            let name = call_names.get(id).cloned().unwrap_or_else(|| "unknown".to_string());
            let content = b.get("content").and_then(Value::as_str).unwrap_or_default();
            let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let response = if is_error { json!({ "error": content }) } else { json!({ "result": content }) };
            Some(json!({ "functionResponse": { "name": name, "response": response } }))
        }
        _ => None,
    }
}

/// Our `{name, description, input_schema}` -> Gemini's `{name, description, parameters}`.
fn to_function_declaration(t: &Value) -> Value {
    json!({
        "name": t.get("name").cloned().unwrap_or(Value::Null),
        "description": t.get("description").cloned().unwrap_or(Value::Null),
        "parameters": t.get("input_schema").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    })
}

// ---------------------------------------------------------------------------
// Response translation: Gemini chunks -> synthetic Anthropic stream events
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Translator {
    index: i64,
    current_kind: Option<&'static str>,
    output_tokens: u64,
    input_tokens: u64, // non-cached prompt tokens (promptTokenCount - cached)
    cache_read: u64,   // cachedContentTokenCount
    finish_reason: Option<String>,
    started: bool,
}

impl Translator {
    fn on_chunk(&mut self, chunk: &Value, emit: &mut impl FnMut(Value)) {
        if let Some(u) = chunk.get("usageMetadata") {
            let candidates = u.get("candidatesTokenCount").and_then(Value::as_u64).unwrap_or(0);
            let thoughts = u.get("thoughtsTokenCount").and_then(Value::as_u64).unwrap_or(0);
            self.output_tokens = candidates + thoughts; // output_tokens includes thinking, like Anthropic's
            // Split the prompt into cached vs not, matching Anthropic's shape so
            // input + cache_read == the full context size (no double counting).
            let prompt = u.get("promptTokenCount").and_then(Value::as_u64).unwrap_or(0);
            let cached = u.get("cachedContentTokenCount").and_then(Value::as_u64).unwrap_or(0);
            self.cache_read = cached;
            self.input_tokens = prompt.saturating_sub(cached);
        }
        let Some(candidate) = chunk.get("candidates").and_then(Value::as_array).and_then(|a| a.first()) else {
            return;
        };
        if let Some(fr) = candidate.get("finishReason").and_then(Value::as_str) {
            self.finish_reason = Some(fr.to_string());
        }
        let Some(parts) = candidate.get("content").and_then(|c| c.get("parts")).and_then(Value::as_array) else {
            return;
        };
        for part in parts {
            self.on_part(part, emit);
        }
    }

    fn on_part(&mut self, part: &Value, emit: &mut impl FnMut(Value)) {
        self.started = true;
        if let Some(call) = part.get("functionCall") {
            self.index += 1;
            let idx = self.index;
            let name = call.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            // Gemini 3 attaches a `thoughtSignature` (base64 crypto-signature of the
            // reasoning) to the function-call part. It MUST be echoed back verbatim
            // on the next request or multi-round tool calls fail, so carry it through
            // the tool_use block (Accumulator → stored `_gemini_signature`).
            let mut content_block =
                json!({ "type": "tool_use", "id": format!("gemini-call-{idx}"), "name": name });
            if let Some(sig) = part.get("thoughtSignature").and_then(Value::as_str) {
                content_block["_gemini_signature"] = json!(sig);
            }
            emit(json!({
                "type": "content_block_start", "index": idx, "content_block": content_block
            }));
            emit(json!({
                "type": "content_block_delta", "index": idx,
                "delta": { "type": "input_json_delta", "partial_json": args.to_string() }
            }));
            self.current_kind = None; // next text/thought part (if any) opens a fresh block
            return;
        }

        // Thinking: either a dedicated `thought` string field, or a `text` part
        // flagged `"thought": true` — different sources describe each; accept both.
        let (kind, text): (&'static str, &str) = if let Some(t) = part.get("thought").and_then(Value::as_str) {
            ("thinking", t)
        } else if part.get("thought").and_then(Value::as_bool) == Some(true) {
            ("thinking", part.get("text").and_then(Value::as_str).unwrap_or(""))
        } else if let Some(t) = part.get("text").and_then(Value::as_str) {
            ("text", t)
        } else {
            return; // unrecognized part kind (e.g. an inline image in the reply) — skip
        };
        if text.is_empty() && self.current_kind == Some(kind) {
            return; // nothing new to append
        }
        if self.current_kind != Some(kind) {
            self.index += 1;
            emit(json!({
                "type": "content_block_start", "index": self.index,
                "content_block": { "type": kind }
            }));
            self.current_kind = Some(kind);
        }
        let delta = if kind == "thinking" {
            json!({ "type": "thinking_delta", "thinking": text })
        } else {
            json!({ "type": "text_delta", "text": text })
        };
        emit(json!({ "type": "content_block_delta", "index": self.index, "delta": delta }));
    }

    /// No explicit "message stop" event on the wire — synthesize the
    /// `message_delta` `Accumulator` expects once the SSE stream ends.
    fn finish(&self, emit: &mut impl FnMut(Value)) {
        if !self.started && self.output_tokens == 0 {
            return; // nothing streamed at all (e.g. an error already surfaced)
        }
        let stop_reason = match self.finish_reason.as_deref() {
            Some("MAX_TOKENS") => "max_tokens",
            _ => "end_turn",
        };
        emit(json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason },
            "usage": {
                "output_tokens": self.output_tokens,
                "input_tokens": self.input_tokens,
                "cache_read_input_tokens": self.cache_read,
            }
        }));
    }
}

impl ApiError {
    /// Small helper so `gemini::stream` doesn't need `client`'s private
    /// constructor — a network-layer failure (DNS/TLS/timeout), same as
    /// `client::stream`'s.
    fn network_for_gemini(message: String) -> Self {
        ApiError { status: None, retry_after: None, message }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_text_and_tool_use_messages_to_contents() {
        let messages = vec![
            json!({"role":"user","content":"hi"}),
            json!({"role":"assistant","content":[
                {"type":"text","text":"looking"},
                {"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"a.rs"}}
            ]}),
            json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":"file contents","is_error":false}
            ]}),
        ];
        let body = build_body(&messages, &[], "sys", 8192, false);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "hi");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["name"], "Read");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["args"]["file_path"], "a.rs");
        // The tool_result's name is looked up from the matching tool_use id.
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "Read");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["response"]["result"], "file contents");
    }

    #[test]
    fn function_declaration_renames_input_schema_to_parameters() {
        let tool = json!({"name":"Read","description":"reads a file","input_schema":{"type":"object"}});
        let decl = to_function_declaration(&tool);
        assert_eq!(decl["name"], "Read");
        assert_eq!(decl["parameters"]["type"], "object");
        assert!(decl.get("input_schema").is_none());
    }

    #[test]
    fn translator_emits_text_then_tool_call_as_separate_blocks() {
        let mut events = Vec::new();
        let mut xlate = Translator::default();
        xlate.on_chunk(
            &json!({"candidates":[{"content":{"parts":[{"text":"looking into it"}]}}]}),
            &mut |v| events.push(v),
        );
        xlate.on_chunk(
            &json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"Read","args":{"file_path":"a.rs"}}}]},"finishReason":"STOP"}],
                     "usageMetadata":{"candidatesTokenCount":12,"thoughtsTokenCount":3}}),
            &mut |v| events.push(v),
        );
        xlate.finish(&mut |v| events.push(v));

        assert_eq!(events[0]["type"], "content_block_start");
        assert_eq!(events[0]["content_block"]["type"], "text");
        assert_eq!(events[1]["type"], "content_block_delta");
        assert_eq!(events[1]["delta"]["text"], "looking into it");
        assert_eq!(events[2]["content_block"]["type"], "tool_use");
        assert_eq!(events[2]["content_block"]["name"], "Read");
        assert_eq!(events[3]["delta"]["partial_json"], json!({"file_path":"a.rs"}).to_string());
        let last = events.last().unwrap();
        assert_eq!(last["type"], "message_delta");
        assert_eq!(last["usage"]["output_tokens"], 15);
    }

    #[test]
    fn translator_recognizes_both_thought_conventions() {
        let mut events = Vec::new();
        let mut xlate = Translator::default();
        xlate.on_chunk(&json!({"candidates":[{"content":{"parts":[{"thought":"hmm, let me think"}]}}]}), &mut |v| events.push(v));
        assert_eq!(events[0]["content_block"]["type"], "thinking");
        assert_eq!(events[1]["delta"]["thinking"], "hmm, let me think");

        let mut events2 = Vec::new();
        let mut xlate2 = Translator::default();
        xlate2.on_chunk(&json!({"candidates":[{"content":{"parts":[{"text":"pondering","thought":true}]}}]}), &mut |v| events2.push(v));
        assert_eq!(events2[0]["content_block"]["type"], "thinking");
        assert_eq!(events2[1]["delta"]["thinking"], "pondering");
    }

    #[test]
    fn translator_captures_thought_signature_on_function_call() {
        let mut events = Vec::new();
        let mut xlate = Translator::default();
        xlate.on_chunk(
            &json!({"candidates":[{"content":{"parts":[
                {"functionCall":{"name":"Read","args":{"file_path":"a.rs"}}, "thoughtSignature":"SIG123"}
            ]}}]}),
            &mut |v| events.push(v),
        );
        // The signature rides on the tool_use content_block for the Accumulator to store.
        assert_eq!(events[0]["content_block"]["type"], "tool_use");
        assert_eq!(events[0]["content_block"]["_gemini_signature"], "SIG123");
    }

    #[test]
    fn translator_splits_prompt_into_input_and_cache() {
        let mut events = Vec::new();
        let mut xlate = Translator::default();
        xlate.on_chunk(
            &json!({"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}],
                    "usageMetadata":{"candidatesTokenCount":10,"promptTokenCount":100,"cachedContentTokenCount":30}}),
            &mut |v| events.push(v),
        );
        xlate.finish(&mut |v| events.push(v));
        let usage = &events.last().unwrap()["usage"];
        assert_eq!(usage["output_tokens"], 10);
        assert_eq!(usage["input_tokens"], 70); // 100 prompt - 30 cached
        assert_eq!(usage["cache_read_input_tokens"], 30);
    }

    #[test]
    fn build_body_echoes_stored_thought_signature() {
        let messages = vec![json!({"role":"assistant","content":[
            {"type":"tool_use","id":"gemini-call-1","name":"Read",
             "input":{"file_path":"a.rs"}, "_gemini_signature":"SIG123"}
        ]})];
        let body = build_body(&messages, &[], "sys", 8192, false);
        let part = &body["contents"][0]["parts"][0];
        assert_eq!(part["functionCall"]["name"], "Read");
        assert_eq!(part["thoughtSignature"], "SIG123"); // echoed back verbatim
    }
}
