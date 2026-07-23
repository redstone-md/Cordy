//! Thread goals — a persistent objective the agent keeps pursuing across turns.
//!
//! A goal is a durable objective attached to one session. While it is [`GoalStatus::Active`] the
//! agent keeps taking turns toward it on its own; usage (tokens, wall time, cost, turns) is
//! charged against it as work happens, and crossing a cap flips it to
//! [`GoalStatus::BudgetLimited`] so the model is told to wrap up instead of starting new work.
//!
//! The status machine is deliberately strict: only the model's `update_goal` tool can mark a goal
//! `complete` or `blocked`, and only the user or the system can pause, resume, budget-limit, or
//! usage-limit it. A terminal status is never silently overwritten — asking a budget-limited goal
//! to pause leaves it budget-limited, and reactivating a goal that is already over budget lands
//! straight back on budget-limited.
//!
//! State lives in one JSON file next to the session log, so a goal survives `--resume` and travels
//! with a forked session.

pub mod accounting;
pub mod prompts;
pub mod runtime;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::core::session_store::now_unix;

/// Where a goal is in its life cycle.
///
/// `Paused`, `UsageLimited` and `BudgetLimited` are all "stopped but resumable"; `Blocked` means
/// the agent hit a real impasse; `Complete` is the only status that lets a new goal replace it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::Blocked => "blocked",
            GoalStatus::UsageLimited => "usage_limited",
            GoalStatus::BudgetLimited => "budget_limited",
            GoalStatus::Complete => "complete",
        }
    }

    pub fn is_active(self) -> bool {
        self == GoalStatus::Active
    }

    /// Whether the goal has stopped in a way that ends the automatic loop.
    pub fn is_terminal(self) -> bool {
        matches!(self, GoalStatus::BudgetLimited | GoalStatus::Complete)
    }
}

impl std::str::FromStr for GoalStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(GoalStatus::Active),
            "paused" => Ok(GoalStatus::Paused),
            "blocked" => Ok(GoalStatus::Blocked),
            "usage_limited" => Ok(GoalStatus::UsageLimited),
            "budget_limited" => Ok(GoalStatus::BudgetLimited),
            "complete" => Ok(GoalStatus::Complete),
            other => Err(format!("unknown goal status `{other}`")),
        }
    }
}

/// The caps that stop a goal. All are optional; whichever is reached first wins.
///
/// `token_budget` is the portable one (it is what the model sees and reasons about); `cost_cap_usd`
/// and `max_iterations` are local safety rails for unattended runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct GoalLimits {
    #[serde(default)]
    pub token_budget: Option<i64>,
    #[serde(default)]
    pub cost_cap_usd: Option<f64>,
    #[serde(default)]
    pub max_iterations: Option<u32>,
}

impl GoalLimits {
    pub fn tokens(token_budget: Option<i64>) -> Self {
        GoalLimits {
            token_budget,
            ..Default::default()
        }
    }

    pub fn is_unbounded(&self) -> bool {
        self.token_budget.is_none() && self.cost_cap_usd.is_none() && self.max_iterations.is_none()
    }
}

/// A goal and everything charged against it so far.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    /// Identifies this particular goal; used as a compare-and-swap guard so usage accounted for a
    /// goal that has since been replaced is dropped rather than charged to its successor.
    pub goal_id: String,
    pub objective: String,
    pub status: GoalStatus,
    #[serde(default)]
    pub limits: GoalLimits,
    #[serde(default)]
    pub tokens_used: i64,
    #[serde(default)]
    pub time_used_seconds: i64,
    #[serde(default)]
    pub cost_used_usd: f64,
    #[serde(default)]
    pub iterations_used: u32,
    pub created_at: u64,
    pub updated_at: u64,
    /// Suppresses one round of automatic continuation (set when a goal is restored from disk, so
    /// resuming a session doesn't immediately spend tokens before the user has said anything).
    #[serde(default)]
    pub continuation_deferred: bool,
}

