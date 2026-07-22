//! The unifying extensibility seam.
//!
//! Everything that gives the agent tools or context — builtin tools, MCP servers, skill sets,
//! the sub-agent registry — implements [`CapabilitySource`]. The agent loop stays ignorant of
//! where a tool came from: it sees only the assembled registry and system prompt. Adding a new
//! capability is one more implementor, nothing in the loop changes.

use std::sync::Arc;

use crate::tools::Tool;

pub trait CapabilitySource: Send + Sync {
    /// Tools this source contributes to the shared registry.
    fn tools(&self) -> Vec<Arc<dyn Tool>>;

    /// An optional fragment appended to the system prompt (e.g. skill listings, resource
    /// hints). Defaults to nothing.
    fn prompt_fragment(&self) -> Option<String> {
        None
    }
}
