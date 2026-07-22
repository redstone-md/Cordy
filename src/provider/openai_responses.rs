//! OpenAI Responses API adapter.
//!
//! The Responses API uses typed SSE events (`response.output_text.delta`,
//! `response.function_call_arguments.delta`, `response.output_item.added/done`,
//! `response.completed`) and an `input`-items request shape distinct from Chat Completions.
//! [`ResponsesStreamParser`] maps the events to canonical [`WireEvent`]s (function-call `item_id`
//! -> `call_id`), golden-fixture tested. This is the API family that offers server-side
//! compaction.

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

/// Normalizes Responses SSE payloads to canonical events. Stateful: maps a function call's
/// streaming `item_id` to the `call_id` used as the canonical tool id.
#[derive(Default)]
pub struct ResponsesStreamParser {
    /// function-call item_id -> call_id
    call_ids: HashMap<String, String>,
}

impl ResponsesStreamParser {
    pub fn push(&mut self, data: &str) -> Vec<WireEvent> {
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        match v["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => {
                vec![WireEvent::TextDelta(
                    v["delta"].as_str().unwrap_or_default().into(),
                )]
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                vec![WireEvent::ThinkingDelta(
                    v["delta"].as_str().unwrap_or_default().into(),
                )]
            }
            "response.output_item.added" => {
                let item = &v["item"];
                if item["type"] == "function_call" {
                    let item_id = item["id"].as_str().unwrap_or_default().to_string();
                    let call_id = item["call_id"].as_str().unwrap_or_default().to_string();
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    self.call_ids.insert(item_id, call_id.clone());
                    vec![WireEvent::ToolUseStart { id: call_id, name }]
                } else {
                    Vec::new()
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = v["item_id"].as_str().unwrap_or_default();
                match self.call_ids.get(item_id) {
                    Some(call_id) => vec![WireEvent::ToolInputDelta {
                        id: call_id.clone(),
                        json: v["delta"].as_str().unwrap_or_default().into(),
                    }],
                    None => Vec::new(),
                }
            }
            "response.output_item.done" => {
                let item_id = v["item"]["id"].as_str().unwrap_or_default();
                match self.call_ids.get(item_id) {
                    Some(call_id) => vec![WireEvent::ToolUseEnd {
                        id: call_id.clone(),
                    }],
                    None => Vec::new(),
                }
            }
            "response.completed" | "response.incomplete" => {
                let usage = &v["response"]["usage"];
                vec![
                    WireEvent::Usage(Usage {
                        input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                        ..Default::default()
                    }),
                    WireEvent::Done,
                ]
            }
            "response.failed" | "error" => {
                let msg = v["response"]["error"]["message"]
                    .as_str()
                    .or_else(|| v["message"].as_str())
                    .unwrap_or("responses stream error");
                vec![WireEvent::Error(msg.into())]
            }
            _ => Vec::new(),
        }
    }
}

/// OpenAI Responses provider.
pub struct OpenAiResponses {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAiResponses {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        OpenAiResponses {
            client: reqwest::Client::new(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn to_wire(&self, req: &ChatRequest) -> Value {
        let mut input: Vec<Value> = Vec::new();
        for m in &req.messages {
            input.extend(render_message(m));
        }
        let model = if req.model.is_empty() {
            &self.model
        } else {
            &req.model
        };
        let mut body = json!({
            "model": model,
            "stream": true,
            "input": input,
        });
        if !req.system.is_empty() {
            body["instructions"] = json!(req.system);
        }
        if !req.tools.is_empty() {
            body["tools"] = render_tools(&req.tools);
        }
        if let Some(mt) = req.max_tokens {
            body["max_output_tokens"] = json!(mt);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        body
    }
}

#[async_trait]
impl Provider for OpenAiResponses {
    async fn stream(&self, req: ChatRequest) -> anyhow::Result<BoxStream<'static, WireEvent>> {
        let body = self.to_wire(&req);
        let resp = self
            .client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let mut parser = ResponsesStreamParser::default();
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
            native_context_mgmt: true, // Responses offers server-side compaction
        }
    }
}

fn render_message(m: &Message) -> Vec<Value> {
    match m.role {
        Role::System => Vec::new(), // system -> `instructions`
        Role::User => {
            let content: Vec<Value> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => {
                        Some(json!({ "type": "input_text", "text": text }))
                    }
                    ContentBlock::Image { media_type, data } => Some(json!({
                        "type": "input_image",
                        "image_url": format!("data:{media_type};base64,{data}"),
                    })),
                    _ => None,
                })
                .collect();
            vec![json!({ "role": "user", "content": content })]
        }
        Role::Assistant => {
            let mut items: Vec<Value> = Vec::new();
            let mut text_parts: Vec<Value> = Vec::new();
            for b in &m.content {
                match b {
                    ContentBlock::Text { text } => {
                        text_parts.push(json!({ "type": "output_text", "text": text }));
                    }
                    ContentBlock::ToolUse { id, name, input } => items.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": input.to_string(),
                    })),
                    _ => {}
                }
            }
            if !text_parts.is_empty() {
                items.insert(0, json!({ "role": "assistant", "content": text_parts }));
            }
            items
        }
        Role::Tool => m
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { id, out } => Some(json!({
                    "type": "function_call_output",
                    "call_id": id,
                    "output": out.text,
                })),
                _ => None,
            })
            .collect(),
    }
}

fn render_tools(tools: &[ToolSpec]) -> Value {
    json!(
        tools
            .iter()
            .map(|t| json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            }))
            .collect::<Vec<_>>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_sse(raw: &str) -> Vec<WireEvent> {
        let mut parser = ResponsesStreamParser::default();
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
        let raw = include_str!("../../tests/fixtures/openai_responses_toolcall.sse");
        assert_eq!(
            parse_sse(raw),
            vec![
                WireEvent::TextDelta("Reading ".into()),
                WireEvent::TextDelta("file.".into()),
                WireEvent::ToolUseStart {
                    id: "call_1".into(),
                    name: "read".into()
                },
                WireEvent::ToolInputDelta {
                    id: "call_1".into(),
                    json: "{\"path\":".into()
                },
                WireEvent::ToolInputDelta {
                    id: "call_1".into(),
                    json: "\"a.rs\"}".into()
                },
                WireEvent::ToolUseEnd {
                    id: "call_1".into()
                },
                WireEvent::Usage(Usage {
                    input_tokens: 30,
                    output_tokens: 12,
                    ..Default::default()
                }),
                WireEvent::Done,
            ]
        );
    }

    #[test]
    fn to_wire_uses_input_items_and_instructions() {
        let p = OpenAiResponses::new("k", "gpt-x");
        let req = ChatRequest {
            model: String::new(),
            system: "be terse".into(),
            messages: vec![Message::user("hi")],
            tools: vec![ToolSpec {
                name: "read".into(),
                description: "read a file".into(),
                input_schema: json!({ "type": "object" }),
            }],
            max_tokens: Some(128),
            temperature: None,
        };
        let body = p.to_wire(&req);
        assert_eq!(body["instructions"], "be terse");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["max_output_tokens"], 128);
    }
}
