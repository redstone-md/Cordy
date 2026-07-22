//! OpenAI Chat Completions adapter — the first provider, kept green as the reference.
//!
//! Two halves:
//! - [`ChatStreamParser`] — a pure, stateful mapper from streamed `data:` payloads to canonical
//!   [`WireEvent`]s. Tool calls arrive as fragments keyed by array `index`; the parser stitches
//!   them (id+name on first sight, argument JSON in pieces, close on `finish_reason`). This half
//!   is golden-fixture tested with no network.
//! - [`OpenAiChat`] — the [`Provider`] impl: renders a canonical [`ChatRequest`] to the wire
//!   body and drives the SSE response through the parser.

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::core::types::{
    Caps, ChatRequest, ContentBlock, Message, Role, ToolSpec, Usage, WireEvent,
};
use crate::provider::Provider;

// ---- wire response shapes (only the fields we consume) -------------------------------------

/// Deserialize a field that may be present-but-`null` (some OpenAI-compatible servers, e.g.
/// byesu, send `"tool_calls":null`). Plain `#[serde(default)]` only fires on an *absent* field;
/// an explicit `null` would otherwise fail to parse a non-`Option` collection.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Deserialize, Default)]
struct ChatChunk {
    #[serde(default, deserialize_with = "null_default")]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    index: u64,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FnDelta>,
}

#[derive(Deserialize)]
struct FnDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

// ---- pure streaming parser -----------------------------------------------------------------

/// Folds streamed `data:` payloads into canonical [`WireEvent`]s. Feed each payload (already
/// stripped of the `data: ` prefix) to [`push`](Self::push); it returns the events that payload
/// produced. Stateful across calls: tool-call `index` -> id mapping and call order are retained
/// so fragmented arguments and the closing `finish_reason` resolve to the right call.
#[derive(Default)]
pub struct ChatStreamParser {
    ids: HashMap<u64, String>,
    order: Vec<u64>,
}

impl ChatStreamParser {
    pub fn push(&mut self, data: &str) -> Vec<WireEvent> {
        let data = data.trim();
        if data == "[DONE]" {
            return vec![WireEvent::Done];
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Vec::new(), // ignore keep-alives / unparseable lines
        };
        // Some OpenAI-compatible servers stream errors as `{"error": {...}}` with a 200 status.
        if let Some(err) = value.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider error");
            return vec![WireEvent::Error(msg.to_string())];
        }
        let chunk: ChatChunk = serde_json::from_value(value).unwrap_or_default();

        let mut out = Vec::new();

        for choice in chunk.choices {
            if let Some(content) = choice.delta.content
                && !content.is_empty()
            {
                out.push(WireEvent::TextDelta(content));
            }

            for tc in choice.delta.tool_calls {
                let idx = tc.index;
                if let Some(id) = tc.id {
                    let name = tc
                        .function
                        .as_ref()
                        .and_then(|f| f.name.clone())
                        .unwrap_or_default();
                    self.ids.insert(idx, id.clone());
                    self.order.push(idx);
                    out.push(WireEvent::ToolUseStart { id, name });
                }
                if let Some(f) = tc.function
                    && let Some(args) = f.arguments
                    && !args.is_empty()
                    && let Some(id) = self.ids.get(&idx)
                {
                    out.push(WireEvent::ToolInputDelta {
                        id: id.clone(),
                        json: args,
                    });
                }
            }

            if choice.finish_reason.as_deref() == Some("tool_calls") {
                for idx in &self.order {
                    if let Some(id) = self.ids.get(idx) {
                        out.push(WireEvent::ToolUseEnd { id: id.clone() });
                    }
                }
            }
        }

        if let Some(u) = chunk.usage {
            out.push(WireEvent::Usage(Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                ..Default::default()
            }));
        }

        out
    }
}

// ---- provider impl -------------------------------------------------------------------------

/// Capabilities of a stock OpenAI Chat Completions endpoint.
fn openai_default_caps() -> Caps {
    Caps {
        thinking: false,
        tools: true,
        images: true,
        prompt_cache: true,         // OpenAI caches long prefixes automatically
        native_context_mgmt: false, // Chat Completions has no server-side compaction
    }
}

