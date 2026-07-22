//! Configuration: TOML loaded from a project `.cordy/config.toml` merged over the user-level
//! `~/.cordy/config.toml`. All user-level settings live under `~/.cordy`.
//!
//! Scalars: the project file wins when it sets a value. Lists (`provider`, `model`, `mcp`):
//! user + project entries are concatenated, and a project entry with the same `name` replaces
//! the user's. Carries the model catalog (context window, pricing, and the `cognitive_core`
//! flag that switches the environment into small-orchestrator mode).

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Native token optimizer on/off (default on when absent).
    pub optimize: Option<bool>,
    /// Show the animated mascot (default on).
    pub mascot: Option<bool>,
    pub theme: Option<String>,
    /// Custom status-line template. Placeholders: `{cwd}` `{model}` `{provider}` `{mode}`
    /// `{tokens}` `{cost}` `{ctx}` `{saved}` `{status}` `{spinner}` `{version}`. `None` = default.
    pub statusline: Option<String>,
    pub compact_model: Option<String>,
    pub compact_threshold: Option<u64>,
    /// `manual` | `auto`.
    pub compact_mode: Option<String>,
    #[serde(rename = "provider")]
    pub providers: Vec<ProviderProfile>,
    #[serde(rename = "model")]
    pub models: Vec<ModelProfile>,
    #[serde(rename = "mcp")]
    pub mcp_servers: Vec<McpServer>,
    /// Up-front permission rules so the agent doesn't prompt for pre-approved commands.
    pub permissions: PermissionConfig,
    /// Per-role color overrides (`#rrggbb`) applied on top of the selected theme.
    pub colors: ColorConfig,
}

/// Hex color overrides for the UI theme. Any unset field keeps the theme's default.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct ColorConfig {
    pub user: Option<String>,
    pub assistant: Option<String>,
    pub tool: Option<String>,
    pub system: Option<String>,
    pub dim: Option<String>,
    pub accent: Option<String>,
    pub border: Option<String>,
    pub surface: Option<String>,
    pub base: Option<String>,
}

