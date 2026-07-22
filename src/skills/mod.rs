//! Skills — reusable instruction packets with progressive disclosure.
//!
//! A skill lives at `.cordy/skills/<name>/SKILL.md` (or `.cordy/skills/<name>.md`) as markdown
//! with frontmatter (`name`, `description`, `when-to-use`). Only the name + description are put
//! in the system prompt (cheap); the full body is loaded on demand by the `skill` tool. The
//! [`SkillSet`] is a [`CapabilitySource`](crate::core::capability::CapabilitySource).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::core::capability::CapabilitySource;
use crate::core::types::ToolOutput;
use crate::tools::{Risk, Tool, ToolCtx};

/// A loaded skill.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    /// Full instruction body, disclosed on demand.
    pub body: String,
}

/// Parse a `SKILL.md` (frontmatter + body).
pub fn parse_skill_md(content: &str, fallback_name: &str) -> SkillDef {
    let mut name = fallback_name.to_string();
    let mut description = String::new();
    let mut when_to_use = None;
    let mut body = content;

    if let Some(rest) = content.strip_prefix("---")
        && let Some(end) = rest.find("\n---")
    {
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
                "when-to-use" | "when_to_use" => when_to_use = Some(v.to_string()),
                _ => {}
            }
        }
    }

    SkillDef {
        name,
        description,
        when_to_use,
        body: body.trim().to_string(),
    }
}

/// Load skills from `dir`, supporting both `<name>/SKILL.md` and `<name>.md` layouts.
pub fn load_skills(dir: &std::path::Path) -> Vec<SkillDef> {
    let mut skills = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return skills;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let (name, file) = if path.is_dir() {
            let f = path.join("SKILL.md");
            (
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("skill")
                    .to_string(),
                f,
            )
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            (
                path.file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("skill")
                    .to_string(),
                path.clone(),
            )
        } else {
            continue;
        };
        if let Ok(content) = std::fs::read_to_string(&file) {
            skills.push(parse_skill_md(&content, &name));
        }
    }
    skills
}

/// The `skill` tool: load a skill's full body by name.
pub struct SkillTool {
    skills: Arc<Vec<SkillDef>>,
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load the full instructions for a named skill before doing a task it covers."
    }

    fn schema(&self) -> Value {
        let names: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
        json!({
            "type": "object",
            "properties": { "name": { "type": "string", "enum": names } },
            "required": ["name"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    async fn run(&self, input: Value, _ctx: &ToolCtx) -> ToolOutput {
        let Some(name) = input["name"].as_str() else {
            return ToolOutput::error("skill: missing `name`");
        };
        match self.skills.iter().find(|s| s.name == name) {
            Some(s) => ToolOutput::ok(s.body.clone()),
            None => ToolOutput::error(format!("skill: unknown skill `{name}`")),
        }
    }
}

/// Capability source: the `skill` tool plus a prompt fragment listing available skills.
pub struct SkillSet {
    skills: Arc<Vec<SkillDef>>,
}

impl SkillSet {
    pub fn new(skills: Vec<SkillDef>) -> Self {
        SkillSet {
            skills: Arc::new(skills),
        }
    }
}

impl CapabilitySource for SkillSet {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        if self.skills.is_empty() {
            Vec::new()
        } else {
            vec![Arc::new(SkillTool {
                skills: self.skills.clone(),
            })]
        }
    }

    fn prompt_fragment(&self) -> Option<String> {
        if self.skills.is_empty() {
            return None;
        }
        let mut s = String::from("## Skills (load full instructions via the `skill` tool)\n");
        for sk in self.skills.iter() {
            let when = sk
                .when_to_use
                .as_deref()
                .map(|w| format!(" — when: {w}"))
                .unwrap_or_default();
            s.push_str(&format!("- {}: {}{when}\n", sk.name, sk.description));
        }
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_skill_frontmatter() {
        let md = "---\nname: commit\ndescription: write commits\nwhen-to-use: before committing\n---\nWrite a conventional commit.";
        let s = parse_skill_md(md, "fallback");
        assert_eq!(s.name, "commit");
        assert_eq!(s.when_to_use.as_deref(), Some("before committing"));
        assert_eq!(s.body, "Write a conventional commit.");
    }

    #[tokio::test]
    async fn skill_tool_returns_body_on_demand() {
        let skills = Arc::new(vec![SkillDef {
            name: "commit".into(),
            description: "write commits".into(),
            when_to_use: None,
            body: "Full commit instructions.".into(),
        }]);
        let tool = SkillTool { skills };
        let out = tool
            .run(json!({ "name": "commit" }), &ToolCtx::new("."))
            .await;
        assert_eq!(out.text, "Full commit instructions.");

        let miss = tool
            .run(json!({ "name": "nope" }), &ToolCtx::new("."))
            .await;
        assert!(miss.is_error);
    }

    #[test]
    fn skillset_fragment_lists_names() {
        let set = SkillSet::new(vec![SkillDef {
            name: "commit".into(),
            description: "write commits".into(),
            when_to_use: Some("before committing".into()),
            body: "x".into(),
        }]);
        let frag = set.prompt_fragment().unwrap();
        assert!(frag.contains("commit: write commits"));
        assert!(frag.contains("when: before committing"));
        assert_eq!(set.tools().len(), 1);
    }
}