/// OpenAI Chat Completions provider. `base_url` + `caps` overrides let the same adapter serve
/// any OpenAI-compatible endpoint (ollama, vllm, openrouter, groq, ...).
pub struct OpenAiChat {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    caps: Caps,
}

impl OpenAiChat {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        OpenAiChat {
            client: reqwest::Client::new(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: api_key.into(),
            model: model.into(),
            caps: openai_default_caps(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Override advertised capabilities (e.g. a local model without image support).
    pub fn with_caps(mut self, caps: Caps) -> Self {
        self.caps = caps;
        self
    }

    /// Render a canonical request into the Chat Completions JSON body.
    fn to_wire(&self, req: &ChatRequest) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        if !req.system.is_empty() {
            messages.push(json!({ "role": "system", "content": req.system }));
        }
        for m in &req.messages {
            messages.extend(render_message(m));
        }

        let model = if req.model.is_empty() {
            &self.model
        } else {
            &req.model
        };
        let mut body = json!({
            "model": model,
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": messages,
        });
        if !req.tools.is_empty() {
            body["tools"] = render_tools(&req.tools);
        }
        if let Some(mt) = req.max_tokens {
            body["max_tokens"] = json!(mt);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        body
    }
}

#[async_trait]
impl Provider for OpenAiChat {
    async fn stream(&self, req: ChatRequest) -> anyhow::Result<BoxStream<'static, WireEvent>> {
        let body = self.to_wire(&req);
        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        // On an HTTP error, read the response body so the real reason (e.g. "model not found",
        // "invalid parameter") is surfaced instead of a bare status code. The word "client error"
        // / "server error" lets RetryProvider tell 4xx (don't retry) from 5xx (retry).
        let status = resp.status();
        if !status.is_success() {
            let kind = if status.is_client_error() {
                "client error"
            } else {
                "server error"
            };
            let body = resp.text().await.unwrap_or_default();
            let snippet = extract_error_message(&body);
            anyhow::bail!("HTTP {kind} {status}: {snippet}");
        }

        let mut parser = ChatStreamParser::default();
        let stream = resp.bytes_stream().eventsource().flat_map(move |item| {
            let events = match item {
                Ok(ev) => parser.push(&ev.data),
                Err(e) => vec![WireEvent::Error(e.to_string())],
            };
            futures::stream::iter(events)
        });

        Ok(stream.boxed())
    }

    fn caps(&self) -> Caps {
        self.caps
    }
}

/// Pull the human-readable reason out of an error response body: OpenAI-style `error.message`,
/// a bare `message`, else a trimmed snippet of the raw body.
fn extract_error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        for path in [&v["error"]["message"], &v["message"], &v["error"]] {
            if let Some(m) = path.as_str()
                && !m.is_empty()
            {
                return m.chars().take(400).collect();
            }
        }
    }
    let s = body.trim();
    if s.is_empty() {
        "(no response body)".into()
    } else {
        s.chars().take(400).collect()
    }
}

// ---- request rendering ---------------------------------------------------------------------

fn render_message(m: &Message) -> Vec<Value> {
    match m.role {
        Role::System => vec![json!({ "role": "system", "content": join_text(&m.content) })],
        Role::User => vec![json!({ "role": "user", "content": user_content(&m.content) })],
        Role::Assistant => {
            let mut text = String::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for b in &m.content {
                match b {
                    ContentBlock::Text { text: t } => text.push_str(t),
                    ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": input.to_string() },
                    })),
                    _ => {}
                }
            }
            let mut msg = json!({ "role": "assistant" });
            msg["content"] = if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            };
            if !tool_calls.is_empty() {
                msg["tool_calls"] = json!(tool_calls);
            }
            vec![msg]
        }
        Role::Tool => m
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { id, out } => Some(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": out.text,
                })),
                _ => None,
            })
            .collect(),
    }
}

fn user_content(content: &[ContentBlock]) -> Value {
    let has_image = content
        .iter()
        .any(|b| matches!(b, ContentBlock::Image { .. }));
    if !has_image {
        return json!(join_text(content));
    }
    let mut parts: Vec<Value> = Vec::new();
    for b in content {
        match b {
            ContentBlock::Text { text } => parts.push(json!({ "type": "text", "text": text })),
            ContentBlock::Image { media_type, data } => parts.push(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{media_type};base64,{data}") },
            })),
            _ => {}
        }
    }
    json!(parts)
}

