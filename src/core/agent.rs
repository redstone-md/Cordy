//! The agent loop: drive a turn to completion.
//!
//! A turn streams the model's response, assembles it into a canonical assistant [`Message`]
//! (forwarding live deltas to the UI), runs any requested tools through the [`Registry`], feeds
//! the results back, and repeats until the model stops calling tools. The same drive powers
//! sub-agents. Verified headless with a mock provider; no live key needed.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::core::types::{ChatRequest, ContentBlock, Message, Role, ToolOutput, Usage, WireEvent};
use crate::provider::Provider;
use crate::tools::{Registry, ToolCtx};

/// A display event emitted to the UI as a turn progresses.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolStarted {
        id: String,
        name: String,
        input: Value,
    },
    ToolFinished {
        id: String,
        name: String,
        output: ToolOutput,
    },
    TurnComplete {
        usage: Usage,
    },
    /// Activity from a spawned sub-agent, surfaced to the parent UI as a tree line.
    SubAgent {
        agent: String,
        note: String,
    },
    Error(String),
}

/// A conversation: the system prompt, the model in use, and the running message history.
pub struct Session {
    pub system: String,
    pub model: String,
    pub messages: Vec<Message>,
    pub total_usage: Usage,
}

impl Session {
    pub fn new(system: impl Into<String>, model: impl Into<String>) -> Self {
        Session {
            system: system.into(),
            model: model.into(),
            messages: Vec::new(),
            total_usage: Usage::default(),
        }
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.messages.push(Message::user(text));
    }

    /// Push a user message built from arbitrary content blocks (e.g. text + images).
    pub fn push_user_content(&mut self, content: Vec<ContentBlock>) {
        self.messages.push(Message {
            role: Role::User,
            content,
        });
    }
}

/// Drives conversations against a provider using a shared tool registry.
pub struct AgentLoop {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<Registry>,
    pub ctx: ToolCtx,
    pub max_tokens: Option<u32>,
}

impl AgentLoop {
    pub fn new(provider: Arc<dyn Provider>, registry: Arc<Registry>, ctx: ToolCtx) -> Self {
        AgentLoop {
            provider,
            registry,
            ctx,
            max_tokens: None,
        }
    }

    /// Run one user turn to completion: stream, assemble, run tools, feed back, repeat until the
    /// model stops requesting tools. Display events are sent to `events`. Cancelling `cancel`
    /// ends the turn at the next safe point (between the stream and the next request).
    pub async fn run_turn(
        &self,
        session: &mut Session,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        loop {
            if cancel.is_cancelled() {
                let _ = events.send(AgentEvent::TurnComplete {
                    usage: Usage::default(),
                });
                return Ok(());
            }
            let req = ChatRequest {
                model: session.model.clone(),
                system: session.system.clone(),
                messages: session.messages.clone(),
                tools: self.registry.specs(),
                max_tokens: self.max_tokens,
                temperature: None,
            };

            // Race the request establishment against cancellation too, so Esc aborts instantly even
            // while the request is still hanging before the first byte (TTFT), not only mid-stream.
            let stream = tokio::select! {
                s = self.provider.stream(req) => s?,
                _ = cancel.cancelled() => {
                    let _ = events.send(AgentEvent::TurnComplete { usage: Usage::default() });
                    return Ok(());
                }
            };
            let tap = events.clone();
            let (assistant, usage) = tokio::select! {
                r = assemble_with(stream, move |ev| {
                    // Tool starts are surfaced from the execution loop below (with full args, and
                    // right when they begin running) rather than mid-stream before the args exist.
                    let mapped = match ev {
                        WireEvent::TextDelta(s) => Some(AgentEvent::TextDelta(s.clone())),
                        WireEvent::ThinkingDelta(s) => Some(AgentEvent::ThinkingDelta(s.clone())),
                        _ => None,
                    };
                    if let Some(m) = mapped {
                        let _ = tap.send(m);
                    }
                }) => r?,
                _ = cancel.cancelled() => {
                    let _ = events.send(AgentEvent::TurnComplete { usage: Usage::default() });
                    return Ok(());
                }
            };

            session.total_usage = add_usage(session.total_usage, usage);

            // Some models (Gemma, Qwen/Hermes-style, and endpoints that render tool calls as text)
            // don't populate the structured `tool_calls` field — recover any text-encoded calls so
            // they execute normally. Only runs when the provider produced no structured tool call.
            let mut assistant = assistant;
            let has_structured = assistant
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
            if !has_structured {
                crate::core::toolcall_text::recover_text_tool_calls(&mut assistant);
            }
            session.messages.push(assistant.clone());

            let tool_uses: Vec<(String, String, Value)> = assistant
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();

            if tool_uses.is_empty() {
                // Surface a note when the model returned nothing (e.g. bad model / endpoint).
                let has_text = assistant
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { text } if !text.is_empty()));
                if !has_text {
                    let _ = events.send(AgentEvent::Error(
                        "empty response — check the model name and endpoint".into(),
                    ));
                }
                let _ = events.send(AgentEvent::TurnComplete { usage });
                return Ok(());
            }

