//! Permission rule engine.
//!
//! Wraps an interactive asker (the TUI modal) with a rule list and a write sandbox. A request
//! is decided by the first matching rule (`tool:glob` -> allow/deny); unmatched requests fall
//! through to the asker. Writes outside the sandbox root always ask, even if a rule would allow
//! them. Rules come from `.cordy/permissions.toml` (allowlist/denylist) or are added at runtime
//! by an "allow always" choice.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use regex::Regex;

use crate::tools::{Permission, PermissionRequest, Risk};

/// A single allow/deny rule matching a tool's request key against a glob.
pub struct Rule {
    tool: String,
    /// The original glob (for display / persistence).
    pattern: String,
    matcher: Regex,
    allow: bool,
}

impl Rule {
    /// `tool` may be `*` to match any tool; `glob` supports `*` wildcards over the request key.
    pub fn new(tool: &str, glob: &str, allow: bool) -> Rule {
        Rule {
            tool: tool.to_string(),
            pattern: glob.to_string(),
            matcher: glob_regex(glob),
            allow,
        }
    }

    fn matches(&self, tool: &str, key: &str) -> bool {
        (self.tool == "*" || self.tool == tool) && self.matcher.is_match(key)
    }

    /// `allow bash:git *` style description.
    fn describe(&self) -> String {
        let verb = if self.allow { "allow" } else { "deny " };
        format!("{verb} {}:{}", self.tool, self.pattern)
    }
}

/// Rule-based permission gate delegating to `asker` on no match. Rules are behind a mutex so the
/// UI can add an "allow always" rule mid-session.
pub struct PermissionEngine {
    rules: Mutex<Vec<Rule>>,
    sandbox_root: Option<PathBuf>,
    asker: Arc<dyn Permission>,
}

impl PermissionEngine {
    pub fn new(asker: Arc<dyn Permission>) -> Self {
        PermissionEngine {
            rules: Mutex::new(Vec::new()),
            sandbox_root: None,
            asker,
        }
    }

    pub fn with_rules(self, rules: Vec<Rule>) -> Self {
        *self.rules.lock().unwrap() = rules;
        self
    }

    /// Confine `Write` actions to paths under `root`; writes elsewhere always ask.
    pub fn with_sandbox(mut self, root: impl Into<PathBuf>) -> Self {
        self.sandbox_root = Some(root.into());
        self
    }

    /// Add a rule at runtime (e.g. an "allow always" choice from the permission modal).
    pub fn add_rule(&self, tool: &str, glob: &str, allow: bool) {
        self.rules
            .lock()
            .unwrap()
            .push(Rule::new(tool, glob, allow));
    }

    /// Replace all rules (used when the config is hot-reloaded).
    pub fn set_rules(&self, rules: Vec<Rule>) {
        *self.rules.lock().unwrap() = rules;
    }

    /// One-line descriptions of every active rule, for `/permissions`.
    pub fn describe_rules(&self) -> Vec<String> {
        self.rules
            .lock()
            .unwrap()
            .iter()
            .map(Rule::describe)
            .collect()
    }

    /// First matching rule's decision, if any.
    fn rule_decision(&self, tool: &str, key: &str) -> Option<bool> {
        self.rules
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.matches(tool, key))
            .map(|r| r.allow)
    }

    fn outside_sandbox(&self, req: &PermissionRequest<'_>) -> bool {
        if req.risk != Risk::Write {
            return false;
        }
        match &self.sandbox_root {
            Some(root) => {
                let p = Path::new(req.key);
                p.is_absolute() && !p.starts_with(root)
            }
            None => false,
        }
    }
}

#[async_trait]
impl Permission for PermissionEngine {
    async fn request(&self, req: PermissionRequest<'_>) -> bool {
        // A deny rule always wins, even outside the sandbox.
        if self.rule_decision(req.tool, req.key) == Some(false) {
            return false;
        }
        // Outside the sandbox: never auto-allow; defer to the asker.
        if self.outside_sandbox(&req) {
            return self.asker.request(req).await;
        }
        match self.rule_decision(req.tool, req.key) {
            Some(decision) => decision,
            None => self.asker.request(req).await,
        }
    }
}

/// Convert a `*`-glob into an anchored regex over the whole key.
fn glob_regex(glob: &str) -> Regex {
    let mut re = String::from("^");
    for ch in glob.chars() {
        match ch {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c if ".+()|^$[]{}\\".contains(c) => {
                re.push('\\');
                re.push(c);
            }
            c => re.push(c),
        }
    }
    re.push('$');
    Regex::new(&re).unwrap_or_else(|_| Regex::new("^$").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{AutoApprove, DenyAll};

    fn req<'a>(tool: &'a str, key: &'a str, risk: Risk) -> PermissionRequest<'a> {
        PermissionRequest {
            risk,
            tool,
            key,
            summary: key,
        }
    }

    #[tokio::test]
    async fn allow_rule_short_circuits_asker() {
        // Asker denies everything; an allow rule must still permit.
        let engine = PermissionEngine::new(Arc::new(DenyAll))
            .with_rules(vec![Rule::new("bash", "git *", true)]);
        assert!(engine.request(req("bash", "git status", Risk::Exec)).await);
    }

    #[tokio::test]
    async fn deny_rule_wins() {
        let engine = PermissionEngine::new(Arc::new(AutoApprove))
            .with_rules(vec![Rule::new("bash", "rm -rf *", false)]);
        assert!(!engine.request(req("bash", "rm -rf /", Risk::Exec)).await);
    }

    #[tokio::test]
    async fn no_rule_delegates_to_asker() {
        let allow = PermissionEngine::new(Arc::new(AutoApprove));
        assert!(allow.request(req("bash", "ls", Risk::Exec)).await);
        let deny = PermissionEngine::new(Arc::new(DenyAll));
        assert!(!deny.request(req("bash", "ls", Risk::Exec)).await);
    }

    #[tokio::test]
    async fn add_rule_takes_effect_and_describes() {
        let engine = PermissionEngine::new(Arc::new(DenyAll));
        assert!(!engine.request(req("bash", "ls", Risk::Exec)).await); // asker denies
        engine.add_rule("bash", "*", true); // allow-always
        assert!(engine.request(req("bash", "ls", Risk::Exec)).await);
        assert_eq!(engine.describe_rules(), vec!["allow bash:*".to_string()]);
    }

    #[tokio::test]
    async fn write_outside_sandbox_ignores_allow_rule() {
        // Use real OS-absolute paths so the check works cross-platform.
        let root = std::env::temp_dir().join("cordy_sandbox_test");
        let inside = root.join("src").join("a.rs");
        let outside = std::env::temp_dir().join("elsewhere").join("x.rs");
        let inside = inside.to_string_lossy().into_owned();
        let outside = outside.to_string_lossy().into_owned();

        let engine = PermissionEngine::new(Arc::new(DenyAll))
            .with_sandbox(&root)
            .with_rules(vec![Rule::new("write", "*", true)]);

        // Even with a broad allow rule, a write outside the sandbox asks (asker denies).
        assert!(!engine.request(req("write", &outside, Risk::Write)).await);
        // Inside the sandbox the allow rule applies.
        assert!(engine.request(req("write", &inside, Risk::Write)).await);
    }
}