impl Goal {
    /// Tokens left before the budget stops the goal; `None` when there is no token budget.
    pub fn remaining_tokens(&self) -> Option<i64> {
        self.limits
            .token_budget
            .map(|budget| (budget - self.tokens_used).max(0))
    }

    /// Whether any configured cap has been reached.
    pub fn over_budget(&self) -> bool {
        self.limits
            .token_budget
            .is_some_and(|budget| self.tokens_used >= budget)
            || self
                .limits
                .cost_cap_usd
                .is_some_and(|cap| self.cost_used_usd >= cap)
            || self
                .limits
                .max_iterations
                .is_some_and(|cap| self.iterations_used >= cap)
    }

    /// Which cap tripped, for the wrap-up prompt and the UI.
    pub fn budget_cause(&self) -> Option<&'static str> {
        if self
            .limits
            .token_budget
            .is_some_and(|budget| self.tokens_used >= budget)
        {
            Some("token budget")
        } else if self
            .limits
            .cost_cap_usd
            .is_some_and(|cap| self.cost_used_usd >= cap)
        {
            Some("cost cap")
        } else if self
            .limits
            .max_iterations
            .is_some_and(|cap| self.iterations_used >= cap)
        {
            Some("turn cap")
        } else {
            None
        }
    }
}

/// A partial edit of the stored goal. `None` fields are left untouched.
#[derive(Debug, Clone, Default)]
pub struct GoalUpdate {
    pub objective: Option<String>,
    pub status: Option<GoalStatus>,
    pub limits: Option<GoalLimits>,
    /// When set, the update only applies if the stored goal still has this id.
    pub expected_goal_id: Option<String>,
}

/// Which statuses a usage-accounting write is allowed to touch.
///
/// Accounting runs at several points in a turn and must not resurrect a goal that has already
/// stopped, so each caller picks the narrowest mode that still records its own work: a mid-turn
/// tool completion uses [`GoalAccountingMode::ActiveOnly`], while the `update_goal` call that
/// finishes the goal must also be able to charge the goal it just completed.
// The names describe which statuses each mode accepts; the shared prefix is the point.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalAccountingMode {
    ActiveStatusOnly,
    ActiveOnly,
    ActiveOrComplete,
    ActiveOrStopped,
}

impl GoalAccountingMode {
    /// Statuses whose usage this mode records.
    fn accepts(self, status: GoalStatus) -> bool {
        match self {
            GoalAccountingMode::ActiveStatusOnly => status == GoalStatus::Active,
            GoalAccountingMode::ActiveOnly => {
                matches!(status, GoalStatus::Active | GoalStatus::BudgetLimited)
            }
            GoalAccountingMode::ActiveOrComplete => matches!(
                status,
                GoalStatus::Active | GoalStatus::BudgetLimited | GoalStatus::Complete
            ),
            GoalAccountingMode::ActiveOrStopped => status != GoalStatus::Complete,
        }
    }

    /// Statuses this mode may promote to [`GoalStatus::BudgetLimited`] when a cap is reached.
    fn may_budget_limit(self, status: GoalStatus) -> bool {
        match self {
            GoalAccountingMode::ActiveStatusOnly
            | GoalAccountingMode::ActiveOnly
            | GoalAccountingMode::ActiveOrComplete => status == GoalStatus::Active,
            GoalAccountingMode::ActiveOrStopped => status != GoalStatus::Complete,
        }
    }
}

/// Result of charging usage: `Updated` when the write landed, `Unchanged` when the goal was in a
/// status this mode doesn't touch (or the delta was empty).
#[derive(Debug, Clone)]
pub enum GoalAccountingOutcome {
    Unchanged(Option<Goal>),
    Updated(Goal),
}

/// Work to charge against a goal.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageDelta {
    pub seconds: i64,
    pub tokens: i64,
    pub cost_usd: f64,
    pub iterations: u32,
}

impl UsageDelta {
    fn is_empty(&self) -> bool {
        self.seconds <= 0 && self.tokens <= 0 && self.cost_usd <= 0.0 && self.iterations == 0
    }

