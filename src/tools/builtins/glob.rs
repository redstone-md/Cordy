//! `glob` — list files matching a glob pattern, honoring ignore files.

use async_trait::async_trait;
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{Value, json};

use crate::core::types::ToolOutput;
use crate::tools::{Risk, Tool, ToolCtx};

const MAX_HITS: usize = 500;

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "List files matching a glob pattern (e.g. `**/*.rs`), honoring ignore files."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string", "description": "Root dir; defaults to cwd." }
            },
            "required": ["pattern"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(pattern) = input["pattern"].as_str() else {
            return ToolOutput::error("glob: missing `pattern`");
        };
        let re = match Regex::new(&glob_to_regex(pattern)) {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("glob: bad pattern: {e}")),
        };
        let root = match input["path"].as_str() {
            Some(p) => ctx.resolve(p),
            None => ctx.cwd.clone(),
        };

        let out = tokio::task::spawn_blocking(move || {
            let mut hits: Vec<String> = Vec::new();
            for entry in WalkBuilder::new(&root).require_git(false).build().flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                let rel = rel.to_string_lossy().replace('\\', "/");
                if re.is_match(&rel) {
                    hits.push(rel);
                    if hits.len() >= MAX_HITS {
                        break;
                    }
                }
            }
            hits.sort();
            hits
        })
        .await
        .unwrap_or_default();

        if out.is_empty() {
            ToolOutput::ok("no matches")
        } else {
            ToolOutput::ok(out.join("\n"))
        }
    }
}

/// Convert a glob to an anchored regex. Supports `**` (any dirs), `*` (within a segment), `?`.
fn glob_to_regex(glob: &str) -> String {
    let mut re = String::from("^");
    let bytes = glob.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] as char {
            '*' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    re.push_str(".*");
                    i += 1; // consume second '*'
                    // swallow a following '/' so `**/x` matches `x` at the root too
                    if i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        i += 1;
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                re.push('\\');
                re.push(bytes[i] as char);
            }
            c => re.push(c),
        }
        i += 1;
    }
    re.push('$');
    re
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_regex_matches_expected() {
        let re = Regex::new(&glob_to_regex("**/*.rs")).unwrap();
        assert!(re.is_match("src/main.rs"));
        assert!(re.is_match("main.rs"));
        assert!(!re.is_match("main.txt"));

        let re2 = Regex::new(&glob_to_regex("src/*.rs")).unwrap();
        assert!(re2.is_match("src/a.rs"));
        assert!(!re2.is_match("src/sub/a.rs"));
    }
}
