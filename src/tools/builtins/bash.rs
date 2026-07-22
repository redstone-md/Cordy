//! `bash` — run a shell command, cross-platform, with output passed through the optimizer.
//!
//! Uses `sh -c` on unix and `powershell -NoProfile -Command` on Windows (the always-present
//! shell; `pwsh` preference can be added later). The combined stdout+stderr is compressed by the
//! native optimizer before being returned, and `ToolOutput.saved` reports the token reduction.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::core::types::ToolOutput;
use crate::tools::builtins::BgRegistry;
use crate::tools::optimize::Optimizer;
use crate::tools::{PermissionRequest, Risk, Tool, ToolCtx};

/// Which shell the platform runs commands through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Sh,
    PowerShell,
}

impl ShellKind {
    pub fn detect() -> Self {
        if cfg!(windows) {
            ShellKind::PowerShell
        } else {
            ShellKind::Sh
        }
    }

    fn program(self) -> &'static str {
        match self {
            ShellKind::Sh => "sh",
            ShellKind::PowerShell => "powershell",
        }
    }

    fn args(self, command: &str) -> Vec<String> {
        match self {
            ShellKind::Sh => vec!["-c".into(), command.into()],
            ShellKind::PowerShell => {
                // -NonInteractive stops host prompts (e.g. Invoke-WebRequest's security prompt,
                // which `curl` aliases to) from writing to the real console and shredding the TUI.
                // The prelude silences the progress bar and confirmation prompts too.
                let wrapped = format!(
                    "$ProgressPreference='SilentlyContinue';$ConfirmPreference='None';{command}"
                );
                vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    wrapped,
                ]
            }
        }
    }
}

pub struct Bash {
    optimizer: Arc<Optimizer>,
    shell: ShellKind,
    bg: BgRegistry,
}

impl Bash {
    pub fn new(optimizer: Arc<Optimizer>, bg: BgRegistry) -> Self {
        Bash {
            optimizer,
            shell: ShellKind::detect(),
            bg,
        }
    }
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command and return its combined output (optimized to save tokens)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute." },
                "background": {
                    "type": "boolean",
                    "description": "Run detached (dev servers, watchers). Returns a job id immediately; \
                                    inspect it with the `process` tool. Default false."
                }
            },
            "required": ["command"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Exec
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(command) = input["command"].as_str() else {
            return ToolOutput::error("bash: missing `command`");
        };

        let allowed = ctx
            .permission
            .request(PermissionRequest {
                risk: Risk::Exec,
                tool: "bash",
                key: command,
                summary: command,
            })
            .await;
        if !allowed {
            return ToolOutput::error("bash: denied");
        }

        // Background mode: spawn detached and hand back a job id to inspect via `process`.
        if input["background"].as_bool().unwrap_or(false) {
            return match self.bg.spawn(
                self.shell.program(),
                self.shell.args(command),
                &ctx.cwd,
                command,
            ) {
                Ok(id) => ToolOutput::ok(format!(
                    "started background job {id}. Inspect with process(action=\"output\", id=\"{id}\") \
                     or process(action=\"wait\", id=\"{id}\", until=\"<regex>\")."
                )),
                Err(e) => ToolOutput::error(format!("bash: background spawn failed: {e}")),
            };
        }

        // stdin is closed so interactive commands (npm create, prompts) get EOF instead of
        // blocking forever; a timeout + kill_on_drop is the backstop for anything that still hangs.
        let child = tokio::process::Command::new(self.shell.program())
            .args(self.shell.args(command))
            .current_dir(&ctx.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("bash: spawn failed: {e}")),
        };

        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
        let output = match tokio::time::timeout(TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return ToolOutput::error(format!("bash: {e}")),
            Err(_) => {
                return ToolOutput::error(
                    "bash: timed out after 120s and was killed — the command may be waiting for \
                     interactive input, which isn't supported. Use non-interactive flags \
                     (e.g. `npm create vite@latest name -- --template react`).",
                );
            }
        };

        let mut raw = String::new();
        raw.push_str(&String::from_utf8_lossy(&output.stdout));
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            if !raw.is_empty() && !raw.ends_with('\n') {
                raw.push('\n');
            }
            raw.push_str(&stderr);
        }
        let code = output.status.code().unwrap_or(-1);
        if !output.status.success() {
            raw.push_str(&format!("\n[exit {code}]"));
        }

        let compressed = self.optimizer.apply(command, &raw);
        ToolOutput {
            text: compressed.text,
            is_error: !output.status.success(),
            saved: compressed.saved,
        }
    }
}
