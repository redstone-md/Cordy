//! The canonical model — the single representation every layer speaks.
//!
//! Provider wire formats (OpenAI Chat / OpenAI Responses / Anthropic Messages) are translated
//! to and from these types inside their adapters and never leak past them. The agent loop,
//! tools, and UI only ever touch what is defined here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who authored a [`Message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One piece of a message. A message is a sequence of these, allowing interleaved text,
/// reasoning, tool calls, tool results, and images.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        out: ToolOutput,
    },
    Image {
        media_type: String,
        data: String,
    },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }
}

/// The result of running a [`crate::tools::Tool`]. `saved` records tokens the native optimizer
/// removed from `text` (0 when optimization was off or not applicable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub text: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub saved: u64,
}

impl ToolOutput {
    pub fn ok(text: impl Into<String>) -> Self {
        ToolOutput {
            text: text.into(),
            is_error: false,
            saved: 0,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        ToolOutput {
            text: text.into(),
            is_error: true,
            saved: 0,
        }
    }
}

/// A full turn's message in the canonical model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Message {
            role: Role::Assistant,
            content,
        }
    }
}

/// The normalized streaming event every provider adapter emits. Wire-specific SSE shapes are
/// mapped onto this by `parse_stream` in each adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WireEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    /// A fragment of the tool call's argument JSON (streamed incrementally).
    ToolInputDelta {
        id: String,
        json: String,
    },
    ToolUseEnd {
        id: String,
    },
    Usage(Usage),
    Done,
    Error(String),
}

/// Token accounting for a turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// What a provider/model can do. Drives adapter behavior and UI affordances.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    pub thinking: bool,
    pub tools: bool,
    pub images: bool,
    pub prompt_cache: bool,
    /// Provider offers server-side context management (Anthropic context editing / OpenAI
    /// Responses compaction). When false, the client-side compactor is mandatory.
    pub native_context_mgmt: bool,
}

/// A tool advertised to the model, in canonical form. Adapters render this into each
/// provider's tool-spec shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A request to a provider, canonical. Adapters translate this into their wire body.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trips_through_json() {
        let msg = Message::assistant(vec![
            ContentBlock::Thinking {
                text: "let me think".into(),
            },
            ContentBlock::text("here is the plan"),
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "edit".into(),
                input: serde_json::json!({ "path": "a.rs", "old": "x", "new": "y" }),
            },
            ContentBlock::ToolResult {
                id: "call_1".into(),
                out: ToolOutput::ok("applied"),
            },
            ContentBlock::Image {
                media_type: "image/png".into(),
                data: "aGk=".into(),
            },
        ]);

        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn wire_event_round_trips() {
        let events = vec![
            WireEvent::TextDelta("hi".into()),
            WireEvent::ToolUseStart {
                id: "1".into(),
                name: "bash".into(),
            },
            WireEvent::ToolInputDelta {
                id: "1".into(),
                json: "{\"cmd\"".into(),
            },
            WireEvent::ToolUseEnd { id: "1".into() },
            WireEvent::Usage(Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            WireEvent::Done,
        ];
        let json = serde_json::to_string(&events).unwrap();
        let back: Vec<WireEvent> = serde_json::from_str(&json).unwrap();
        assert_eq!(events, back);
    }
}
