//! `ls` — list a directory's entries.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::core::types::ToolOutput;
use crate::tools::{Risk, Tool, ToolCtx};

pub struct Ls;

#[async_trait]
impl Tool for Ls {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List the entries of a directory (defaults to cwd). Directories end with `/`."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory; defaults to cwd." }
            }
        })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let dir = match input["path"].as_str() {
            Some(p) => ctx.resolve(p),
            None => ctx.cwd.clone(),
        };
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) => return ToolOutput::error(format!("ls: {}: {e}", dir.display())),
        };
        let mut names: Vec<String> = Vec::new();
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            names.push(if is_dir { format!("{name}/") } else { name });
        }
        names.sort();
        if names.is_empty() {
            ToolOutput::ok("(empty)")
        } else {
            ToolOutput::ok(names.join("\n"))
        }
    }
}