            let mut results = Vec::with_capacity(tool_uses.len());
            let mut interrupted = false;
            for (id, name, input) in tool_uses {
                // Once interrupted, still emit a result for every remaining tool_use so the tool
                // message stays valid (providers reject a tool_use with no matching tool_result).
                if interrupted {
                    results.push(ContentBlock::ToolResult {
                        id,
                        out: ToolOutput::error("interrupted"),
                    });
                    continue;
                }
                // Announce the tool (with its args) the moment it starts running — the UI shows it
                // live instead of only after it returns.
                let _ = events.send(AgentEvent::ToolStarted {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                let output = match self.registry.get(&name) {
                    // Race the tool against cancellation so Esc aborts a hung tool immediately
                    // (dropping the future also kills a spawned child via `kill_on_drop`).
                    Some(tool) => tokio::select! {
                        o = tool.run(input, &self.ctx) => o,
                        _ = cancel.cancelled() => {
                            interrupted = true;
                            ToolOutput::error("interrupted")
                        }
                    },
                    None => ToolOutput::error(format!("unknown tool: {name}")),
                };
                let _ = events.send(AgentEvent::ToolFinished {
                    id: id.clone(),
                    name,
                    output: output.clone(),
                });
                results.push(ContentBlock::ToolResult { id, out: output });
            }
            session.messages.push(Message {
                role: Role::Tool,
                content: results,
            });
            if interrupted {
                let _ = events.send(AgentEvent::TurnComplete {
                    usage: Usage::default(),
                });
                return Ok(());
            }
        }
    }
}

fn add_usage(a: Usage, b: Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens + b.input_tokens,
        output_tokens: a.output_tokens + b.output_tokens,
        cache_read: a.cache_read + b.cache_read,
        cache_write: a.cache_write + b.cache_write,
    }
}

/// Fold a provider event stream into an assistant [`Message`] plus its [`Usage`].
pub async fn assemble(stream: BoxStream<'static, WireEvent>) -> anyhow::Result<(Message, Usage)> {
    assemble_with(stream, |_| {}).await
}

