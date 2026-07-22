//! `rewind` — undo recent file edits by restoring workspace checkpoints.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::core::types::ToolOutput;
use crate::tools::{PermissionRequest, Risk, Tool, ToolCtx};

pub struct Rewind;

#[async_trait]
impl Tool for Rewind {
    fn name(&self) -> &str {
        "rewind"
    }

    fn description(&self) -> &str {
        "Undo the last N file edits by restoring workspace checkpoints (default 1)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "steps": { "type": "integer", "minimum": 1, "description": "How many edits to undo." }
            }
        })
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let steps = input["steps"].as_u64().unwrap_or(1).max(1) as usize;
        let summary = format!("rewind {steps} edit(s)");
        let ok = ctx
            .permission
            .request(PermissionRequest {
                risk: Risk::Write,
                tool: "rewind",
                key: "rewind",
                summary: &summary,
            })
            .await;
        if !ok {
            return ToolOutput::error("rewind: denied");
        }
        let result = { ctx.checkpoints.lock().map(|mut s| s.rewind_last(steps)) };
        match result {
            Ok(Ok(n)) => ToolOutput::ok(format!("rewound {n} file(s)")),
            Ok(Err(e)) => ToolOutput::error(format!("rewind: {e}")),
            Err(_) => ToolOutput::error("rewind: checkpoint store unavailable"),
        }
    }
}
