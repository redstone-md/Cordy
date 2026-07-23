//! Turn-local bookkeeping that turns raw usage counters into goal deltas.
//!
//! The provider reports *cumulative* usage, and several places in a turn want to charge "whatever
//! has been spent since the last time we charged" — after every tool call, at turn end, and while
//! the thread sits idle with a goal still active. This module keeps the last-accounted watermark
//! per turn so those callers can run concurrently without double-charging the same tokens.
//!
//! The rule that makes it safe: take a [`ProgressSnapshot`], write it to the store, and only then
//! call [`GoalAccountingState::mark_progress_accounted`] — all while holding
//! [`GoalAccountingState::progress_permit`].

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, SemaphorePermit};

use crate::core::goal::{GoalStatus, UsageDelta};
use crate::core::types::Usage;

/// Tokens a usage counter contributes to a goal: fresh input plus output. Cache reads are excluded
/// because replaying a cached prefix is not new work toward the objective.
pub fn goal_tokens(usage: &Usage) -> i64 {
    let input = usage.input_tokens.saturating_sub(usage.cache_read);
    input.saturating_add(usage.output_tokens) as i64
}

fn usage_delta_tokens(last: &Usage, current: &Usage) -> i64 {
    let delta = Usage {
        input_tokens: current.input_tokens.saturating_sub(last.input_tokens),
        output_tokens: current.output_tokens.saturating_sub(last.output_tokens),
        cache_read: current.cache_read.saturating_sub(last.cache_read),
        cache_write: current.cache_write.saturating_sub(last.cache_write),
    };
    goal_tokens(&delta)
}

/// Whether a budget-limited goal stays attached to the turn after usage is charged.
///
/// A tool finishing mid-turn keeps it attached ([`KeepActive`](BudgetDisposition::KeepActive)) so
/// the wrap-up steering still lands; the end of a turn detaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetDisposition {
    KeepActive,
    ClearActive,
}

/// Usage to charge, plus the watermark to store once the write succeeds.
#[derive(Debug, Clone)]
pub struct ProgressSnapshot {
    pub expected_goal_id: String,
    pub delta: UsageDelta,
    current_usage: Usage,
    current_cost_usd: f64,
}

/// Wall-clock-only progress for a thread that is idle with an active goal.
#[derive(Debug, Clone)]
pub struct IdleProgressSnapshot {
    pub expected_goal_id: String,
    pub delta: UsageDelta,
}

#[derive(Debug)]
struct TurnAccounting {
    current_usage: Usage,
    last_accounted_usage: Usage,
    current_cost_usd: f64,
    last_accounted_cost_usd: f64,
    pending_iterations: u32,
    active_goal_id: Option<String>,
    account_tokens: bool,
}

impl TurnAccounting {
    fn new(usage: Usage, cost_usd: f64, account_tokens: bool) -> Self {
        TurnAccounting {
            current_usage: usage,
            last_accounted_usage: usage,
            current_cost_usd: cost_usd,
            last_accounted_cost_usd: cost_usd,
            pending_iterations: 0,
            active_goal_id: None,
            account_tokens,
        }
    }

    fn token_delta(&self) -> i64 {
        usage_delta_tokens(&self.last_accounted_usage, &self.current_usage)
    }

    fn cost_delta(&self) -> f64 {
        (self.current_cost_usd - self.last_accounted_cost_usd).max(0.0)
    }

    fn reset_baseline(&mut self) {
        self.last_accounted_usage = self.current_usage;
        self.last_accounted_cost_usd = self.current_cost_usd;
    }
}

#[derive(Debug)]
struct WallClock {
    last_accounted_at: Instant,
    active_goal_id: Option<String>,
}

impl WallClock {
    fn new() -> Self {
        WallClock {
            last_accounted_at: Instant::now(),
            active_goal_id: None,
        }
    }

    fn elapsed_seconds(&self) -> i64 {
        i64::try_from(self.last_accounted_at.elapsed().as_secs()).unwrap_or(i64::MAX)
    }

    fn mark_accounted(&mut self, seconds: i64) {
        if seconds <= 0 {
            return;
        }
        let advance = Duration::from_secs(u64::try_from(seconds).unwrap_or(u64::MAX));
        self.last_accounted_at = self
            .last_accounted_at
            .checked_add(advance)
            .unwrap_or_else(Instant::now);
    }

    fn reset_baseline(&mut self) {
        self.last_accounted_at = Instant::now();
    }

    fn mark_active(&mut self, goal_id: String) {
        if self.active_goal_id.as_deref() != Some(goal_id.as_str()) {
            self.reset_baseline();
            self.active_goal_id = Some(goal_id);
        }
    }

    fn clear_active(&mut self) {
        self.active_goal_id = None;
        self.reset_baseline();
    }
}