    fn clamped(mut self) -> Self {
        self.seconds = self.seconds.max(0);
        self.tokens = self.tokens.max(0);
        if !self.cost_usd.is_finite() || self.cost_usd < 0.0 {
            self.cost_usd = 0.0;
        }
        self
    }
}

pub const MAX_OBJECTIVE_CHARS: usize = 12_000;

/// Reject objectives that are empty or too long to keep re-sending every turn.
pub fn validate_objective(objective: &str) -> Result<(), String> {
    if objective.trim().is_empty() {
        return Err("goal objective must not be empty".to_string());
    }
    let chars = objective.chars().count();
    if chars > MAX_OBJECTIVE_CHARS {
        return Err(format!(
            "goal objective is too long: {chars} characters. Limit: {MAX_OBJECTIVE_CHARS} characters."
        ));
    }
    Ok(())
}

/// Reject non-positive budgets (a zero budget would stop the goal before it starts).
pub fn validate_limits(limits: &GoalLimits) -> Result<(), String> {
    if limits.token_budget.is_some_and(|value| value <= 0) {
        return Err("goal budgets must be positive when provided".to_string());
    }
    if limits.cost_cap_usd.is_some_and(|value| value <= 0.0) {
        return Err("goal cost caps must be positive when provided".to_string());
    }
    if limits.max_iterations.is_some_and(|value| value == 0) {
        return Err("goal turn caps must be positive when provided".to_string());
    }
    Ok(())
}

/// The goal for one session, persisted as JSON.
///
/// Every mutation writes the whole file (atomically, via a temp file + rename), which is plenty for
/// a single object touched a few times per turn.
pub struct GoalStore {
    path: PathBuf,
    state: Mutex<Option<Goal>>,
}