/// Like [`assemble`], but `tap` observes every event as it arrives (used to forward live deltas
/// to the UI). Ordering: text/thinking is flushed as blocks the moment a tool call starts, so a
/// `ToolUse` block lands after the text that preceded it.
pub async fn assemble_with<F: FnMut(&WireEvent)>(
    mut stream: BoxStream<'static, WireEvent>,
    mut tap: F,
) -> anyhow::Result<(Message, Usage)> {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut text = String::new();
    let mut think = String::new();
    let mut usage = Usage::default();

    let mut tool_idx: HashMap<String, usize> = HashMap::new();
    let mut tool_args: HashMap<String, String> = HashMap::new();

    fn flush(text: &mut String, think: &mut String, blocks: &mut Vec<ContentBlock>) {
        if !think.is_empty() {
            blocks.push(ContentBlock::Thinking {
                text: std::mem::take(think),
            });
        }
        if !text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: std::mem::take(text),
            });
        }
    }

    while let Some(ev) = stream.next().await {
        tap(&ev);
        match ev {
            WireEvent::TextDelta(s) => text.push_str(&s),
            WireEvent::ThinkingDelta(s) => think.push_str(&s),
            WireEvent::ToolUseStart { id, name } => {
                flush(&mut text, &mut think, &mut blocks);
                let idx = blocks.len();
                blocks.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name,
                    input: Value::Null,
                });
                tool_idx.insert(id.clone(), idx);
                tool_args.insert(id, String::new());
            }
            WireEvent::ToolInputDelta { id, json } => {
                if let Some(buf) = tool_args.get_mut(&id) {
                    buf.push_str(&json);
                }
            }
            WireEvent::ToolUseEnd { id } => {
                if let (Some(&idx), Some(args)) = (tool_idx.get(&id), tool_args.get(&id)) {
                    let input = if args.trim().is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_str(args).unwrap_or(Value::Null)
                    };
                    if let ContentBlock::ToolUse { input: slot, .. } = &mut blocks[idx] {
                        *slot = input;
                    }
                }
            }
            WireEvent::Usage(u) => usage = u,
            WireEvent::Done => break,
            WireEvent::Error(e) => return Err(anyhow::anyhow!(e)),
        }
    }

    flush(&mut text, &mut think, &mut blocks);
    Ok((Message::assistant(blocks), usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::capability::CapabilitySource;
    use crate::core::types::Caps;
    use crate::provider::Provider;
    use crate::tools::builtins::BuiltinTools;
    use crate::tools::optimize::Optimizer;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    fn stream_of(events: Vec<WireEvent>) -> BoxStream<'static, WireEvent> {
        futures::stream::iter(events).boxed()
    }

    #[tokio::test]
    async fn assembles_text_then_tool_call() {
        let events = vec![
            WireEvent::TextDelta("Editing ".into()),
            WireEvent::TextDelta("the file.".into()),
            WireEvent::ToolUseStart {
                id: "c1".into(),
                name: "edit".into(),
            },
            WireEvent::ToolInputDelta {
                id: "c1".into(),
                json: "{\"path\":".into(),
            },
            WireEvent::ToolInputDelta {
                id: "c1".into(),
                json: "\"a.rs\"}".into(),
            },
            WireEvent::ToolUseEnd { id: "c1".into() },
            WireEvent::Done,
        ];
        let (msg, _usage) = assemble(stream_of(events)).await.unwrap();
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content.len(), 2);
        assert_eq!(msg.content[0], ContentBlock::text("Editing the file."));
    }

    #[tokio::test]
    async fn error_event_propagates() {
        let err = assemble(stream_of(vec![WireEvent::Error("boom".into())]))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "boom");
    }

    /// Returns queued scripted streams, one per `stream` call.
    struct MockProvider {
        scripts: Mutex<VecDeque<Vec<WireEvent>>>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn stream(&self, _req: ChatRequest) -> anyhow::Result<BoxStream<'static, WireEvent>> {
            let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
            Ok(stream_of(script))
        }
        fn caps(&self) -> Caps {
            Caps {
                tools: true,
                ..Default::default()
            }
        }
    }

    #[tokio::test]
    async fn full_turn_runs_tool_and_feeds_result_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello world").unwrap();

        // Turn 1: model asks to read f.txt. Turn 2: model replies and stops.
        let scripts = VecDeque::from(vec![
            vec![
                WireEvent::TextDelta("reading".into()),
                WireEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "read".into(),
                },
                WireEvent::ToolInputDelta {
                    id: "t1".into(),
                    json: "{\"path\":\"f.txt\"}".into(),
                },
                WireEvent::ToolUseEnd { id: "t1".into() },
                WireEvent::Done,
            ],
            vec![
                WireEvent::TextDelta("the file says hello".into()),
                WireEvent::Done,
            ],
        ]);
        let provider = Arc::new(MockProvider {
            scripts: Mutex::new(scripts),
        });

        let mut reg = Registry::new();
        for t in BuiltinTools::new(Arc::new(Optimizer::new(true))).tools() {
            reg.register(t);
        }
        let ctx = ToolCtx::new(dir.path());
        let agent = AgentLoop::new(provider, Arc::new(reg), ctx);

        let mut session = Session::new("sys", "mock");
        session.push_user("what does f.txt say?");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        agent
            .run_turn(&mut session, &tx, &CancellationToken::new())
            .await
            .unwrap();
        drop(tx);

        let mut saw_tool_result_with_hello = false;
        let mut completed = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::ToolFinished { name, output, .. } => {
                    assert_eq!(name, "read");
                    if output.text.contains("hello world") {
                        saw_tool_result_with_hello = true;
                    }
                }
                AgentEvent::TurnComplete { .. } => completed = true,
                _ => {}
            }
        }
        assert!(
            saw_tool_result_with_hello,
            "read result should reach the UI"
        );
        assert!(completed, "turn should complete");

        // History: user, assistant(tool_use), tool(result), assistant(text).
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.messages[3].role, Role::Assistant);
    }
}
