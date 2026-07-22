//! Autonomous mode — `/goal` persistence and ralph-loop guardrails.
//!
//! `/goal` writes a north-star to `.cordy/goal.md`; the agent's running notes live in
//! `.cordy/progress.md`. The ralph-loop insight is to discard the rotting context each iteration
//! and keep the goal + progress on disk, so long tasks don't drown in context. [`Guardrails`]
//! bound the loop so it can run unattended without burning budget: it stops on goal-complete, a
//! max iteration count, or a cost cap.

use std::path::{Path, PathBuf};

/// Reads/writes the goal and progress files under a `.cordy` directory.
pub struct GoalStore {
    dir: PathBuf,
}

impl GoalStore {
    pub fn new(cordy_dir: impl Into<PathBuf>) -> Self {
        GoalStore {
            dir: cordy_dir.into(),
        }
    }

    fn goal_path(&self) -> PathBuf {
        self.dir.join("goal.md")
    }

    fn progress_path(&self) -> PathBuf {
        self.dir.join("progress.md")
    }

    pub fn set_goal(&self, text: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.goal_path(), format!("# GOAL\n\n{text}\n"))
    }

    /// The north-star text, if a goal is set.
    pub fn goal(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.goal_path()).ok()?;
        let body = text.strip_prefix("# GOAL").unwrap_or(&text).trim();
        (!body.is_empty()).then(|| body.to_string())
    }

    pub fn progress(&self) -> String {
        std::fs::read_to_string(self.progress_path()).unwrap_or_default()
    }

    pub fn set_progress(&self, text: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.progress_path(), text)
    }
}

/// Bounds for an unattended ralph-loop.
#[derive(Debug, Clone, Copy)]
pub struct Guardrails {
    pub max_iterations: usize,
    pub cost_cap_usd: Option<f64>,
}

impl Default for Guardrails {
    fn default() -> Self {
        Guardrails {
            max_iterations: 25,
            cost_cap_usd: Some(5.0),
        }
    }
}

impl Guardrails {
    /// Whether the loop should run another iteration.
    pub fn should_continue(&self, iteration: usize, spent_usd: f64, goal_done: bool) -> bool {
        if goal_done || iteration >= self.max_iterations {
            return false;
        }
        match self.cost_cap_usd {
            Some(cap) => spent_usd < cap,
            None => true,
        }
    }

    /// A human-readable reason the loop stopped (for the UI).
    pub fn stop_reason(
        &self,
        iteration: usize,
        spent_usd: f64,
        goal_done: bool,
    ) -> Option<&'static str> {
        if goal_done {
            Some("goal complete")
        } else if iteration >= self.max_iterations {
            Some("iteration cap reached")
        } else if self.cost_cap_usd.is_some_and(|cap| spent_usd >= cap) {
            Some("cost cap reached")
        } else {
            None
        }
    }
}

/// Build the per-iteration prompt: the goal and the latest progress notes.
pub fn iteration_prompt(goal: &str, progress: &str) -> String {
    let mut p = format!("Your goal:\n{goal}\n");
    if !progress.trim().is_empty() {
        p.push_str(&format!("\nProgress so far:\n{}\n", progress.trim()));
    }
    p.push_str(
        "\nDo the next concrete step toward the goal, then update your progress notes. If the \
         goal is fully complete, say DONE.",
    );
    p
}

/// Whether an assistant reply signals completion.
pub fn is_done(reply: &str) -> bool {
    reply
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case("done") || l.trim() == "DONE.")
}

/// Convenience: the `.cordy` dir for a working directory.
pub fn cordy_dir(cwd: &Path) -> PathBuf {
    cwd.join(".cordy")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = GoalStore::new(dir.path());
        assert!(store.goal().is_none());
        store.set_goal("ship the parser").unwrap();
        assert_eq!(store.goal().as_deref(), Some("ship the parser"));
    }

    #[test]
    fn guardrails_stop_conditions() {
        let g = Guardrails {
            max_iterations: 3,
            cost_cap_usd: Some(1.0),
        };
        assert!(g.should_continue(0, 0.0, false));
        assert!(!g.should_continue(0, 0.0, true), "done stops");
        assert!(!g.should_continue(3, 0.0, false), "iteration cap stops");
        assert!(!g.should_continue(1, 1.5, false), "cost cap stops");
        assert_eq!(g.stop_reason(3, 0.0, false), Some("iteration cap reached"));
        assert_eq!(g.stop_reason(1, 2.0, false), Some("cost cap reached"));
    }

    #[test]
    fn detects_done_and_builds_prompt() {
        assert!(is_done("looks good\nDONE"));
        assert!(is_done("DONE."));
        assert!(!is_done("not done yet"));
        let p = iteration_prompt("build X", "did A");
        assert!(p.contains("build X"));
        assert!(p.contains("did A"));
    }
}
