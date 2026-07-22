//! `edit` — exact single-occurrence string replacement in a file.
//!
//! The search string must match exactly once: zero matches or multiple matches are errors with
//! guidance, which forces the model to supply enough surrounding context to be unambiguous.
//! Diff rendering + permission gating are layered on by the TUI/permission steps; the tool
//! itself performs the replacement and reports a compact summary.

use async_trait::async_trait;
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};

use crate::core::types::ToolOutput;
use crate::tools::{PermissionRequest, Risk, Tool, ToolCtx};

/// A compact unified diff between `old` and `new` for the permission preview.
fn unified_diff(old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        out.push_str(sign);
        out.push_str(change.value());
        if !change.value().ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

pub struct Edit;

#[async_trait]
impl Tool for Edit {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace an exact, unique string in a file with a new string. `old` must occur exactly once."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old": { "type": "string", "description": "Exact text to replace; must be unique in the file." },
                "new": { "type": "string", "description": "Replacement text." }
            },
            "required": ["path", "old", "new"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let (Some(path), Some(old), Some(new)) = (
            input["path"].as_str(),
            input["old"].as_str(),
            input["new"].as_str(),
        ) else {
            return ToolOutput::error("edit: requires `path`, `old`, `new`");
        };
        let full = ctx.resolve(path);
        let content = match tokio::fs::read_to_string(&full).await {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("edit: {}: {e}", full.display())),
        };

        let count = content.matches(old).count();
        match count {
            0 => ToolOutput::error(format!(
                "edit: `old` not found in {}. Provide the exact existing text.",
                full.display()
            )),
            1 => {
                let updated = content.replacen(old, new, 1);
                let diff = unified_diff(old, new);
                let key = full.display().to_string();
                let summary = format!("edit {key}\n{diff}");
                let ok = ctx
                    .permission
                    .request(PermissionRequest {
                        risk: Risk::Write,
                        tool: "edit",
                        key: &key,
                        summary: &summary,
                    })
                    .await;
                if !ok {
                    return ToolOutput::error("edit: denied");
                }
                ctx.checkpoint("edit", &full);
                if let Err(e) = tokio::fs::write(&full, &updated).await {
                    return ToolOutput::error(format!("edit: write {}: {e}", full.display()));
                }
                let delta = new.lines().count() as i64 - old.lines().count() as i64;
                ToolOutput::ok(format!(
                    "edited {} (1 replacement, {:+} lines)",
                    full.display(),
                    delta
                ))
            }
            n => ToolOutput::error(format!(
                "edit: `old` occurs {n} times in {}; add surrounding context to make it unique.",
                full.display()
            )),
        }
    }
}
