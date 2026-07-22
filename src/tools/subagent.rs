//! The `task` tool: spawn a sub-agent that runs the same [`AgentLoop`] in an isolated session.
//!
//! Context isolation: the child sees only its own prompt and returns just its final text, so the
//! parent's context stays small. A global semaphore caps real concurrency. The child's tool set
//! is the base capability source, optionally narrowed to the agent type's `tools` list.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::agents::AgentDef;
use crate::core::agent::{AgentEvent, AgentLoop, Session};
use crate::core::capability::CapabilitySource;
use crate::core::types::{ContentBlock, Message, Role, ToolOutput};
use crate::provider::Provider;
use crate::tools::{Registry, Risk, Tool, ToolCtx};

/// A paused child kept alive for continuation: its session plus the tool registry it was given.
struct ChildState {
    session: Session,
    registry: Arc<Registry>,
}

pub struct SubAgentTool {
    provider: Arc<dyn Provider>,
    base_tools: Arc<dyn CapabilitySource>,
    ctx: ToolCtx,
    semaphore: Arc<Semaphore>,
    defs: Arc<Vec<AgentDef>>,
    default_model: String,
    /// Parent UI event sink; child activity is surfaced here as tree lines.
    parent_events: UnboundedSender<AgentEvent>,
    /// Live children kept for continuation, keyed by handle.
    children: Arc<Mutex<HashMap<String, ChildState>>>,
    next_handle: Arc<AtomicUsize>,
}

impl SubAgentTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        base_tools: Arc<dyn CapabilitySource>,
        ctx: ToolCtx,
        semaphore: Arc<Semaphore>,
        defs: Arc<Vec<AgentDef>>,
        default_model: impl Into<String>,
        parent_events: UnboundedSender<AgentEvent>,
    ) -> Self {
        SubAgentTool {
            provider,
            base_tools,
            ctx,
            semaphore,
            defs,
            default_model: default_model.into(),
            parent_events,
            children: Arc::new(Mutex::new(HashMap::new())),
            next_handle: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Run one turn of a child to completion, forwarding its activity to the parent UI.
    async fn drive(
        &self,
        agent: &AgentLoop,
        session: &mut Session,
        agent_name: &str,
    ) -> anyhow::Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let parent = self.parent_events.clone();
        let name = agent_name.to_string();
        let _ = parent.send(AgentEvent::SubAgent {
            agent: name.clone(),
            note: "started".into(),
        });
        let forwarder = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if let Some(note) = summarize_child(&ev) {
                    let _ = parent.send(AgentEvent::SubAgent {
                        agent: name.clone(),
                        note,
                    });
                }
            }
        });
        let result = agent
            .run_turn(session, &tx, &CancellationToken::new())
            .await;
        drop(tx);
        let _ = forwarder.await;
        result
    }

    /// Build the child's registry: base tools, narrowed to `allow` when present.
    fn child_registry(&self, allow: &Option<Vec<String>>) -> Registry {
        let mut reg = Registry::new();
        for tool in self.base_tools.tools() {
            let keep = match allow {
                Some(list) => list.iter().any(|n| n == tool.name()),
                None => true,
            };
            if keep {
                reg.register(tool);
            }
        }
        reg
    }
}

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Spawn a sub-agent (by `agent_type`) for a focused `prompt`; returns its final answer and \
         a `handle`. Pass `continue` = that handle (instead of `agent_type`) to send a follow-up \
         to the same live sub-agent."
    }

    fn schema(&self) -> Value {
        let types: Vec<&str> = self.defs.iter().map(|d| d.name.as_str()).collect();
        json!({
            "type": "object",
            "properties": {
                "agent_type": { "type": "string", "enum": types },
                "prompt": { "type": "string" },
                "continue": { "type": "string", "description": "Handle of a live sub-agent to continue." }
            },
            "required": ["prompt"]
        })
    }

    fn risk(&self) -> Risk {
        // Orchestration only; the child's own tools gate their own writes/exec.
        Risk::ReadOnly
    }

    async fn run(&self, input: Value, _ctx: &ToolCtx) -> ToolOutput {
        let Some(prompt) = input["prompt"].as_str() else {
            return ToolOutput::error("task: requires `prompt`");
        };
        let _permit = self.semaphore.acquire().await; // cap real concurrency

        // Continuation: send a follow-up to a live child kept from an earlier `task`.
        if let Some(handle) = input["continue"].as_str() {
            let state = { self.children.lock().unwrap().remove(handle) };
            let Some(mut state) = state else {
                return ToolOutput::error(format!("task: unknown handle `{handle}`"));
            };
            state.session.push_user(prompt);
            let agent = AgentLoop::new(
                self.provider.clone(),
                state.registry.clone(),
                self.ctx.clone(),
            );
            if let Err(e) = self.drive(&agent, &mut state.session, handle).await {
                return ToolOutput::error(format!("task: continuation failed: {e}"));
            }
            let text = final_text(&state.session.messages);
            self.children
                .lock()
                .unwrap()
                .insert(handle.to_string(), state);
            return ToolOutput::ok(format!("handle: {handle}\n{text}"));
        }

        // Fresh spawn.
        let Some(agent_type) = input["agent_type"].as_str() else {
            return ToolOutput::error("task: requires `agent_type` (or `continue`)");
        };
        let Some(def) = self.defs.iter().find(|d| d.name == agent_type) else {
            return ToolOutput::error(format!("task: unknown agent_type `{agent_type}`"));
        };

        let registry = Arc::new(self.child_registry(&def.tools));
        let model = def
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());
        let agent = AgentLoop::new(self.provider.clone(), registry.clone(), self.ctx.clone());
        let mut session = Session::new(def.system_prompt.clone(), model);
        session.push_user(prompt);

        if let Err(e) = self.drive(&agent, &mut session, agent_type).await {
            return ToolOutput::error(format!("task: sub-agent failed: {e}"));
        }
        let text = final_text(&session.messages);
        let handle = format!("child{}", self.next_handle.fetch_add(1, Ordering::Relaxed));
        self.children
            .lock()
            .unwrap()
            .insert(handle.clone(), ChildState { session, registry });
        ToolOutput::ok(format!("handle: {handle}\n{text}"))
    }
}