/// Pre-configured permission rules. Entries are `tool:glob` (e.g. `bash:ls *`, `bash:git *`);
/// a bare glob applies to any tool. `mode = "auto"` approves everything not explicitly denied
/// (writes outside the working directory still prompt).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct PermissionConfig {
    /// `ask` (default) or `auto`.
    pub mode: Option<String>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ProviderProfile {
    pub name: String,
    /// `openai-chat` | `openai-compatible` | `anthropic` | `openai-responses`.
    pub kind: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Env var holding the API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct ModelProfile {
    pub name: String,
    pub provider: Option<String>,
    pub context_window: Option<u64>,
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    /// Small (2-4B) always-on orchestrator: lean prompt, retrieval-first, max optimize,
    /// small-context compaction, constrained tool output, escalate hard steps.
    pub cognitive_core: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpServer {
    pub name: String,
    /// `stdio` | `http` | `sse`.
    pub transport: String,
    pub command: Option<String>,
    pub url: Option<String>,
    pub enabled: bool,
}

impl Default for McpServer {
    fn default() -> Self {
        McpServer {
            name: String::new(),
            transport: "stdio".into(),
            command: None,
            url: None,
            enabled: true,
        }
    }
}

impl Config {
    pub fn parse(toml_str: &str) -> anyhow::Result<Config> {
        Ok(toml::from_str(toml_str)?)
    }

    /// True unless explicitly disabled.
    pub fn optimize_enabled(&self) -> bool {
        self.optimize.unwrap_or(true)
    }

    /// Whether to show the mascot (default true).
    pub fn mascot_enabled(&self) -> bool {
        self.mascot.unwrap_or(true)
    }

    pub fn model(&self, name: &str) -> Option<&ModelProfile> {
        self.models.iter().find(|m| m.name == name)
    }

    /// Merge `over` on top of `self`: scalars from `over` win when set; list entries are merged
    /// by `name` (an `over` entry replaces a same-named one, otherwise appends).
    pub fn merged_with(mut self, over: Config) -> Config {
        self.optimize = over.optimize.or(self.optimize);
        self.mascot = over.mascot.or(self.mascot);
        self.theme = over.theme.or(self.theme);
        self.statusline = over.statusline.or(self.statusline);
        self.compact_model = over.compact_model.or(self.compact_model);
        self.compact_threshold = over.compact_threshold.or(self.compact_threshold);
        self.compact_mode = over.compact_mode.or(self.compact_mode);
        merge_by_name(&mut self.providers, over.providers, |p| p.name.clone());
        merge_by_name(&mut self.models, over.models, |m| m.name.clone());
        merge_by_name(&mut self.mcp_servers, over.mcp_servers, |s| s.name.clone());
        // Permissions: project mode wins; allow/deny lists concatenate.
        self.permissions.mode = over.permissions.mode.or(self.permissions.mode);
        self.permissions.allow.extend(over.permissions.allow);
        self.permissions.deny.extend(over.permissions.deny);
        // Colors: per-field override.
        let c = &mut self.colors;
        let o = over.colors;
        c.user = o.user.or(c.user.take());
        c.assistant = o.assistant.or(c.assistant.take());
        c.tool = o.tool.or(c.tool.take());
        c.system = o.system.or(c.system.take());
        c.dim = o.dim.or(c.dim.take());
        c.accent = o.accent.or(c.accent.take());
        c.border = o.border.or(c.border.take());
        c.surface = o.surface.or(c.surface.take());
        c.base = o.base.or(c.base.take());
        self
    }

    /// Load and merge the user config then the project config, skipping absent files.
    pub fn load(user_path: Option<&std::path::Path>, project_path: &std::path::Path) -> Config {
        let mut cfg = Config::default();
        if let Some(p) = user_path
            && let Ok(text) = std::fs::read_to_string(p)
            && let Ok(user) = Config::parse(&text)
        {
            cfg = cfg.merged_with(user);
        }
        if let Ok(text) = std::fs::read_to_string(project_path)
            && let Ok(project) = Config::parse(&text)
        {
            cfg = cfg.merged_with(project);
        }
        cfg
    }
}

const CONFIG_TEMPLATE: &str = r##"# Cordy configuration. Secrets stay in env (api_key_env), never here.
optimize = true

# Theme: mono (default) · dark · tokyonight · catppuccin · gruvbox · nord · rosepine · light
# theme = "tokyonight"
#
# Fine-tune any role color (hex) on top of the theme:
# [colors]
# accent  = "#7aa2f7"
# user    = "#c0caf5"
# surface = "#24283b"

# Custom status line. Placeholders:
#   {cwd} {model} {provider} {mode} {tokens} {cost} {ctx} {saved} {status} {spinner}
#   {bg} (background jobs) {agents} (active sub-agents) {version}
# statusline = "{spinner}{status}  ·  {model}  ·  {ctx}  ·  {bg}bg  ·  {cost}"

# Permissions — pre-approve commands so the agent doesn't ask every time.
# Entries are `tool:glob` (bare glob = any tool). mode = "auto" approves everything not denied.
# [permissions]
# mode = "ask"                       # or "auto"
# allow = ["bash:ls *", "bash:git *", "bash:cargo *", "bash:cat *"]
# deny  = ["bash:rm -rf *", "bash:git push *"]

# [[provider]]
# name = "openai"
# kind = "openai-chat"
# api_key_env = "OPENAI_API_KEY"

# [[model]]
# name = "gpt-4o-mini"
# provider = "openai"
# context_window = 128000
# cognitive_core = false
"##;

/// Scaffold a project `.cordy/` directory: a starter `config.toml` and the `agents/`, `skills/`,
/// `commands/` subdirectories. Existing files are left untouched.
pub fn init_project(cwd: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let root = cwd.join(".cordy");
    let mut created = Vec::new();
    for sub in ["agents", "skills", "commands"] {
        let dir = root.join(sub);
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
            created.push(format!(".cordy/{sub}/"));
        }
    }
    let config = root.join("config.toml");
    if !config.exists() {
        std::fs::create_dir_all(&root)?;
        std::fs::write(&config, CONFIG_TEMPLATE)?;
        created.push(".cordy/config.toml".to_string());
    }
    Ok(created)
}

/// Append a `[[provider]]` entry to a config file (creating it if absent), unless a provider with
/// the same `name` is already present. Text is appended rather than re-serialized so user comments
/// and formatting survive. Secrets are never written here — only name/kind/base_url.
pub fn save_provider(
    path: &std::path::Path,
    name: &str,
    kind: &str,
    base_url: &str,
) -> anyhow::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if let Ok(cfg) = Config::parse(&existing)
        && cfg.providers.iter().any(|p| p.name == name)
    {
        return Ok(()); // already saved
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "\n[[provider]]\nname = \"{name}\"\nkind = \"{kind}\"\nbase_url = \"{base_url}\"\n"
    ));
    std::fs::write(path, out)?;
    Ok(())
}

