//! Anthropic Messages adapter.
//!
//! Anthropic's wire format differs from OpenAI's: typed SSE events (`message_start`,
//! `content_block_start/delta/stop`, `message_delta`, `message_stop`) carry content blocks by
//! `index`, and tool arguments stream as `input_json_delta` fragments. [`AnthropicStreamParser`]
//! normalizes all of it to the same canonical [`WireEvent`]s the loop already understands —
//! golden-fixture tested, no network. Requests render into Anthropic's block-structured body.

use std::collections::HashMap;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};

use crate::core::types::{
    Caps, ChatRequest, ContentBlock, Message, Role, ToolSpec, Usage, WireEvent,
};
use crate::provider::Provider;

const API_VERSION: &str = "2023-06-01";

/// Normalizes Anthropic SSE payloads (the JSON after `data:`) to canonical events. Stateful:
/// tracks block `index` -> tool id and accumulates usage across `message_start`/`message_delta`.
#[derive(Default)]
pub struct AnthropicStreamParser {
    tool_ids: HashMap<u64, String>,
    input_tokens: u64,
    output_tokens: u64,
}

impl AnthropicStreamParser {
    pub fn push(&mut self, data: &str) -> Vec<WireEvent> {
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        match v["type"].as_str().unwrap_or_default() {
            "message_start" => {
                if let Some(n) = v["message"]["usage"]["input_tokens"].as_u64() {
                    self.input_tokens = n;
                }
                Vec::new()
            }
            "content_block_start" => {
                let idx = v["index"].as_u64().unwrap_or(0);
                let block = &v["content_block"];
                if block["type"] == "tool_use" {
                    let id = block["id"].as_str().unwrap_or_default().to_string();
                    let name = block["name"].as_str().unwrap_or_default().to_string();
                    self.tool_ids.insert(idx, id.clone());
                    vec![WireEvent::ToolUseStart { id, name }]
                } else {
                    Vec::new()
                }
            }
            "content_block_delta" => {
                let idx = v["index"].as_u64().unwrap_or(0);
                let delta = &v["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        vec![WireEvent::TextDelta(
                            delta["text"].as_str().unwrap_or_default().into(),
                        )]
                    }
                    "thinking_delta" => vec![WireEvent::ThinkingDelta(
                        delta["thinking"].as_str().unwrap_or_default().into(),
                    )],
                    "input_json_delta" => match self.tool_ids.get(&idx) {
                        Some(id) => vec![WireEvent::ToolInputDelta {
                            id: id.clone(),
                            json: delta["partial_json"].as_str().unwrap_or_default().into(),
                        }],
                        None => Vec::new(),
                    },
                    _ => Vec::new(),
                }
            }
            "content_block_stop" => {
                let idx = v["index"].as_u64().unwrap_or(0);
                match self.tool_ids.get(&idx) {
                    Some(id) => vec![WireEvent::ToolUseEnd { id: id.clone() }],
                    None => Vec::new(),
                }
            }
            "message_delta" => {
                if let Some(n) = v["usage"]["output_tokens"].as_u64() {
                    self.output_tokens = n;
                }
                Vec::new()
            }
            "message_stop" => vec![
                WireEvent::Usage(Usage {
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
                    ..Default::default()
                }),
                WireEvent::Done,
            ],
            _ => Vec::new(), // ping and unknown events
        }
    }
}

