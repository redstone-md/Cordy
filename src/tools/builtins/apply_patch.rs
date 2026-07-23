//! `apply_patch` — multi-file edits in one call.
//!
//! `edit` replaces one unique string in one file. `apply_patch` takes a whole patch: several files,
//! several hunks each, adds, deletes and renames, matched tolerantly against the current contents.
//! It is the right tool for a real change; `edit` is the right tool for a one-line fix.
//!
//! The patch is fully resolved before anything is written, so the diff shown for approval is exactly
//! what lands, and a patch that doesn't fit leaves the worktree untouched.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::core::types::ToolOutput;
use crate::tools::apply_patch::plan;
use crate::tools::{PermissionRequest, Risk, Tool, ToolCtx};

pub struct ApplyPatch;

#[async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Edit files with a patch: add, update (optionally renaming), and delete several files in \
         one call. Prefer this over `edit` for multi-hunk or multi-file changes. The patch body \
         goes in `input`, wrapped in `*** Begin Patch` / `*** End Patch`."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "The patch text, starting with '*** Begin Patch' and ending with '*** End Patch'. File paths are relative to the working directory."
                }
            },
            "required": ["input"],
            "additionalProperties": false
        })
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(patch) = input["input"].as_str() else {
            return ToolOutput::error("apply_patch: requires `input` (the patch text)");
        };

        let plan = match plan(patch, &ctx.cwd) {
            Ok(plan) => plan,
            Err(e) => return ToolOutput::error(format!("apply_patch: {e}")),
        };
        if plan.is_empty() {
            return ToolOutput::ok("apply_patch: the patch contained no changes");
        }

        // One approval for the whole patch — it is applied as a unit, so it is approved as a unit.
        let key = plan
            .changes
            .iter()
            .map(|c| c.target().display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let summary = format!("apply_patch {key}\n{}", plan.diff());
        let approved = ctx
            .permission
            .request(PermissionRequest {
                risk: Risk::Write,
                tool: "apply_patch",
                key: &key,
                summary: &summary,
            })
            .await;
        if !approved {
            return ToolOutput::error("apply_patch: denied");
        }

        // Snapshot every touched path first so the whole patch can be rewound as one step.
        for change in &plan.changes {
            for path in change.touched() {
                ctx.checkpoint("apply_patch", &path);
            }
        }
        match plan.apply() {
            Ok(summary) => ToolOutput::ok(format!(
                "applied patch to {} file(s)\n{summary}",
                plan.changes.len()
            )),
            Err(e) => ToolOutput::error(format!("apply_patch: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::DenyAll;
    use std::sync::Arc;

    #[tokio::test]
    async fn applies_a_multi_file_patch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let ctx = ToolCtx::new(dir.path());

        let out = ApplyPatch
            .run(
                json!({ "input": "*** Begin Patch\n\
                     *** Add File: b.txt\n\
                     +fresh\n\
                     *** Update File: a.txt\n\
                     @@\n\
                     -two\n\
                     +TWO\n\
                     *** End Patch" }),
                &ctx,
            )
            .await;

        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\nTWO\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "fresh\n"
        );
        assert!(out.text.contains("2 file(s)"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_denied_patch_leaves_the_worktree_untouched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let ctx = ToolCtx::with_permission(dir.path(), Arc::new(DenyAll));

        let out = ApplyPatch
            .run(
                json!({ "input": "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** End Patch" }),
                &ctx,
            )
            .await;

        assert!(out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\n"
        );
    }

    #[tokio::test]
    async fn a_malformed_patch_reports_what_was_expected() {
        let dir = tempfile::tempdir().unwrap();
        let out = ApplyPatch
            .run(
                json!({ "input": "just some text" }),
                &ToolCtx::new(dir.path()),
            )
            .await;
        assert!(out.is_error);
        assert!(out.text.contains("*** Begin Patch"), "{}", out.text);
    }
}