impl GoalStore {
    /// Open the store at `path`, loading an existing goal if the file is there. A restored goal
    /// starts with its continuation deferred so resuming a session never auto-starts a turn.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut goal: Option<Goal> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        if let Some(g) = goal.as_mut() {
            g.continuation_deferred = true;
        }
        GoalStore {
            path,
            state: Mutex::new(goal),
        }
    }

    /// A store with no backing file (tests, headless runs with persistence off).
    pub fn ephemeral() -> Self {
        GoalStore {
            path: PathBuf::new(),
            state: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Goal>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn persist(&self, goal: Option<&Goal>) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        match goal {
            None => {
                let _ = std::fs::remove_file(&self.path);
            }
            Some(goal) => {
                let Ok(text) = serde_json::to_string_pretty(goal) else {
                    return;
                };
                if let Some(parent) = self.path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let tmp = self.path.with_extension("json.tmp");
                if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &self.path).is_err()
                {
                    // Windows rename onto an existing file can fail; fall back to a plain write.
                    let _ = std::fs::copy(&tmp, &self.path);
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        }
    }

    pub fn get(&self) -> Option<Goal> {
        self.lock().clone()
    }

    /// Start a new goal. Returns `None` when an unfinished goal already exists — only a completed
    /// goal may be replaced, which is what keeps one session on one objective at a time.
    pub fn insert(&self, objective: &str, status: GoalStatus, limits: GoalLimits) -> Option<Goal> {
        let mut slot = self.lock();
        if slot
            .as_ref()
            .is_some_and(|existing| existing.status != GoalStatus::Complete)
        {
            return None;
        }
        let now = now_unix();
        let mut goal = Goal {
            goal_id: new_goal_id(),
            objective: objective.to_string(),
            status,
            limits,
            tokens_used: 0,
            time_used_seconds: 0,
            cost_used_usd: 0.0,
            iterations_used: 0,
            created_at: now,
            updated_at: now,
            continuation_deferred: false,
        };
        goal.status = status_after_budget_limit(&goal);
        *slot = Some(goal.clone());
        self.persist(Some(&goal));
        Some(goal)
    }

    /// Overwrite the goal wholesale (session fork / rewind), deferring one continuation.
    pub fn replace_snapshot(&self, goal: Option<Goal>) {
        let mut slot = self.lock();
        let goal = goal.map(|mut g| {
            g.continuation_deferred = true;
            g
        });
        *slot = goal.clone();
        self.persist(goal.as_ref());
    }

    /// Apply a partial edit.
    ///
    /// Terminal statuses win over de-escalations: a budget-limited goal asked to pause or block
    /// stays budget-limited, and reactivating a goal that is already over budget re-lands on
    /// budget-limited rather than burning more tokens.
    pub fn update(&self, update: GoalUpdate) -> Option<Goal> {
        let mut slot = self.lock();
        let goal = slot.as_mut()?;
        if let Some(expected) = update.expected_goal_id.as_deref()
            && goal.goal_id != expected
        {
            return None;
        }
        if let Some(objective) = update.objective {
            goal.objective = objective;
        }
        if let Some(limits) = update.limits {
            goal.limits = limits;
        }
        if let Some(status) = update.status {
            let keep_budget_limited = goal.status == GoalStatus::BudgetLimited
                && matches!(status, GoalStatus::Paused | GoalStatus::Blocked);
            if !keep_budget_limited {
                goal.status = status;
            }
        }
        goal.status = status_after_budget_limit(goal);
        goal.updated_at = now_unix();
        let updated = goal.clone();
        self.persist(Some(&updated));
        Some(updated)
    }

    /// Pause the goal. Only an active goal can be paused — a stopped goal keeps its status so the
    /// reason it stopped isn't lost.
    pub fn pause_active(&self) -> Option<Goal> {
        self.set_stopped_status(GoalStatus::Paused)
    }

    /// Mark the goal usage-limited (provider quota). Applies to active and budget-limited goals.
    pub fn usage_limit_active(&self) -> Option<Goal> {
        self.set_stopped_status(GoalStatus::UsageLimited)
    }

    fn set_stopped_status(&self, status: GoalStatus) -> Option<Goal> {
        let mut slot = self.lock();
        let goal = slot.as_mut()?;
        let allowed = goal.status == GoalStatus::Active
            || (status == GoalStatus::UsageLimited && goal.status == GoalStatus::BudgetLimited);
        if !allowed {
            return None;
        }
        goal.status = status;
        goal.updated_at = now_unix();
        let updated = goal.clone();
        self.persist(Some(&updated));
        Some(updated)
    }

    /// Drop the goal entirely.
    pub fn delete(&self) -> Option<Goal> {
        let mut slot = self.lock();
        let previous = slot.take();
        if previous.is_some() {
            self.persist(None);
        }
        previous
    }

    pub fn has_continuation_deferral(&self) -> bool {
        self.lock()
            .as_ref()
            .is_some_and(|goal| goal.continuation_deferred)
    }

    pub fn clear_continuation_deferral(&self) {
        let mut slot = self.lock();
        if let Some(goal) = slot.as_mut()
            && goal.continuation_deferred
        {
            goal.continuation_deferred = false;
            let updated = goal.clone();
            self.persist(Some(&updated));
        }
    }

    /// Charge `delta` against the goal, promoting it to [`GoalStatus::BudgetLimited`] if that
    /// pushes it over a cap.
    ///
    /// `expected_goal_id` guards against charging work to a goal that was replaced mid-flight.
    pub fn account_usage(
        &self,
        delta: UsageDelta,
        mode: GoalAccountingMode,
        expected_goal_id: Option<&str>,
    ) -> GoalAccountingOutcome {
        let delta = delta.clamped();
        let mut slot = self.lock();
        if delta.is_empty() {
            return GoalAccountingOutcome::Unchanged(slot.clone());
        }
        let Some(goal) = slot.as_mut() else {
            return GoalAccountingOutcome::Unchanged(None);
        };
        if expected_goal_id.is_some_and(|expected| goal.goal_id != expected)
            || !mode.accepts(goal.status)
        {
            return GoalAccountingOutcome::Unchanged(Some(goal.clone()));
        }

        let may_budget_limit = mode.may_budget_limit(goal.status);
        goal.time_used_seconds = goal.time_used_seconds.saturating_add(delta.seconds);
        goal.tokens_used = goal.tokens_used.saturating_add(delta.tokens);
        goal.cost_used_usd += delta.cost_usd;
        goal.iterations_used = goal.iterations_used.saturating_add(delta.iterations);
        if may_budget_limit && goal.over_budget() {
            goal.status = GoalStatus::BudgetLimited;
        }
        goal.updated_at = now_unix();
        let updated = goal.clone();
        self.persist(Some(&updated));
        GoalAccountingOutcome::Updated(updated)
    }
}

/// An active goal that is already over one of its caps starts (or restarts) budget-limited.
fn status_after_budget_limit(goal: &Goal) -> GoalStatus {
    if goal.status == GoalStatus::Active && goal.over_budget() {
        GoalStatus::BudgetLimited
    } else {
        goal.status
    }
}

fn new_goal_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        bytes[..16].copy_from_slice(&ms.to_le_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> GoalStore {
        GoalStore::ephemeral()
    }

    fn active(store: &GoalStore, limits: GoalLimits) -> Goal {
        store
            .insert("ship the parser", GoalStatus::Active, limits)
            .unwrap()
    }

    #[test]
    fn insert_refuses_to_replace_an_unfinished_goal() {
        let s = store();
        active(&s, GoalLimits::default());
        assert!(
            s.insert("something else", GoalStatus::Active, GoalLimits::default())
                .is_none()
        );

        s.update(GoalUpdate {
            status: Some(GoalStatus::Complete),
            ..Default::default()
        });
        let replaced = s
            .insert("something else", GoalStatus::Active, GoalLimits::default())
            .expect("a completed goal may be replaced");
        assert_eq!(replaced.objective, "something else");
        assert_eq!(replaced.tokens_used, 0, "usage resets with the new goal");
    }

    #[test]
    fn new_goal_already_over_budget_starts_budget_limited() {
        let s = store();
        // A zero-iteration cap is rejected up front, so use the accounting path for the "already
        // over" case: create with a tiny budget, spend it, then reactivate.
        let goal = active(&s, GoalLimits::tokens(Some(10)));
        s.account_usage(
            UsageDelta {
                tokens: 25,
                ..Default::default()
            },
            GoalAccountingMode::ActiveOnly,
            Some(&goal.goal_id),
        );
        let after = s.get().unwrap();
        assert_eq!(after.status, GoalStatus::BudgetLimited);

        let reactivated = s
            .update(GoalUpdate {
                status: Some(GoalStatus::Active),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            reactivated.status,
            GoalStatus::BudgetLimited,
            "reactivating an over-budget goal stays budget-limited"
        );
    }

    #[test]
    fn budget_limited_survives_pause_and_block() {
        let s = store();
        let goal = active(&s, GoalLimits::tokens(Some(10)));
        s.account_usage(
            UsageDelta {
                tokens: 10,
                ..Default::default()
            },
            GoalAccountingMode::ActiveOnly,
            Some(&goal.goal_id),
        );
        assert_eq!(s.get().unwrap().status, GoalStatus::BudgetLimited);

        assert!(
            s.pause_active().is_none(),
            "pause only applies to active goals"
        );
        let blocked = s
            .update(GoalUpdate {
                status: Some(GoalStatus::Blocked),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(blocked.status, GoalStatus::BudgetLimited);

        let usage_limited = s.usage_limit_active().unwrap();
        assert_eq!(
            usage_limited.status,
            GoalStatus::UsageLimited,
            "a provider quota outranks the local budget stop"
        );
    }

    #[test]
    fn accounting_modes_gate_which_statuses_are_charged() {
        let s = store();
        let goal = active(&s, GoalLimits::default());
        s.update(GoalUpdate {
            status: Some(GoalStatus::Paused),
            ..Default::default()
        });

        let delta = UsageDelta {
            tokens: 5,
            ..Default::default()
        };
        assert!(matches!(
            s.account_usage(delta, GoalAccountingMode::ActiveOnly, Some(&goal.goal_id)),
            GoalAccountingOutcome::Unchanged(_)
        ));
        assert!(matches!(
            s.account_usage(
                delta,
                GoalAccountingMode::ActiveOrStopped,
                Some(&goal.goal_id)
            ),
            GoalAccountingOutcome::Updated(_)
        ));
        assert_eq!(s.get().unwrap().tokens_used, 5);
    }

    #[test]
    fn accounting_ignores_a_replaced_goal() {
        let s = store();
        let first = active(&s, GoalLimits::default());
        s.update(GoalUpdate {
            status: Some(GoalStatus::Complete),
            ..Default::default()
        });
        s.insert("next objective", GoalStatus::Active, GoalLimits::default());

        let outcome = s.account_usage(
            UsageDelta {
                tokens: 100,
                ..Default::default()
            },
            GoalAccountingMode::ActiveOnly,
            Some(&first.goal_id),
        );
        assert!(matches!(outcome, GoalAccountingOutcome::Unchanged(_)));
        assert_eq!(s.get().unwrap().tokens_used, 0);
    }

    #[test]
    fn each_cap_can_trip_the_budget_limit() {
        for (limits, delta, cause) in [
            (
                GoalLimits::tokens(Some(100)),
                UsageDelta {
                    tokens: 100,
                    ..Default::default()
                },
                "token budget",
            ),
            (
                GoalLimits {
                    cost_cap_usd: Some(1.0),
                    ..Default::default()
                },
                UsageDelta {
                    cost_usd: 1.5,
                    ..Default::default()
                },
                "cost cap",
            ),
            (
                GoalLimits {
                    max_iterations: Some(3),
                    ..Default::default()
                },
                UsageDelta {
                    iterations: 3,
                    ..Default::default()
                },
                "turn cap",
            ),
        ] {
            let s = store();
            let goal = active(&s, limits);
            s.account_usage(delta, GoalAccountingMode::ActiveOnly, Some(&goal.goal_id));
            let after = s.get().unwrap();
            assert_eq!(after.status, GoalStatus::BudgetLimited, "{cause}");
            assert_eq!(after.budget_cause(), Some(cause));
        }
    }

    #[test]
    fn empty_delta_leaves_the_goal_untouched() {
        let s = store();
        let goal = active(&s, GoalLimits::default());
        let outcome = s.account_usage(
            UsageDelta::default(),
            GoalAccountingMode::ActiveOnly,
            Some(&goal.goal_id),
        );
        assert!(matches!(outcome, GoalAccountingOutcome::Unchanged(Some(_))));
    }

    #[test]
    fn round_trips_through_disk_with_a_deferred_continuation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.goal.json");
        let s = GoalStore::open(&path);
        let goal = s
            .insert("ship it", GoalStatus::Active, GoalLimits::tokens(Some(50)))
            .unwrap();
        assert!(!s.has_continuation_deferral());

        let reopened = GoalStore::open(&path);
        let loaded = reopened.get().unwrap();
        assert_eq!(loaded.goal_id, goal.goal_id);
        assert_eq!(loaded.objective, "ship it");
        assert!(
            reopened.has_continuation_deferral(),
            "a restored goal waits for the user before continuing"
        );
        reopened.clear_continuation_deferral();
        assert!(!reopened.has_continuation_deferral());

        reopened.delete();
        assert!(!path.exists());
    }

    #[test]
    fn validation_rejects_empty_objectives_and_non_positive_budgets() {
        assert!(validate_objective("  ").is_err());
        assert!(validate_objective("do the thing").is_ok());
        assert!(validate_limits(&GoalLimits::tokens(Some(0))).is_err());
        assert!(validate_limits(&GoalLimits::tokens(Some(1))).is_ok());
        assert!(
            validate_limits(&GoalLimits {
                cost_cap_usd: Some(-1.0),
                ..Default::default()
            })
            .is_err()
        );
    }
}