/// Compact one-line summary of a child event for the parent's tree view (text deltas skipped).
fn summarize_child(ev: &AgentEvent) -> Option<String> {
    match ev {
        AgentEvent::ToolStarted { name, .. } => Some(format!("running {name}")),
        AgentEvent::ToolFinished { name, .. } => Some(format!("{name} done")),
        AgentEvent::TurnComplete { .. } => Some("done".into()),
        AgentEvent::Error(e) => Some(format!("error: {e}")),
        _ => None,
    }
}

/// The last assistant message's text, joined.
fn final_text(messages: &[Message]) -> String {
    for m in messages.iter().rev() {
        if m.role == Role::Assistant {
            let text: String = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if !text.is_empty() {
                return text;
            }
        }
    }
    "(sub-agent produced no text)".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{Caps, ChatRequest, WireEvent};
    use crate::tools::builtins::BuiltinTools;
    use crate::tools::optimize::Optimizer;
    use futures::StreamExt;
    use futures::stream::BoxStream;

    struct CannedProvider(Vec<WireEvent>);

    #[async_trait]
    impl Provider for CannedProvider {
        async fn stream(&self, _req: ChatRequest) -> anyhow::Result<BoxStream<'static, WireEvent>> {
            Ok(futures::stream::iter(self.0.clone()).boxed())
        }
        fn caps(&self) -> Caps {
            Caps::default()
        }
    }

    #[tokio::test]
    async fn task_runs_subagent_and_returns_final_text() {
        let provider = Arc::new(CannedProvider(vec![
            WireEvent::TextDelta("sub-agent answer".into()),
            WireEvent::Done,
        ]));
        let base: Arc<dyn CapabilitySource> =
            Arc::new(BuiltinTools::new(Arc::new(Optimizer::new(true))));
        let defs = Arc::new(vec![AgentDef {
            name: "helper".into(),
            description: "helps".into(),
            tools: Some(vec!["read".into()]),
            model: None,
            system_prompt: "you help".into(),
        }]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tool = SubAgentTool::new(
            provider,
            base,
            ToolCtx::new("."),
            Arc::new(Semaphore::new(2)),
            defs,
            "mock",
            tx,
        );

        let out = tool
            .run(
                json!({ "agent_type": "helper", "prompt": "do it" }),
                &ToolCtx::new("."),
            )
            .await;
        assert!(out.text.contains("sub-agent answer"), "{}", out.text);
        assert!(out.text.contains("handle: child0"), "{}", out.text);
        assert!(!out.is_error);

        // Continue the same live sub-agent via its handle.
        let cont = tool
            .run(
                json!({ "continue": "child0", "prompt": "more" }),
                &ToolCtx::new("."),
            )
            .await;
        assert!(!cont.is_error, "{}", cont.text);
        assert!(cont.text.contains("handle: child0"), "{}", cont.text);

        // Unknown handle errors.
        let bad = tool
            .run(
                json!({ "continue": "nope", "prompt": "x" }),
                &ToolCtx::new("."),
            )
            .await;
        assert!(bad.is_error);
    }

    #[tokio::test]
    async fn unknown_agent_type_errors() {
        let provider = Arc::new(CannedProvider(vec![WireEvent::Done]));
        let base: Arc<dyn CapabilitySource> =
            Arc::new(BuiltinTools::new(Arc::new(Optimizer::new(true))));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tool = SubAgentTool::new(
            provider,
            base,
            ToolCtx::new("."),
            Arc::new(Semaphore::new(1)),
            Arc::new(vec![]),
            "mock",
            tx,
        );
        let out = tool
            .run(
                json!({ "agent_type": "nope", "prompt": "x" }),
                &ToolCtx::new("."),
            )
            .await;
        assert!(out.is_error);
    }
}
