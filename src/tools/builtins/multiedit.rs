//! `multiedit` — apply several exact replacements to one file atomically.
//!
//! Each edit's `old` must match exactly once against the file as progressively edited. If any
//! edit fails to match uniquely, nothing is written.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::core::types::ToolOutput;
use crate::tools::{PermissionRequest, Risk, Tool, ToolCtx};

pub struct MultiEdit;

#[async_trait]
impl Tool for MultiEdit {
    fn name(&self) -> &str {
        "multiedit"
    }

    fn description(&self) -> &str {
        "Apply a sequence of exact `old`->`new` replacements to a file atomically. Each `old` \
         must be unique at the point it is applied."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old": { "type": "string" },
                            "new": { "type": "string" }
                        },
                        "required": ["old", "new"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(path) = input["path"].as_str() else {
            return ToolOutput::error("multiedit: missing `path`");
        };
        let Some(edits) = input["edits"].as_array() else {
            return ToolOutput::error("multiedit: `edits` must be an array");
        };
        let full = ctx.resolve(path);
        let mut content = match tokio::fs::read_to_string(&full).await {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("multiedit: {}: {e}", full.display())),
        };

        for (i, edit) in edits.iter().enumerate() {
            let (Some(old), Some(new)) = (edit["old"].as_str(), edit["new"].as_str()) else {
                return ToolOutput::error(format!("multiedit: edit {i} needs `old` and `new`"));
            };
            match content.matches(old).count() {
                1 => content = content.replacen(old, new, 1),
                0 => {
                    return ToolOutput::error(format!("multiedit: edit {i}: `old` not found"));
                }
                n => {
                    return ToolOutput::error(format!(
                        "multiedit: edit {i}: `old` occurs {n} times (not unique)"
                    ));
                }
            }
        }

        let key = full.display().to_string();
        let summary = format!("multiedit {key} ({} edits)", edits.len());
        let ok = ctx
            .permission
            .request(PermissionRequest {
                risk: Risk::Write,
                tool: "multiedit",
                key: &key,
                summary: &summary,
            })
            .await;
        if !ok {
            return ToolOutput::error("multiedit: denied");
        }
        ctx.checkpoint("multiedit", &full);
        match tokio::fs::write(&full, &content).await {
            Ok(()) => ToolOutput::ok(format!(
                "applied {} edits to {}",
                edits.len(),
                full.display()
            )),
            Err(e) => ToolOutput::error(format!("multiedit: write {}: {e}", full.display())),
        }
    }
}