fn join_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn render_tools(tools: &[ToolSpec]) -> Value {
    json!(
        tools
            .iter()
            .map(|t| json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                },
            }))
            .collect::<Vec<_>>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a raw SSE fixture through the parser exactly as `eventsource-stream` would: one
    /// event per `data:` payload.
    fn parse_sse(raw: &str) -> Vec<WireEvent> {
        let mut parser = ChatStreamParser::default();
        let mut out = Vec::new();
        for line in raw.lines() {
            if let Some(payload) = line.strip_prefix("data: ") {
                out.extend(parser.push(payload));
            }
        }
        out
    }

    #[test]
    fn golden_toolcall_stream() {
        let raw = include_str!("../../tests/fixtures/openai_chat_toolcall.sse");
        let events = parse_sse(raw);
        assert_eq!(
            events,
            vec![
                WireEvent::TextDelta("Editing ".into()),
                WireEvent::TextDelta("the file.".into()),
                WireEvent::ToolUseStart {
                    id: "call_abc".into(),
                    name: "edit".into()
                },
                WireEvent::ToolInputDelta {
                    id: "call_abc".into(),
                    json: "{\"path\":".into()
                },
                WireEvent::ToolInputDelta {
                    id: "call_abc".into(),
                    json: "\"a.rs\"}".into()
                },
                WireEvent::ToolUseEnd {
                    id: "call_abc".into()
                },
                WireEvent::Usage(Usage {
                    input_tokens: 42,
                    output_tokens: 18,
                    ..Default::default()
                }),
                WireEvent::Done,
            ]
        );
    }

    #[tokio::test]
    async fn parser_output_assembles_to_message() {
        // The parser output must drive the agent assembler to a correct Message.
        let raw = include_str!("../../tests/fixtures/openai_chat_toolcall.sse");
        let events = parse_sse(raw);
        let stream = futures::stream::iter(events).boxed();
        let (msg, usage) = crate::core::agent::assemble(stream).await.unwrap();
        assert_eq!(msg.content.len(), 2);
        assert_eq!(msg.content[0], ContentBlock::text("Editing the file."));
        assert!(matches!(&msg.content[1], ContentBlock::ToolUse { name, .. } if name == "edit"));
        assert_eq!(usage.input_tokens, 42);
    }

    #[test]
    fn parses_explicit_null_fields() {
        // Some OpenAI-compatible servers (byesu) send `"tool_calls":null` and other explicit
        // nulls in the delta. A plain Vec would fail to parse null; the content must survive.
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"works\",\"reasoning_content\":null,\"tool_calls\":null},\"finish_reason\":null,\"native_finish_reason\":null}]}\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\",\"tool_calls\":null},\"finish_reason\":\"stop\"}],\"usage\":{\"completion_tokens\":1,\"prompt_tokens\":6,\"total_tokens\":7}}\n",
            "data: [DONE]\n",
        );
        assert_eq!(
            parse_sse(raw),
            vec![
                WireEvent::TextDelta("works".into()),
                WireEvent::Usage(Usage {
                    input_tokens: 6,
                    output_tokens: 1,
                    ..Default::default()
                }),
                WireEvent::Done,
            ]
        );
    }

    #[test]
    fn extracts_error_reason_from_body() {
        assert_eq!(
            extract_error_message(r#"{"error":{"message":"model not found","code":404}}"#),
            "model not found"
        );
        assert_eq!(
            extract_error_message(r#"{"message":"bad request"}"#),
            "bad request"
        );
        assert_eq!(extract_error_message("plain text oops"), "plain text oops");
        assert_eq!(extract_error_message("  "), "(no response body)");
    }

    #[test]
    fn to_wire_shapes_tools_and_messages() {
        let p = OpenAiChat::new("k", "gpt-x");
        let req = ChatRequest {
            model: String::new(),
            system: "be terse".into(),
            messages: vec![Message::user("hi")],
            tools: vec![ToolSpec {
                name: "read".into(),
                description: "read a file".into(),
                input_schema: json!({ "type": "object" }),
            }],
            max_tokens: Some(256),
            temperature: None,
        };
        let body = p.to_wire(&req);
        assert_eq!(body["model"], "gpt-x");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hi");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(body["max_tokens"], 256);
    }
}