/// Remove a `[[provider]]` block by name from a config file (text-based, so surrounding comments
/// and other entries survive).
pub fn remove_provider(path: &std::path::Path, name: &str) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "[[provider]]" {
            // Collect the block up to the next table header or EOF.
            let start = i;
            let mut j = i + 1;
            while j < lines.len() && !lines[j].trim_start().starts_with('[') {
                j += 1;
            }
            let block = &lines[start..j];
            let is_target = block.iter().any(|l| {
                let t = l.trim();
                t.starts_with("name") && t.contains(&format!("\"{name}\""))
            });
            if is_target {
                // Skip this block (and a single trailing blank line, if any).
                i = j;
                while out.last().is_some_and(|l| l.trim().is_empty()) {
                    out.pop();
                }
                continue;
            }
        }
        out.push(lines[i]);
        i += 1;
    }
    let mut joined = out.join("\n");
    if !joined.ends_with('\n') {
        joined.push('\n');
    }
    std::fs::write(path, joined)?;
    Ok(())
}

fn merge_by_name<T>(base: &mut Vec<T>, over: Vec<T>, key: impl Fn(&T) -> String) {
    for item in over {
        let k = key(&item);
        if let Some(slot) = base.iter_mut().find(|b| key(b) == k) {
            *slot = item;
        } else {
            base.push(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_providers_models_and_cognitive_core() {
        let cfg = Config::parse(
            r#"
            optimize = true

            [[provider]]
            name = "local"
            kind = "openai-compatible"
            base_url = "http://localhost:11434/v1"

            [[model]]
            name = "qwen2.5-coder-3b"
            provider = "local"
            context_window = 8192
            cognitive_core = true

            [[model]]
            name = "gpt-4o"
            provider = "openai"
        "#,
        )
        .unwrap();

        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].kind, "openai-compatible");
        assert!(cfg.model("qwen2.5-coder-3b").unwrap().cognitive_core);
        assert!(!cfg.model("gpt-4o").unwrap().cognitive_core);
        assert!(cfg.optimize_enabled());
    }

    #[test]
    fn project_overrides_user_scalars_and_models() {
        let user = Config::parse(
            r#"
            optimize = false
            theme = "dark"
            [[model]]
            name = "m"
            context_window = 1000
        "#,
        )
        .unwrap();
        let project = Config::parse(
            r#"
            optimize = true
            [[model]]
            name = "m"
            context_window = 2000
            cognitive_core = true
        "#,
        )
        .unwrap();

        let merged = user.merged_with(project);
        assert_eq!(merged.optimize, Some(true)); // project wins
        assert_eq!(merged.theme.as_deref(), Some("dark")); // kept from user
        assert_eq!(merged.models.len(), 1); // merged by name
        assert_eq!(merged.model("m").unwrap().context_window, Some(2000));
        assert!(merged.model("m").unwrap().cognitive_core);
    }

    #[test]
    fn init_scaffolds_cordy_dir() {
        let dir = tempfile::tempdir().unwrap();
        let created = init_project(dir.path()).unwrap();
        assert!(dir.path().join(".cordy/config.toml").exists());
        assert!(dir.path().join(".cordy/agents").is_dir());
        assert!(created.iter().any(|c| c.contains("config.toml")));

        // Idempotent: a second run creates nothing new.
        let again = init_project(dir.path()).unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn save_provider_appends_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "optimize = true\n").unwrap();

        save_provider(&path, "nvidia", "openai-compatible", "https://x/v1").unwrap();
        let cfg = Config::load(None, &path);
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].name, "nvidia");
        assert_eq!(cfg.providers[0].base_url.as_deref(), Some("https://x/v1"));

        // Idempotent by name.
        save_provider(&path, "nvidia", "openai-compatible", "https://x/v1").unwrap();
        assert_eq!(Config::load(None, &path).providers.len(), 1);

        // Comments/scalars preserved.
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("optimize = true")
        );
    }

    #[test]
    fn remove_provider_drops_block_keeps_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "optimize = true\n").unwrap();
        save_provider(&path, "a", "openai-chat", "https://a/v1").unwrap();
        save_provider(&path, "b", "openai-compatible", "https://b/v1").unwrap();
        assert_eq!(Config::load(None, &path).providers.len(), 2);

        remove_provider(&path, "a").unwrap();
        let cfg = Config::load(None, &path);
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].name, "b");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("optimize = true")
        );
    }

    #[test]
    fn mcp_defaults_enabled() {
        let cfg = Config::parse(
            r#"
            [[mcp]]
            name = "fs"
            transport = "stdio"
            command = "mcp-server-fs"
        "#,
        )
        .unwrap();
        assert!(cfg.mcp_servers[0].enabled);
    }
}
