//! Sub-agent types and their registry.
//!
//! Agent definitions live in `.cordy/agents/*.md` as markdown with a YAML-ish frontmatter
//! (`name`, `description`, `tools`, `model`); the body is the sub-agent's system prompt. The
//! [`AgentRegistry`] is a [`CapabilitySource`](crate::core::capability::CapabilitySource) that
//! exposes the `task` tool for spawning them.

use std::sync::Arc;

use crate::core::capability::CapabilitySource;
use crate::tools::Tool;

/// A sub-agent type.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// Restrict the sub-agent to these tool names; `None` means all base tools.
    pub tools: Option<Vec<String>>,
    /// Override model; `None` inherits the parent's.
    pub model: Option<String>,
    /// System prompt (the markdown body).
    pub system_prompt: String,
}

/// Parse one agent markdown file (frontmatter + body). `fallback_name` is used when the
/// frontmatter omits `name` (typically the file stem).
pub fn parse_agent_md(content: &str, fallback_name: &str) -> AgentDef {
    let mut name = fallback_name.to_string();
    let mut description = String::new();
    let mut tools = None;
    let mut model = None;
    let mut body = content;

    if let Some(rest) = content.strip_prefix("---") {
        // Frontmatter block terminated by a line of `---`.
        if let Some(end) = rest.find("\n---") {
            let front = &rest[..end];
            body = rest[end + 4..].trim_start_matches('\n');
            for line in front.lines() {
                let Some((k, v)) = line.split_once(':') else {
                    continue;
                };
                let (k, v) = (k.trim(), v.trim());
                match k {
                    "name" => name = v.to_string(),
                    "description" => description = v.to_string(),
                    "model" => model = Some(v.to_string()),
                    "tools" => {
                        tools = Some(
                            v.split(',')
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect(),
                        )
                    }
                    _ => {}
                }
            }
        }
    }

    AgentDef {
        name,
        description,
        tools,
        model,
        system_prompt: body.trim().to_string(),
    }
}

/// Load every `*.md` under `dir` as an agent definition. Missing dir -> empty.
pub fn load_agents(dir: &std::path::Path) -> Vec<AgentDef> {
    let mut defs = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return defs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("agent");
        if let Ok(content) = std::fs::read_to_string(&path) {
            defs.push(parse_agent_md(&content, stem));
        }
    }
    defs
}

/// Capability source exposing the `task` tool plus a prompt fragment listing agent types.
pub struct AgentRegistry {
    defs: Arc<Vec<AgentDef>>,
    task_tool: Arc<dyn Tool>,
}

impl AgentRegistry {
    pub fn new(defs: Arc<Vec<AgentDef>>, task_tool: Arc<dyn Tool>) -> Self {
        AgentRegistry { defs, task_tool }
    }
}

impl CapabilitySource for AgentRegistry {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.task_tool.clone()]
    }

    fn prompt_fragment(&self) -> Option<String> {
        if self.defs.is_empty() {
            return None;
        }
        let mut s = String::from("## Sub-agents (spawn via the `task` tool)\n");
        for d in self.defs.iter() {
            s.push_str(&format!("- {}: {}\n", d.name, d.description));
        }
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let md = "---\nname: reviewer\ndescription: reviews diffs\ntools: read, grep\nmodel: gpt-4o\n---\nYou review code carefully.";
        let def = parse_agent_md(md, "fallback");
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "reviews diffs");
        assert_eq!(
            def.tools,
            Some(vec!["read".to_string(), "grep".to_string()])
        );
        assert_eq!(def.model.as_deref(), Some("gpt-4o"));
        assert_eq!(def.system_prompt, "You review code carefully.");
    }

    #[test]
    fn no_frontmatter_uses_fallback_name_and_full_body() {
        let def = parse_agent_md("just a prompt", "helper");
        assert_eq!(def.name, "helper");
        assert_eq!(def.tools, None);
        assert_eq!(def.system_prompt, "just a prompt");
    }
}
