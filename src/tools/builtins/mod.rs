//! Builtin tools and the [`BuiltinTools`] capability source that registers them.

use std::sync::Arc;

use crate::core::capability::CapabilitySource;
use crate::tools::Tool;
use crate::tools::optimize::Optimizer;

mod bash;
mod edit;
mod glob;
mod grep;
mod ls;
mod multiedit;
mod process;
mod read;
mod rewind;
mod todo;
mod web;
mod write;

pub use bash::Bash;
pub use edit::Edit;
pub use glob::Glob;
pub use grep::Grep;
pub use ls::Ls;
pub use multiedit::MultiEdit;
pub use process::{BgRegistry, Process};
pub use read::Read;
pub use rewind::Rewind;
pub use todo::{Todo, TodoList};
pub use web::{WebFetch, WebSearch};
pub use write::Write;

/// The built-in toolset. Shares the [`Optimizer`] with the `bash` tool so command output is
/// compressed on the way back to the model, and a [`BgRegistry`] between `bash` and `process`
/// so background jobs started by one are visible to the other.
pub struct BuiltinTools {
    optimizer: Arc<Optimizer>,
    bg: BgRegistry,
}

impl BuiltinTools {
    pub fn new(optimizer: Arc<Optimizer>) -> Self {
        Self::with_bg(optimizer, BgRegistry::default())
    }

    /// Construct with a shared [`BgRegistry`] so the caller (the TUI) can also observe background
    /// jobs (e.g. for the status bar).
    pub fn with_bg(optimizer: Arc<Optimizer>, bg: BgRegistry) -> Self {
        BuiltinTools { optimizer, bg }
    }
}

impl CapabilitySource for BuiltinTools {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(Read),
            Arc::new(Write),
            Arc::new(Edit),
            Arc::new(MultiEdit),
            Arc::new(Grep),
            Arc::new(Glob),
            Arc::new(Ls),
            Arc::new(Todo::new(TodoList::default())),
            Arc::new(Rewind),
            Arc::new(WebSearch),
            Arc::new(WebFetch),
            Arc::new(Bash::new(self.optimizer.clone(), self.bg.clone())),
            Arc::new(Process::new(self.bg.clone())),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::capability::CapabilitySource;
    use crate::tools::{Registry, ToolCtx};
    use serde_json::json;

    fn registry() -> (Registry, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::new();
        let src = BuiltinTools::new(Arc::new(Optimizer::new(true)));
        for t in src.tools() {
            reg.register(t);
        }
        (reg, dir)
    }

    #[tokio::test]
    async fn edit_requires_unique_match() {
        let (reg, dir) = registry();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "foo\nbar\nfoo\n").unwrap();
        let ctx = ToolCtx::new(dir.path());
        let edit = reg.get("edit").unwrap();

        // multiple matches -> error
        let out = edit
            .run(json!({ "path": "a.txt", "old": "foo", "new": "baz" }), &ctx)
            .await;
        assert!(out.is_error, "duplicate `old` should error");

        // zero matches -> error
        let out = edit
            .run(json!({ "path": "a.txt", "old": "nope", "new": "x" }), &ctx)
            .await;
        assert!(out.is_error, "missing `old` should error");

        // unique match -> applied
        let out = edit
            .run(json!({ "path": "a.txt", "old": "bar", "new": "BAR" }), &ctx)
            .await;
        assert!(!out.is_error, "unique `old` should apply: {}", out.text);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "foo\nBAR\nfoo\n");
    }

    #[tokio::test]
    async fn read_returns_numbered_lines() {
        let (reg, dir) = registry();
        std::fs::write(dir.path().join("b.txt"), "one\ntwo\nthree\n").unwrap();
        let ctx = ToolCtx::new(dir.path());
        let out = reg
            .get("read")
            .unwrap()
            .run(json!({ "path": "b.txt", "offset": 2, "limit": 1 }), &ctx)
            .await;
        assert!(out.text.contains("     2\ttwo"));
        assert!(!out.text.contains("three"));
    }

    #[tokio::test]
    async fn grep_finds_matches_and_honors_ignore() {
        let (reg, dir) = registry();
        std::fs::write(dir.path().join("code.rs"), "fn wanted() {}\nother\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "skip.rs\n").unwrap();
        std::fs::write(dir.path().join("skip.rs"), "fn wanted() {}\n").unwrap();
        let ctx = ToolCtx::new(dir.path());
        let out = reg
            .get("grep")
            .unwrap()
            .run(json!({ "pattern": "fn wanted" }), &ctx)
            .await;
        assert!(out.text.contains("code.rs"));
        assert!(
            !out.text.contains("skip.rs"),
            "ignored file must be skipped"
        );
    }

    #[tokio::test]
    async fn bash_runs_and_reports_output() {
        let (reg, dir) = registry();
        let ctx = ToolCtx::new(dir.path());
        let out = reg
            .get("bash")
            .unwrap()
            .run(json!({ "command": "echo cordy_ok" }), &ctx)
            .await;
        assert!(out.text.contains("cordy_ok"), "got: {}", out.text);
        assert!(!out.is_error);
    }
}
