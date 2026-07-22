//! MCP client — connect to Model Context Protocol servers and expose their tools.
//!
//! Feature-gated behind `mcp` (pulls the heavy `rmcp` SDK). A server's tools are discovered on
//! connect and each is wrapped as a [`Tool`] whose name is namespaced `server__tool` to avoid
//! collisions; calling it forwards to the server. [`McpCapability`] is a
//! [`CapabilitySource`](crate::core::capability::CapabilitySource). The connection
//! (`RunningService`) must be kept alive by the caller for the tools to work.

use std::sync::Arc;

use async_trait::async_trait;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::core::capability::CapabilitySource;
use crate::core::types::ToolOutput;
use crate::tools::{Risk, Tool, ToolCtx};

/// Namespaced tool name to avoid collisions across servers.
pub fn qualify(server: &str, tool: &str) -> String {
    format!("{server}__{tool}")
}

/// A tool provided by an MCP server, invoked over the live connection.
pub struct McpTool {
    peer: Peer<RoleClient>,
    /// Namespaced name advertised to the model.
    qualified: String,
    /// Real tool name on the server.
    real: String,
    description: String,
    schema: Value,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    fn risk(&self) -> Risk {
        // Unknown side effects; treat as network/exec so it is permission-gated.
        Risk::Network
    }

    async fn run(&self, input: Value, _ctx: &ToolCtx) -> ToolOutput {
        let mut params = CallToolRequestParams::new(self.real.clone());
        if let Some(obj) = input.as_object() {
            params = params.with_arguments(obj.clone());
        }
        match self.peer.call_tool(params).await {
            Ok(result) => {
                let text = result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                    .collect::<Vec<_>>()
                    .join("\n");
                if result.is_error.unwrap_or(false) {
                    ToolOutput::error(text)
                } else {
                    ToolOutput::ok(text)
                }
            }
            Err(e) => ToolOutput::error(format!("mcp {}: {e}", self.qualified)),
        }
    }
}

/// A live MCP server connection plus its wrapped tools.
pub struct McpConnection {
    /// Kept alive; dropping it closes the connection.
    pub service: RunningService<RoleClient, ()>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub server: String,
}

/// Discover a connected server's tools and wrap them into an [`McpConnection`].
async fn finish(
    service: RunningService<RoleClient, ()>,
    server: &str,
) -> anyhow::Result<McpConnection> {
    let peer = service.peer().clone();
    let discovered = service.list_all_tools().await?;
    let tools: Vec<Arc<dyn Tool>> = discovered
        .into_iter()
        .map(|t| {
            let real = t.name.to_string();
            let schema = serde_json::to_value(&*t.input_schema)
                .unwrap_or_else(|_| json!({ "type": "object" }));
            Arc::new(McpTool {
                peer: peer.clone(),
                qualified: qualify(server, &real),
                real,
                description: t.description.map(|c| c.to_string()).unwrap_or_default(),
                schema,
            }) as Arc<dyn Tool>
        })
        .collect();
    Ok(McpConnection {
        service,
        tools,
        server: server.to_string(),
    })
}

/// Spawn a stdio MCP server, discover its tools, and wrap them.
pub async fn connect_stdio(
    server: &str,
    command: &str,
    args: &[String],
    envs: &[(String, String)],
) -> anyhow::Result<McpConnection> {
    // On Windows, npm-shipped launchers (npx, npm) are `.cmd` scripts that `Command::new` can't
    // spawn directly — run them through `cmd /c`.
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/c").arg(command).args(args);
        c
    } else {
        let mut c = Command::new(command);
        c.args(args);
        c
    };
    for (k, v) in envs {
        cmd.env(k, v);
    }
    // Silence the child's stderr so a misbehaving server's shutdown spew (e.g. browsermcp's
    // recursive close handler) doesn't pollute the user's terminal after Cordy exits.
    cmd.stderr(std::process::Stdio::null());
    let transport = TokioChildProcess::new(cmd)?;
    let service = ().serve(transport).await?;
    finish(service, server).await
}

/// Connect to a streamable-HTTP MCP server, discover its tools, and wrap them.
pub async fn connect_http(server: &str, url: &str) -> anyhow::Result<McpConnection> {
    let transport = StreamableHttpClientTransport::from_uri(url);
    let service = ().serve(transport).await?;
    finish(service, server).await
}

/// Capability source exposing one server's tools.
pub struct McpCapability {
    server: String,
    tools: Vec<Arc<dyn Tool>>,
}

impl McpCapability {
    pub fn new(server: impl Into<String>, tools: Vec<Arc<dyn Tool>>) -> Self {
        McpCapability {
            server: server.into(),
            tools,
        }
    }
}

impl CapabilitySource for McpCapability {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }

    fn prompt_fragment(&self) -> Option<String> {
        if self.tools.is_empty() {
            return None;
        }
        Some(format!(
            "## MCP server `{}` ({} tools available)\n",
            self.server,
            self.tools.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualify_namespaces() {
        assert_eq!(qualify("fs", "read_file"), "fs__read_file");
    }
}