/// Anthropic Messages provider.
pub struct Anthropic {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl Anthropic {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Anthropic {
            client: reqwest::Client::new(),
            base_url: "https://api.anthropic.com".into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn to_wire(&self, req: &ChatRequest) -> Value {
        let messages = coalesce_roles(req.messages.iter().filter_map(render_message).collect());
        let model = if req.model.is_empty() {
            &self.model
        } else {
            &req.model
        };
        let mut body = json!({
            "model": model,
            "stream": true,
            "max_tokens": req.max_tokens.unwrap_or(4096),
            "messages": messages,
        });
        if !req.system.is_empty() {
            // Cache the system prompt: send it as a text block with a cache breakpoint.
            body["system"] = json!([{
                "type": "text",
                "text": req.system,
                "cache_control": { "type": "ephemeral" },
            }]);
        }
        if !req.tools.is_empty() {
            body["tools"] = render_tools(&req.tools);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        body
    }
}

#[async_trait]
impl Provider for Anthropic {
    async fn stream(&self, req: ChatRequest) -> anyhow::Result<BoxStream<'static, WireEvent>> {
        let body = self.to_wire(&req);
        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let mut parser = AnthropicStreamParser::default();
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
        Caps {
            thinking: true,
            tools: true,
            images: true,
            prompt_cache: true,
            native_context_mgmt: true, // context editing (beta header) available
        }
    }
}

/// Merge adjacent wire messages that share a role.
///
/// The Messages API wants strictly alternating roles, but the canonical history can legitimately
/// produce two user-role messages in a row — a tool-result message followed by an injected
/// steering message, for instance. Concatenating their content blocks preserves both.
fn coalesce_roles(messages: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        let same_role = out
            .last()
            .and_then(|prev: &Value| prev.get("role"))
            .is_some_and(|role| Some(role) == msg.get("role"));
        if same_role
            && let Some(prev) = out.last_mut()
            && let (Some(prev_blocks), Some(blocks)) = (
                prev.get_mut("content").and_then(Value::as_array_mut),
                msg.get("content").and_then(Value::as_array),
            )
        {
            prev_blocks.extend(blocks.iter().cloned());
            continue;
        }
        out.push(msg);
    }
    out
}

fn render_message(m: &Message) -> Option<Value> {
    match m.role {
        Role::System => None, // system is a top-level field
        Role::User => Some(json!({ "role": "user", "content": user_blocks(&m.content) })),
        Role::Assistant => {
            let blocks: Vec<Value> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(json!({ "type": "text", "text": text })),
                    ContentBlock::ToolUse { id, name, input } => Some(json!({
                        "type": "tool_use", "id": id, "name": name, "input": input,
                    })),
                    _ => None,
                })
                .collect();
            Some(json!({ "role": "assistant", "content": blocks }))
        }
        Role::Tool => {
            let blocks: Vec<Value> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult { id, out } => Some(json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": out.text,
                        "is_error": out.is_error,
                    })),
                    _ => None,
                })
                .collect();
            Some(json!({ "role": "user", "content": blocks }))
        }
    }
}

fn user_blocks(content: &[ContentBlock]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(json!({ "type": "text", "text": text })),
            ContentBlock::Image { media_type, data } => Some(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data },
            })),
            _ => None,
        })
        .collect()
}

fn render_tools(tools: &[ToolSpec]) -> Value {
    let last = tools.len().saturating_sub(1);
    json!(
        tools
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut v = json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                });
                // A cache breakpoint on the last tool caches the whole tool block.
                if i == last {
                    v["cache_control"] = json!({ "type": "ephemeral" });
                }
                v
            })
            .collect::<Vec<_>>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_sse(raw: &str) -> Vec<WireEvent> {
        let mut parser = AnthropicStreamParser::default();
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
        let raw = include_str!("../../tests/fixtures/anthropic_toolcall.sse");
        assert_eq!(
            parse_sse(raw),
            vec![
                WireEvent::TextDelta("Reading ".into()),
                WireEvent::TextDelta("file.".into()),
                WireEvent::ToolUseStart {
                    id: "toolu_1".into(),
                    name: "read".into()
                },
                WireEvent::ToolInputDelta {
                    id: "toolu_1".into(),
                    json: "{\"path\":".into()
                },
                WireEvent::ToolInputDelta {
                    id: "toolu_1".into(),
                    json: "\"a.rs\"}".into()
                },
                WireEvent::ToolUseEnd {
                    id: "toolu_1".into()
                },
                WireEvent::Usage(Usage {
                    input_tokens: 25,
                    output_tokens: 15,
                    ..Default::default()
                }),
                WireEvent::Done,
            ]
        );
    }

    #[tokio::test]
    async fn parser_output_assembles_to_message() {
        let raw = include_str!("../../tests/fixtures/anthropic_toolcall.sse");
        let stream = futures::stream::iter(parse_sse(raw)).boxed();
        let (msg, usage) = crate::core::agent::assemble(stream).await.unwrap();
        assert_eq!(msg.content[0], ContentBlock::text("Reading file."));
        assert!(matches!(&msg.content[1], ContentBlock::ToolUse { name, .. } if name == "read"));
        assert_eq!(usage.output_tokens, 15);
    }

    #[test]
    fn to_wire_uses_system_field_and_block_content() {
        let p = Anthropic::new("k", "claude-x");
        let req = ChatRequest {
            model: String::new(),
            system: "be terse".into(),
            messages: vec![Message::user("hi")],
            tools: vec![ToolSpec {
                name: "read".into(),
                description: "read a file".into(),
                input_schema: json!({ "type": "object" }),
            }],
            max_tokens: None,
            temperature: None,
        };
        let body = p.to_wire(&req);
        assert_eq!(body["model"], "claude-x");
        assert_eq!(body["system"][0]["text"], "be terse");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
    }
}