#[derive(Debug)]
struct Inner {
    current_turn_id: Option<String>,
    turns: HashMap<String, TurnAccounting>,
    wall_clock: WallClock,
    budget_limit_reported_goal_id: Option<String>,
}

/// Per-session accounting state. Cheap to clone behind an `Arc`; all methods take `&self`.
#[derive(Debug)]
pub struct GoalAccountingState {
    inner: Mutex<Inner>,
    progress_lock: Semaphore,
}

impl Default for GoalAccountingState {
    fn default() -> Self {
        GoalAccountingState {
            inner: Mutex::new(Inner {
                current_turn_id: None,
                turns: HashMap::new(),
                wall_clock: WallClock::new(),
                budget_limit_reported_goal_id: None,
            }),
            progress_lock: Semaphore::new(1),
        }
    }
}

impl GoalAccountingState {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Serializes accounting writes. Hold it from taking a snapshot until after the store write and
    /// the matching `mark_*_accounted` call, so two concurrent charges can't claim the same delta.
    pub async fn progress_permit(&self) -> Option<SemaphorePermit<'_>> {
        self.progress_lock.acquire().await.ok()
    }

    pub fn start_turn(&self, turn_id: impl Into<String>, usage: Usage, cost_usd: f64) {
        self.start_turn_with(turn_id, usage, cost_usd, /*account_tokens*/ true);
    }

    /// `account_tokens = false` records wall time but not tokens (used for turns that shouldn't be
    /// charged against the objective at all).
    pub fn start_turn_with(
        &self,
        turn_id: impl Into<String>,
        usage: Usage,
        cost_usd: f64,
        account_tokens: bool,
    ) {
        let turn_id = turn_id.into();
        let mut inner = self.inner();
        inner.current_turn_id = Some(turn_id.clone());
        inner.turns.insert(
            turn_id,
            TurnAccounting::new(usage, cost_usd, account_tokens),
        );
    }

    pub fn finish_turn(&self, turn_id: &str) {
        let mut inner = self.inner();
        inner.turns.remove(turn_id);
        if inner.current_turn_id.as_deref() == Some(turn_id) {
            inner.current_turn_id = None;
        }
    }

    pub fn current_turn_id(&self) -> Option<String> {
        self.inner().current_turn_id.clone()
    }

    /// Whether `turn_id` is the live turn and is charging a goal.
    pub fn turn_is_current_active_goal(&self, turn_id: &str) -> bool {
        let inner = self.inner();
        if inner.current_turn_id.as_deref() != Some(turn_id) {
            return false;
        }
        inner
            .turns
            .get(turn_id)
            .is_some_and(|turn| turn.account_tokens && turn.active_goal_id.is_some())
    }

    /// Record the latest cumulative counters for a turn.
    pub fn record_usage(&self, turn_id: &str, total_usage: Usage, total_cost_usd: f64) {
        let mut inner = self.inner();
        if let Some(turn) = inner.turns.get_mut(turn_id) {
            turn.current_usage = total_usage;
            turn.current_cost_usd = total_cost_usd;
        }
    }

    /// Count one completed model turn toward the goal's turn cap.
    pub fn record_iteration(&self, turn_id: &str) {
        let mut inner = self.inner();
        if let Some(turn) = inner.turns.get_mut(turn_id) {
            turn.pending_iterations = turn.pending_iterations.saturating_add(1);
        }
    }

    pub fn mark_turn_goal_active(&self, turn_id: &str, goal_id: impl Into<String>) {
        let goal_id = goal_id.into();
        let mut inner = self.inner();
        if inner.budget_limit_reported_goal_id.as_deref() != Some(goal_id.as_str()) {
            inner.budget_limit_reported_goal_id = None;
        }
        let is_current = inner.current_turn_id.as_deref() == Some(turn_id);
        if let Some(turn) = inner.turns.get_mut(turn_id) {
            turn.active_goal_id = Some(goal_id.clone());
        } else {
            return;
        }
        if is_current {
            inner.wall_clock.mark_active(goal_id);
        }
    }

    /// Attach a goal to the live turn, rebasing the watermark so work done before the goal existed
    /// isn't charged to it. Returns the turn id when there is a live turn.
    pub fn mark_current_turn_goal_active(&self, goal_id: impl Into<String>) -> Option<String> {
        let goal_id = goal_id.into();
        let mut inner = self.inner();
        let turn_id = inner.current_turn_id.clone()?;
        if inner.budget_limit_reported_goal_id.as_deref() != Some(goal_id.as_str()) {
            inner.budget_limit_reported_goal_id = None;
        }
        let turn = inner.turns.get_mut(turn_id.as_str())?;
        turn.active_goal_id = Some(goal_id.clone());
        turn.reset_baseline();
        turn.pending_iterations = 0;
        inner.wall_clock.mark_active(goal_id);
        Some(turn_id)
    }

    /// Start charging idle wall time to `goal_id` (no turn in flight).
    pub fn mark_idle_goal_active(&self, goal_id: impl Into<String>) {
        let goal_id = goal_id.into();
        let mut inner = self.inner();
        if inner.budget_limit_reported_goal_id.as_deref() != Some(goal_id.as_str()) {
            inner.budget_limit_reported_goal_id = None;
        }
        inner.wall_clock.mark_active(goal_id);
    }

    pub fn clear_current_turn_goal(&self) -> Option<String> {
        let mut inner = self.inner();
        let turn_id = inner.current_turn_id.clone()?;
        if let Some(turn) = inner.turns.get_mut(turn_id.as_str()) {
            turn.active_goal_id = None;
        }
        inner.wall_clock.clear_active();
        inner.budget_limit_reported_goal_id = None;
        Some(turn_id)
    }

    pub fn clear_active_goal(&self) {
        let mut inner = self.inner();
        if let Some(turn_id) = inner.current_turn_id.clone()
            && let Some(turn) = inner.turns.get_mut(turn_id.as_str())
        {
            turn.active_goal_id = None;
        }
        inner.wall_clock.clear_active();
        inner.budget_limit_reported_goal_id = None;
    }

    /// Everything spent on `turn_id` since the last charge, or `None` when there is nothing to
    /// charge (no goal attached, or an empty delta).
    pub fn progress_snapshot(&self, turn_id: &str) -> Option<ProgressSnapshot> {
        let inner = self.inner();
        let turn = inner.turns.get(turn_id)?;
        if !turn.account_tokens {
            return None;
        }
        let expected_goal_id = turn.active_goal_id.clone()?;
        let tokens = turn.token_delta();
        let cost_usd = turn.cost_delta();
        let iterations = turn.pending_iterations;
        let seconds =
            if inner.wall_clock.active_goal_id.as_deref() == Some(expected_goal_id.as_str()) {
                inner.wall_clock.elapsed_seconds()
            } else {
                0
            };
        let delta = UsageDelta {
            seconds,
            tokens,
            cost_usd,
            iterations,
        };
        if delta.seconds == 0 && delta.tokens <= 0 && delta.cost_usd <= 0.0 && delta.iterations == 0
        {
            return None;
        }
        Some(ProgressSnapshot {
            expected_goal_id,
            delta,
            current_usage: turn.current_usage,
            current_cost_usd: turn.current_cost_usd,
        })
    }

    /// Wall time accrued while idle with a goal attached.
    pub fn idle_progress_snapshot(&self) -> Option<IdleProgressSnapshot> {
        let inner = self.inner();
        let expected_goal_id = inner.wall_clock.active_goal_id.clone()?;
        let seconds = inner.wall_clock.elapsed_seconds();
        if seconds == 0 {
            return None;
        }
        Some(IdleProgressSnapshot {
            expected_goal_id,
            delta: UsageDelta {
                seconds,
                ..Default::default()
            },
        })
    }

    /// Advance the watermarks after a snapshot has been written to the store.
    pub fn mark_progress_accounted(
        &self,
        turn_id: &str,
        snapshot: &ProgressSnapshot,
        status: GoalStatus,
        disposition: BudgetDisposition,
    ) {
        let clear = should_clear_active_goal(status, disposition);
        let mut inner = self.inner();
        if let Some(turn) = inner.turns.get_mut(turn_id) {
            turn.last_accounted_usage = snapshot.current_usage;
            turn.last_accounted_cost_usd = snapshot.current_cost_usd;
            turn.pending_iterations = turn
                .pending_iterations
                .saturating_sub(snapshot.delta.iterations);
            if clear {
                turn.active_goal_id = None;
            }
        }
        inner.wall_clock.mark_accounted(snapshot.delta.seconds);
        if clear {
            inner.wall_clock.clear_active();
        }
        if status != GoalStatus::BudgetLimited {
            inner.budget_limit_reported_goal_id = None;
        }
    }

    pub fn mark_idle_progress_accounted(
        &self,
        snapshot: &IdleProgressSnapshot,
        status: GoalStatus,
        disposition: BudgetDisposition,
    ) {
        let mut inner = self.inner();
        inner.wall_clock.mark_accounted(snapshot.delta.seconds);
        if should_clear_active_goal(status, disposition) {
            inner.wall_clock.clear_active();
        }
        if status != GoalStatus::BudgetLimited {
            inner.budget_limit_reported_goal_id = None;
        }
    }

    pub fn reset_idle_baseline_and_clear_active_goal(&self) {
        let mut inner = self.inner();
        inner.wall_clock.clear_active();
        inner.budget_limit_reported_goal_id = None;
    }

    /// True the first time a given goal hits its budget, so the wrap-up prompt is injected once.
    pub fn mark_budget_limit_reported_if_new(&self, goal_id: &str) -> bool {
        let mut inner = self.inner();
        if inner.budget_limit_reported_goal_id.as_deref() == Some(goal_id) {
            return false;
        }
        inner.budget_limit_reported_goal_id = Some(goal_id.to_string());
        true
    }
}

