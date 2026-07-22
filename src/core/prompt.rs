//! System prompt assembly (pi-style layered composition).
//!
//! The final prompt is built from several sources, in order:
//! 1. **Base instructions** — a `SYSTEM.md` override (project `.cordy/SYSTEM.md`, else global
//!    `~/.cordy/SYSTEM.md`) if present, otherwise the built-in default (full, or a lean variant
//!    for Cognitive Core models).
//! 2. **Tools** — the available tool names.
//! 3. **Environment** — cwd + OS.
//! 4. **Project context** — `AGENTS.md` / `CLAUDE.md` / `.cordy/CORDY.md` discovered by walking up
//!    from the working directory, wrapped in a `<project_context>` block with per-file paths.
//! 5. **Capabilities** — skills / sub-agents / MCP prompt fragments.
//! 6. **Append** — `APPEND_SYSTEM.md` (global then project) added verbatim at the end.
//!
//! A project override/append takes priority over the global one. This mirrors pi's
//! `SYSTEM.md` / `APPEND_SYSTEM.md` mechanism.

use std::path::{Path, PathBuf};

/// Inputs for [`build_system_prompt`].
pub struct PromptContext<'a> {
    pub cwd: &'a Path,
    pub os: &'a str,
    pub tool_names: &'a [String],
    /// `<project_context>` body (from [`load_project_context`]).
    pub project_context: Option<String>,
    /// Full base-prompt override from `SYSTEM.md` (replaces the built-in default).
    pub custom_prompt: Option<String>,
    /// Extra text appended verbatim from `APPEND_SYSTEM.md`.
    pub append_prompt: Option<String>,
    /// Capability fragments (skills / sub-agents / MCP) to fold into the prompt.
    pub capabilities: Option<String>,
    /// Emit the lean small-orchestrator prompt.
    pub cognitive_core: bool,
}

const FULL_PREAMBLE: &str = "\
You are Cordy, an expert autonomous coding agent operating in the user's terminal. You help by \
reading files, running commands, editing code, and writing new files — working directly in the \
project with the tools provided.

## How you work
- Understand before you act: read the relevant files and search the codebase before proposing or \
making changes.
- Edit precisely: use `edit` with an exact, unique `old` string; keep diffs minimal and focused; \
never reformat or touch unrelated code.
- Match the surroundings: follow the project's existing style, naming, and conventions.
- Verify your work: after changes, build/test/run what's relevant and report the actual result — \
if something fails, say so with the output.
- Prefer tools over recall: read the code, grep, run commands, and search the web for current \
facts instead of relying on memory.
- Work one concrete step at a time; don't guess when you can check.

## Style
- Be concise and factual. Lead with the answer or the action; skip preamble and filler.
- Show file paths clearly (e.g. `src/main.rs:42`) so they are easy to follow.
- When you change files, state what changed and where.
- Ask only when you are genuinely blocked on a decision that is the user's to make.

## Safety
- Destructive or irreversible actions (deleting files, force operations, mass edits) need care — \
confirm intent unless clearly authorized.
- Respect the permission system: some tools are gated and will prompt the user before running.";

const LEAN_PREAMBLE: &str = "\
You are Cordy, a compact coding agent that drives tools. You hold little built-in knowledge — \
rely on tools: read files, grep, run commands, and search the web instead of recalling. Take one \
concrete step at a time and keep output terse. Use `edit` with an exact, unique `old` string; \
read before you edit; verify every change.";

/// Assemble the full system prompt string.
pub fn build_system_prompt(ctx: &PromptContext) -> String {
    let mut s = String::new();

    // 1. Base instructions: SYSTEM.md override, else the built-in default.
    match &ctx.custom_prompt {
        Some(custom) if !custom.trim().is_empty() => s.push_str(custom.trim()),
        _ => s.push_str(if ctx.cognitive_core {
            LEAN_PREAMBLE
        } else {
            FULL_PREAMBLE
        }),
    }

    // 2. Tools.
    if !ctx.tool_names.is_empty() {
        s.push_str("\n\n## Tools\n");
        s.push_str(&ctx.tool_names.join(", "));
        s.push_str(
            "\nOther tools may be available depending on the project (skills, MCP servers).",
        );
    }

    // 3. Environment.
    s.push_str("\n\n## Environment\n");
    s.push_str(&format!("cwd: {}\nos: {}\n", ctx.cwd.display(), ctx.os));

    // 4. Project context (AGENTS.md / CLAUDE.md / CORDY.md), tagged with paths.
    if let Some(pc) = &ctx.project_context
        && !pc.trim().is_empty()
    {
        s.push_str("\n<project_context>\n");
        s.push_str(pc.trim());
        s.push_str("\n</project_context>\n");
    }

    // 5. Capabilities (skills / sub-agents / MCP fragments).
    if let Some(cap) = &ctx.capabilities
        && !cap.trim().is_empty()
    {
        s.push('\n');
        s.push_str(cap.trim());
        s.push('\n');
    }

    // 6. Append (APPEND_SYSTEM.md), verbatim and last.
    if let Some(app) = &ctx.append_prompt
        && !app.trim().is_empty()
    {
        s.push_str("\n\n");
        s.push_str(app.trim());
        s.push('\n');
    }

    s
}

/// Files Cordy looks for as project context, nearest-directory last.
const CONTEXT_FILES: [&str; 3] = ["AGENTS.md", "CLAUDE.md", ".cordy/CORDY.md"];

