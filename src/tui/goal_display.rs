//! Rendering helpers for the session goal: status labels, elapsed time, and the usage summary.

use crate::core::goal::{Goal, GoalStatus};

pub const GOAL_USAGE: &str =
    "usage: /goal [<objective> [--budget N] [--cost N] [--turns N]|edit|pause|resume|clear]";

/// Compact elapsed time: `45s`, `30m`, `1h 30m`, `2d 23h 42m`.
pub fn format_goal_elapsed_seconds(seconds: i64) -> String {
    let seconds = seconds.max(0) as u64;
    if seconds < 60 {
        return format!("{seconds}s");
    }

    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }

    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;
    if hours >= 24 {
        let days = hours / 24;
        let remaining_hours = hours % 24;
        return format!("{days}d {remaining_hours}h {remaining_minutes}m");
    }

    if remaining_minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {remaining_minutes}m")
    }
}

/// Short token count: `950`, `63.9K`, `1.2M`.
pub fn format_tokens_compact(tokens: i64) -> String {
    let tokens = tokens.max(0);
    if tokens < 1_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        let thousands = tokens as f64 / 1_000.0;
        return if thousands.fract() == 0.0 || thousands >= 100.0 {
            format!("{}K", thousands.round())
        } else {
            format!("{thousands:.1}K")
        };
    }
    let millions = tokens as f64 / 1_000_000.0;
    if millions.fract() == 0.0 || millions >= 100.0 {
        format!("{}M", millions.round())
    } else {
        format!("{millions:.1}M")
    }
}

pub fn goal_status_label(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Blocked => "blocked",
        GoalStatus::UsageLimited => "usage limited",
        GoalStatus::BudgetLimited => "limited by budget",
        GoalStatus::Complete => "complete",
    }
}

/// One-line "what has this goal cost so far" summary.
pub fn goal_usage_summary(goal: &Goal) -> String {
    let mut parts = vec![format!("Objective: {}", goal.objective)];
    if goal.time_used_seconds > 0 {
        parts.push(format!(
            "Time: {}.",
            format_goal_elapsed_seconds(goal.time_used_seconds)
        ));
    }
    match goal.limits.token_budget {
        Some(budget) => parts.push(format!(
            "Tokens: {}/{}.",
            format_tokens_compact(goal.tokens_used),
            format_tokens_compact(budget)
        )),
        None if goal.tokens_used > 0 => parts.push(format!(
            "Tokens: {}.",
            format_tokens_compact(goal.tokens_used)
        )),
        None => {}
    }
    if let Some(cap) = goal.limits.cost_cap_usd {
        parts.push(format!("Cost: ${:.2}/${cap:.2}.", goal.cost_used_usd));
    }
    if let Some(cap) = goal.limits.max_iterations {
        parts.push(format!("Turns: {}/{cap}.", goal.iterations_used));
    }
    parts.join(" ")
}

/// The status-bar chip: `goal: active · 12.5K/50K · 3m`.
pub fn goal_status_line(goal: &Goal) -> String {
    let mut parts = vec![goal_status_label(goal.status).to_string()];
    if let Some(budget) = goal.limits.token_budget {
        parts.push(format!(
            "{}/{}",
            format_tokens_compact(goal.tokens_used),
            format_tokens_compact(budget)
        ));
    } else if goal.tokens_used > 0 {
        parts.push(format_tokens_compact(goal.tokens_used));
    }
    if goal.time_used_seconds > 0 {
        parts.push(format_goal_elapsed_seconds(goal.time_used_seconds));
    }
    format!("goal: {}", parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::goal::GoalLimits;

    fn goal(token_budget: Option<i64>, tokens_used: i64) -> Goal {
        Goal {
            goal_id: "g1".into(),
            objective: "Complete the task described in ../prompt5.txt".into(),
            status: GoalStatus::BudgetLimited,
            limits: GoalLimits::tokens(token_budget),
            tokens_used,
            time_used_seconds: 120,
            cost_used_usd: 0.0,
            iterations_used: 0,
            created_at: 0,
            updated_at: 0,
            continuation_deferred: false,
        }
    }

    #[test]
    fn elapsed_seconds_are_compact() {
        assert_eq!(format_goal_elapsed_seconds(0), "0s");
        assert_eq!(format_goal_elapsed_seconds(59), "59s");
        assert_eq!(format_goal_elapsed_seconds(60), "1m");
        assert_eq!(format_goal_elapsed_seconds(30 * 60), "30m");
        assert_eq!(format_goal_elapsed_seconds(90 * 60), "1h 30m");
        assert_eq!(format_goal_elapsed_seconds(2 * 60 * 60), "2h");
        assert_eq!(format_goal_elapsed_seconds(24 * 60 * 60 - 1), "23h 59m");
        assert_eq!(format_goal_elapsed_seconds(24 * 60 * 60), "1d 0h 0m");
        let almost_three_days = 2 * 24 * 60 * 60 + 23 * 60 * 60 + 42 * 60;
        assert_eq!(format_goal_elapsed_seconds(almost_three_days), "2d 23h 42m");
    }

    #[test]
    fn tokens_are_compact() {
        assert_eq!(format_tokens_compact(950), "950");
        assert_eq!(format_tokens_compact(50_000), "50K");
        assert_eq!(format_tokens_compact(63_876), "63.9K");
        assert_eq!(format_tokens_compact(1_500_000), "1.5M");
    }

    #[test]
    fn usage_summary_reports_time_and_budgeted_tokens() {
        assert_eq!(
            goal_usage_summary(&goal(Some(50_000), 63_876)),
            "Objective: Complete the task described in ../prompt5.txt Time: 2m. Tokens: 63.9K/50K."
        );
    }

    #[test]
    fn status_line_is_short_enough_for_the_footer() {
        assert_eq!(
            goal_status_line(&goal(Some(50_000), 12_500)),
            "goal: limited by budget · 12.5K/50K · 2m"
        );
    }
}
