//! `read` — read a file, optionally a line window, returned with line numbers.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::core::types::ToolOutput;
use crate::tools::{Risk, Tool, ToolCtx};

pub struct Read;

#[async_trait]
impl Tool for Read {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file's contents (optionally a line range), with line numbers."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path, absolute or relative to cwd." },
                "offset": { "type": "integer", "description": "1-based first line to read." },
                "limit": { "type": "integer", "description": "Max lines to read." }
            },
            "required": ["path"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(path) = input["path"].as_str() else {
            return ToolOutput::error("read: missing `path`");
        };
        let full = ctx.resolve(path);
        let content = match tokio::fs::read_to_string(&full).await {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("read: {}: {e}", full.display())),
        };

        let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = input["limit"].as_u64().map(|l| l as usize);

        let mut out = String::new();
        for (i, line) in content.lines().enumerate() {
            let lineno = i + 1;
            if lineno < offset {
                continue;
            }
            if let Some(lim) = limit
                && lineno >= offset + lim
            {
                break;
            }
            out.push_str(&format!("{lineno:>6}\t{line}\n"));
        }
        ToolOutput::ok(out)
    }
}
