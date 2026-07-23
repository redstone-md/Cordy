//! The hidden prompts that steer a goal.
//!
//! Three moments need a message the user never typed: continuing an active goal into a new turn,
//! telling the model to wrap up once a budget is spent, and re-pointing an in-flight turn after the
//! user edits the objective. Each is a template with `{{placeholder}}` slots; the objective is
//! XML-escaped and framed as untrusted data so a hostile objective can't impersonate instructions.

use crate::core::goal::{Goal, GoalLimits};

const CONTINUATION: &str = include_str!("templates/continuation.md");
const BUDGET_LIMIT: &str = include_str!("templates/budget_limit.md");
const OBJECTIVE_UPDATED: &str = include_str!("templates/objective_updated.md");

/// Substitute `{{name}}` placeholders. Unknown placeholders are left as-is so a typo shows up in
/// the prompt instead of silently vanishing.
fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, value) in vars {
        out = out.replace(&format!("{{{{{name}}}}}"), value);
    }
    out
}

fn escape_xml_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The `Budget:` block — always tokens, plus whichever local caps are configured.
fn budget_block(goal: &Goal) -> String {
    let GoalLimits {
        token_budget,
        cost_cap_usd,
        max_iterations,
    } = goal.limits;
    let mut lines = vec![format!("- Tokens used: {}", goal.tokens_used)];
    match token_budget {
        Some(budget) => {
            lines.push(format!("- Token budget: {budget}"));
            lines.push(format!(
                "- Tokens remaining: {}",
                goal.remaining_tokens().unwrap_or(0)
            ));
        }
        None => {
            lines.push("- Token budget: none".to_string());
            lines.push("- Tokens remaining: unbounded".to_string());
        }
    }
    if goal.time_used_seconds > 0 {
        lines.push(format!(
            "- Time spent pursuing goal: {} seconds",
            goal.time_used_seconds
        ));
    }
    if let Some(cap) = cost_cap_usd {
        lines.push(format!(
            "- Cost used: ${:.2} of ${cap:.2}",
            goal.cost_used_usd
        ));
    }
    if let Some(cap) = max_iterations {
        lines.push(format!(
            "- Goal turns used: {} of {cap}",
            goal.iterations_used
        ));
    }
    lines.join("\n")
}

/// Hidden prompt that carries an active goal into the next turn.
pub fn continuation_prompt(goal: &Goal) -> String {
    let objective = escape_xml_text(&goal.objective);
    render(
        CONTINUATION,
        &[
            ("objective", objective.as_str()),
            ("budget", budget_block(goal).as_str()),
        ],
    )
}

/// Hidden prompt asking the model to wrap up after a cap is reached.
pub fn budget_limit_prompt(goal: &Goal) -> String {
    let objective = escape_xml_text(&goal.objective);
    render(
        BUDGET_LIMIT,
        &[
            ("objective", objective.as_str()),
            ("budget", budget_block(goal).as_str()),
            ("cause", goal.budget_cause().unwrap_or("budget")),
        ],
    )
}

/// Hidden prompt injected into a running turn after the user edits the objective.
pub fn objective_updated_prompt(goal: &Goal) -> String {
    let objective = escape_xml_text(&goal.objective);
    render(
        OBJECTIVE_UPDATED,
        &[
            ("objective", objective.as_str()),
            ("budget", budget_block(goal).as_str()),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::goal::{GoalStatus, GoalStore};

    fn goal_with(limits: GoalLimits) -> Goal {
        let store = GoalStore::ephemeral();
        store
            .insert("make <it> work & ship", GoalStatus::Active, limits)
            .unwrap()
    }

    #[test]
    fn continuation_escapes_the_objective_and_reports_the_budget() {
        let mut goal = goal_with(GoalLimits::tokens(Some(1000)));
        goal.tokens_used = 250;
        let p = continuation_prompt(&goal);
        assert!(p.contains("make &lt;it&gt; work &amp; ship"));
        assert!(p.contains("- Tokens used: 250"));
        assert!(p.contains("- Token budget: 1000"));
        assert!(p.contains("- Tokens remaining: 750"));
        assert!(p.contains("Completion audit:"));
        assert!(!p.contains("{{"), "every placeholder is filled");
    }

    #[test]
    fn unbounded_goal_says_so() {
        let goal = goal_with(GoalLimits::default());
        let p = continuation_prompt(&goal);
        assert!(p.contains("- Token budget: none"));
        assert!(p.contains("- Tokens remaining: unbounded"));
    }

    #[test]
    fn budget_limit_names_the_cap_that_tripped() {
        let mut goal = goal_with(GoalLimits {
            max_iterations: Some(3),
            ..Default::default()
        });
        goal.iterations_used = 3;
        let p = budget_limit_prompt(&goal);
        assert!(p.contains("has reached its turn cap"));
        assert!(p.contains("- Goal turns used: 3 of 3"));
        assert!(p.contains("do not start new substantive work"));
    }

    #[test]
    fn objective_updated_supersedes_the_previous_objective() {
        let goal = goal_with(GoalLimits::default());
        let p = objective_updated_prompt(&goal);
        assert!(p.contains("supersedes any previous session goal objective"));
        assert!(p.contains("<untrusted_objective>"));
        assert!(!p.contains("{{"));
    }
}