/// Walk from the filesystem root down to `cwd`, collecting any context files found, wrapping each
/// in a `<file path="...">` tag so the most specific (nearest to `cwd`) guidance appears last.
/// Returns the inner body for a `<project_context>` block, or `None` when nothing is found.
pub fn load_project_context(cwd: &Path) -> Option<String> {
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut cur = Some(cwd.to_path_buf());
    while let Some(dir) = cur {
        chain.push(dir.clone());
        cur = dir.parent().map(Path::to_path_buf);
    }
    chain.reverse();

    let mut parts: Vec<String> = Vec::new();
    for dir in chain {
        for name in CONTEXT_FILES {
            let path = dir.join(name);
            if let Ok(text) = std::fs::read_to_string(&path) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(format!(
                        "<file path=\"{}\">\n{}\n</file>",
                        path.display(),
                        trimmed
                    ));
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Read the first existing `SYSTEM.md` override: project `.cordy/SYSTEM.md` wins, else the global
/// `<user_dir>/SYSTEM.md`. `None` when neither exists (the built-in default is then used).
pub fn load_system_override(cwd: &Path, user_dir: Option<&Path>) -> Option<String> {
    read_nonempty(&cwd.join(".cordy").join("SYSTEM.md"))
        .or_else(|| user_dir.and_then(|d| read_nonempty(&d.join("SYSTEM.md"))))
}

/// Concatenate `APPEND_SYSTEM.md` from the global dir then the project (`.cordy/APPEND_SYSTEM.md`),
/// so project guidance comes last. `None` when neither exists.
pub fn load_system_append(cwd: &Path, user_dir: Option<&Path>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = user_dir
        && let Some(t) = read_nonempty(&d.join("APPEND_SYSTEM.md"))
    {
        parts.push(t);
    }
    if let Some(t) = read_nonempty(&cwd.join(".cordy").join("APPEND_SYSTEM.md")) {
        parts.push(t);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Read a file, returning its trimmed contents only if non-empty.
fn read_nonempty(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(cwd: &'a Path, tools: &'a [String]) -> PromptContext<'a> {
        PromptContext {
            cwd,
            os: "linux",
            tool_names: tools,
            project_context: None,
            custom_prompt: None,
            append_prompt: None,
            capabilities: None,
            cognitive_core: false,
        }
    }

    #[test]
    fn full_prompt_lists_tools_and_env() {
        let tools = vec!["read".to_string(), "edit".to_string()];
        let p = build_system_prompt(&ctx(Path::new("/proj"), &tools));
        assert!(p.contains("autonomous coding agent"));
        assert!(p.contains("read, edit"));
        assert!(p.contains("os: linux"));
    }

    #[test]
    fn lean_prompt_for_cognitive_core() {
        let tools = vec!["read".to_string()];
        let mut c = ctx(Path::new("/p"), &tools);
        c.cognitive_core = true;
        let p = build_system_prompt(&c);
        assert!(p.contains("compact coding agent"));
    }

    #[test]
    fn custom_prompt_overrides_default() {
        let tools = vec!["read".to_string()];
        let mut c = ctx(Path::new("/p"), &tools);
        c.custom_prompt = Some("BESPOKE INSTRUCTIONS".into());
        let p = build_system_prompt(&c);
        assert!(p.contains("BESPOKE INSTRUCTIONS"));
        assert!(!p.contains("autonomous coding agent")); // default replaced
        assert!(p.contains("read")); // tools still appended
    }

    #[test]
    fn context_and_append_are_placed() {
        let tools = vec!["read".to_string()];
        let mut c = ctx(Path::new("/p"), &tools);
        c.project_context = Some("<file path=\"AGENTS.md\">tabs</file>".into());
        c.append_prompt = Some("EXTRA".into());
        let p = build_system_prompt(&c);
        assert!(p.contains("<project_context>"));
        assert!(p.contains("tabs"));
        // Append comes after the context block.
        assert!(p.find("EXTRA").unwrap() > p.find("<project_context>").unwrap());
    }

    #[test]
    fn context_files_tagged_with_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Follow the style guide.").unwrap();
        let pc = load_project_context(dir.path()).unwrap();
        assert!(pc.contains("Follow the style guide."));
        assert!(pc.contains("<file path="));
        assert!(pc.contains("AGENTS.md"));
    }

    #[test]
    fn system_override_prefers_project_then_global() {
        let proj = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join(".cordy")).unwrap();
        std::fs::write(user.path().join("SYSTEM.md"), "global").unwrap();
        // Only global present.
        assert_eq!(
            load_system_override(proj.path(), Some(user.path())).as_deref(),
            Some("global")
        );
        // Project override wins.
        std::fs::write(proj.path().join(".cordy").join("SYSTEM.md"), "project").unwrap();
        assert_eq!(
            load_system_override(proj.path(), Some(user.path())).as_deref(),
            Some("project")
        );
    }

    #[test]
    fn append_concatenates_global_then_project() {
        let proj = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join(".cordy")).unwrap();
        std::fs::write(user.path().join("APPEND_SYSTEM.md"), "G").unwrap();
        std::fs::write(proj.path().join(".cordy").join("APPEND_SYSTEM.md"), "P").unwrap();
        let a = load_system_append(proj.path(), Some(user.path())).unwrap();
        assert!(a.find("G").unwrap() < a.find("P").unwrap());
    }
}
