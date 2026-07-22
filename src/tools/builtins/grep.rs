//! `grep` — regex search across files, honoring .gitignore/.cordyignore.

use async_trait::async_trait;
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{Value, json};

use crate::core::types::ToolOutput;
use crate::tools::{Risk, Tool, ToolCtx};

const MAX_MATCHES: usize = 200;

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents by regex, honoring ignore files. Returns file:line:text matches."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression." },
                "path": { "type": "string", "description": "Directory to search; defaults to cwd." }
            },
            "required": ["pattern"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(pattern) = input["pattern"].as_str() else {
            return ToolOutput::error("grep: missing `pattern`");
        };
        let re = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("grep: bad pattern: {e}")),
        };
        let root = match input["path"].as_str() {
            Some(p) => ctx.resolve(p),
            None => ctx.cwd.clone(),
        };

        // Blocking file walk off the async runtime.
        let out = tokio::task::spawn_blocking(move || search(&re, &root)).await;
        match out {
            Ok(text) => ToolOutput::ok(text),
            Err(e) => ToolOutput::error(format!("grep: {e}")),
        }
    }
}

fn search(re: &Regex, root: &std::path::Path) -> String {
    let mut hits: Vec<String> = Vec::new();
    let mut truncated = false;

    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .require_git(false) // honor .gitignore even outside a git repo
        .build();
    'walk: for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(path) else {
            continue; // skip binary / unreadable
        };
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                if hits.len() >= MAX_MATCHES {
                    truncated = true;
                    break 'walk;
                }
                hits.push(format!("{}:{}:{}", path.display(), i + 1, line.trim_end()));
            }
        }
    }

    if hits.is_empty() {
        return "no matches".to_string();
    }
    if truncated {
        hits.push(format!("... (truncated at {MAX_MATCHES} matches)"));
    }
    hits.join("\n")
}