fn should_clear_active_goal(status: GoalStatus, disposition: BudgetDisposition) -> bool {
    match status {
        GoalStatus::Active => false,
        GoalStatus::BudgetLimited => disposition == BudgetDisposition::ClearActive,
        GoalStatus::Paused
        | GoalStatus::Blocked
        | GoalStatus::UsageLimited
        | GoalStatus::Complete => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, cache_read: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read,
            cache_write: 0,
        }
    }

    #[test]
    fn token_delta_excludes_cached_input() {
        assert_eq!(goal_tokens(&usage(100, 20, 40)), 80);
        assert_eq!(
            usage_delta_tokens(&usage(100, 20, 40), &usage(180, 50, 60)),
            // fresh input 80-20=60, output 30
            90
        );
    }

    #[test]
    fn snapshot_charges_only_new_usage() {
        let state = GoalAccountingState::default();
        state.start_turn("t1", usage(10, 0, 0), 0.0);
        state.mark_turn_goal_active("t1", "g1");
        state.record_usage("t1", usage(110, 40, 10), 0.25);

        let snap = state.progress_snapshot("t1").expect("usage to charge");
        assert_eq!(snap.expected_goal_id, "g1");
        assert_eq!(snap.delta.tokens, 130);
        assert!((snap.delta.cost_usd - 0.25).abs() < 1e-9);

        state.mark_progress_accounted(
            "t1",
            &snap,
            GoalStatus::Active,
            BudgetDisposition::KeepActive,
        );
        assert!(
            state.progress_snapshot("t1").is_none(),
            "the same usage is never charged twice"
        );

        state.record_usage("t1", usage(110, 60, 10), 0.25);
        let snap = state.progress_snapshot("t1").unwrap();
        assert_eq!(snap.delta.tokens, 20);
    }

    #[test]
    fn iterations_are_charged_once() {
        let state = GoalAccountingState::default();
        state.start_turn("t1", Usage::default(), 0.0);
        state.mark_turn_goal_active("t1", "g1");
        state.record_iteration("t1");

        let snap = state.progress_snapshot("t1").unwrap();
        assert_eq!(snap.delta.iterations, 1);
        state.mark_progress_accounted(
            "t1",
            &snap,
            GoalStatus::Active,
            BudgetDisposition::KeepActive,
        );
        assert!(state.progress_snapshot("t1").is_none());
    }

    #[test]
    fn terminal_status_detaches_the_goal_from_the_turn() {
        let state = GoalAccountingState::default();
        state.start_turn("t1", Usage::default(), 0.0);
        state.mark_turn_goal_active("t1", "g1");
        state.record_usage("t1", usage(50, 0, 0), 0.0);
        let snap = state.progress_snapshot("t1").unwrap();

        state.mark_progress_accounted(
            "t1",
            &snap,
            GoalStatus::BudgetLimited,
            BudgetDisposition::KeepActive,
        );
        assert!(
            state.turn_is_current_active_goal("t1"),
            "a mid-turn budget stop keeps the goal attached for the wrap-up"
        );

        state.record_usage("t1", usage(80, 0, 0), 0.0);
        let snap = state.progress_snapshot("t1").unwrap();
        state.mark_progress_accounted(
            "t1",
            &snap,
            GoalStatus::BudgetLimited,
            BudgetDisposition::ClearActive,
        );
        assert!(!state.turn_is_current_active_goal("t1"));
    }

    #[test]
    fn budget_limit_is_reported_once_per_goal() {
        let state = GoalAccountingState::default();
        assert!(state.mark_budget_limit_reported_if_new("g1"));
        assert!(!state.mark_budget_limit_reported_if_new("g1"));
        assert!(state.mark_budget_limit_reported_if_new("g2"));
    }

    #[test]
    fn attaching_a_goal_mid_turn_rebases_the_watermark() {
        let state = GoalAccountingState::default();
        state.start_turn("t1", Usage::default(), 0.0);
        state.record_usage("t1", usage(500, 100, 0), 1.0);
        // Work done before the goal existed must not be charged to it.
        state.mark_current_turn_goal_active("g1");
        assert!(state.progress_snapshot("t1").is_none());

        state.record_usage("t1", usage(520, 100, 0), 1.0);
        assert_eq!(state.progress_snapshot("t1").unwrap().delta.tokens, 20);
    }
}
