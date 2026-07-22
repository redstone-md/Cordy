//! `write` — create or overwrite a file (permission-gated).

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::core::types::ToolOutput;
use crate::tools::{PermissionRequest, Risk, Tool, ToolCtx};

pub struct Write;

#[async_trait]
impl Tool for Write {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Create or overwrite a file with the given contents."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let (Some(path), Some(content)) = (input["path"].as_str(), input["content"].as_str())
        else {
            return ToolOutput::error("write: requires `path` and `content`");
        };
        let full = ctx.resolve(path);
        let key = full.display().to_string();
        let summary = format!("write {key} ({} bytes)", content.len());
        let ok = ctx
            .permission
            .request(PermissionRequest {
                risk: Risk::Write,
                tool: "write",
                key: &key,
                summary: &summary,
            })
            .await;
        if !ok {
            return ToolOutput::error("write: denied");
        }
        ctx.checkpoint("write", &full);
        if let Some(parent) = full.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolOutput::error(format!("write: mkdir {}: {e}", parent.display()));
        }
        match tokio::fs::write(&full, content).await {
            Ok(()) => ToolOutput::ok(format!("wrote {}", full.display())),
            Err(e) => ToolOutput::error(format!("write: {}: {e}", full.display())),
        }
    }
}
