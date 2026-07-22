//! `todo` — a shared task checklist the agent maintains across a turn.
//!
//! Holds the list in memory (shared via `Arc`) so the model can set and re-read its plan. The
//! TUI can render it in later steps. Setting the list replaces it; the rendered checklist is
//! returned.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::core::types::ToolOutput;
use crate::tools::{Risk, Tool, ToolCtx};

#[derive(Clone)]
struct Item {
    content: String,
    status: String,
}

/// In-memory checklist shared with the tool.
#[derive(Clone, Default)]
pub struct TodoList {
    items: Arc<Mutex<Vec<Item>>>,
}

pub struct Todo {
    list: TodoList,
}

impl Todo {
    pub fn new(list: TodoList) -> Self {
        Todo { list }
    }
}

#[async_trait]
impl Tool for Todo {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Set the task checklist for the current work. Replaces the list; returns it rendered."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "done"]
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    async fn run(&self, input: Value, _ctx: &ToolCtx) -> ToolOutput {
        let Some(todos) = input["todos"].as_array() else {
            return ToolOutput::error("todo: `todos` must be an array");
        };
        let items: Vec<Item> = todos
            .iter()
            .filter_map(|t| {
                Some(Item {
                    content: t["content"].as_str()?.to_string(),
                    status: t["status"].as_str().unwrap_or("pending").to_string(),
                })
            })
            .collect();

        let rendered = render(&items);
        *self.list.items.lock().unwrap() = items;
        ToolOutput::ok(rendered)
    }
}

fn render(items: &[Item]) -> String {
    if items.is_empty() {
        return "(no tasks)".to_string();
    }
    items
        .iter()
        .map(|i| {
            let mark = match i.status.as_str() {
                "done" => "[x]",
                "in_progress" => "[~]",
                _ => "[ ]",
            };
            format!("{mark} {}", i.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sets_and_renders_list() {
        let todo = Todo::new(TodoList::default());
        let out = todo
            .run(
                json!({ "todos": [
                    { "content": "scan", "status": "done" },
                    { "content": "fix", "status": "in_progress" }
                ]}),
                &ToolCtx::new("."),
            )
            .await;
        assert!(out.text.contains("[x] scan"));
        assert!(out.text.contains("[~] fix"));
    }
}
