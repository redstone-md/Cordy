//! Tool layer: one trait for builtins, MCP tools, and sub-agents alike.
//!
//! The agent loop resolves tools through the [`Registry`] and never distinguishes their
//! source. Builtin implementations, the native output optimizer, and permission wiring land in
//! later build-order steps; this module defines the trait surface and the registry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use crate::core::checkpoint::CheckpointStore;
use crate::core::types::{ToolOutput, ToolSpec};

pub mod apply_patch;
pub mod builtins;
pub mod optimize;
pub mod subagent;

/// Coarse risk class a tool declares; drives the permission engine's defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    ReadOnly,
    Write,
    Exec,
    Network,
}

/// A request to perform a risky tool action. `key` is the matchable target (a command, a path)
/// used by rule engines; `summary` is the human-readable detail (a diff, the command line).
pub struct PermissionRequest<'a> {
    pub risk: Risk,
    pub tool: &'a str,
    pub key: &'a str,
    pub summary: &'a str,
}

/// Decides whether a risky tool action may proceed. Implementations range from a headless
/// auto-approver to a rule engine wrapping the interactive TUI (which shows the summary and
/// waits for a keypress).
#[async_trait]
pub trait Permission: Send + Sync {
    async fn request(&self, req: PermissionRequest<'_>) -> bool;
}

/// Approves everything. Used in tests and headless/AutoAccept mode.
pub struct AutoApprove;

#[async_trait]
impl Permission for AutoApprove {
    async fn request(&self, _req: PermissionRequest<'_>) -> bool {
        true
    }
}

/// Denies everything. Used to model plan/read-only mode in tests.
pub struct DenyAll;

#[async_trait]
impl Permission for DenyAll {
    async fn request(&self, _req: PermissionRequest<'_>) -> bool {
        false
    }
}

/// Execution context handed to a tool: the working directory plus the permission gate that
/// `Write`/`Exec` tools consult before mutating anything. Grows in later steps (event bus
/// sender, cancellation token, shell kind).
#[derive(Clone)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub permission: Arc<dyn Permission>,
    /// Shared workspace-checkpoint store; write tools snapshot files here before mutating them.
    pub checkpoints: Arc<Mutex<CheckpointStore>>,
}

impl ToolCtx {
    /// Context with an auto-approving permission gate (tests / non-interactive use).
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        ToolCtx {
            cwd: cwd.into(),
            permission: Arc::new(AutoApprove),
            checkpoints: Arc::new(Mutex::new(CheckpointStore::new())),
        }
    }

    pub fn with_permission(cwd: impl Into<PathBuf>, permission: Arc<dyn Permission>) -> Self {
        ToolCtx {
            cwd: cwd.into(),
            permission,
            checkpoints: Arc::new(Mutex::new(CheckpointStore::new())),
        }
    }

    /// Snapshot `path`'s current content before it is modified, for later rewind.
    pub fn checkpoint(&self, label: &str, path: &Path) {
        if let Ok(mut store) = self.checkpoints.lock() {
            store.snapshot(label, std::slice::from_ref(&path.to_path_buf()));
        }
    }

    /// Resolve a possibly-relative path against the working directory.
    pub fn resolve(&self, path: impl AsRef<Path>) -> PathBuf {
        let p = path.as_ref();
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;

    /// JSON Schema for the tool's input, advertised to the model.
    fn schema(&self) -> Value;

    /// Risk class; defaults to read-only (never prompts for permission).
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput;

    /// Canonical spec derived from the trait methods.
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.schema(),
        }
    }
}

/// Name -> tool lookup shared across builtin, MCP, and sub-agent sources.
#[derive(Default)]
pub struct Registry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Registry::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Specs for every registered tool, for advertising to a provider.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo the input back"
        }
        fn schema(&self) -> Value {
            serde_json::json!({ "type": "object", "properties": { "text": { "type": "string" } } })
        }
        async fn run(&self, input: Value, _ctx: &ToolCtx) -> ToolOutput {
            ToolOutput::ok(input["text"].as_str().unwrap_or_default())
        }
    }

    #[tokio::test]
    async fn registry_registers_and_runs() {
        let mut reg = Registry::new();
        reg.register(Arc::new(Echo));
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.specs()[0].name, "echo");

        let tool = reg.get("echo").unwrap();
        let out = tool
            .run(serde_json::json!({ "text": "hi" }), &ToolCtx::new("."))
            .await;
        assert_eq!(out.text, "hi");
        assert!(!out.is_error);
    }
}
