//! Drives a goal through the turn life cycle.
//!
//! [`GoalRuntime`] is the single object the agent loop and the TUI talk to. It owns the store and
//! the accounting state, charges usage at the points a turn passes through, and produces the hidden
//! steering messages the loop injects. Nothing here starts a turn: `continue_if_idle` hands back a
//! prompt and the caller decides whether to run it, which keeps the automatic loop interruptible.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::core::goal::accounting::{BudgetDisposition, GoalAccountingState};
use crate::core::goal::prompts::{
    budget_limit_prompt, continuation_prompt, objective_updated_prompt,
};
use crate::core::goal::{
    Goal, GoalAccountingMode, GoalAccountingOutcome, GoalLimits, GoalStatus, GoalStore, GoalUpdate,
    validate_limits, validate_objective,
};
use crate::core::types::Usage;

/// Why an in-flight turn stopped a goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The turn ended on a non-retryable error. Block the goal so the automatic loop can't spin on
    /// the same failure and burn the budget.
    TurnError,
    /// The provider reported a usage/quota limit.
    UsageLimit,
}

/// Model prices used to convert token usage into dollars for the cost cap.
#[derive(Debug, Clone, Copy, Default)]
pub struct Pricing {
    pub input_per_mtok: Option<f64>,
    pub output_per_mtok: Option<f64>,
}

impl Pricing {
    pub fn cost_usd(&self, usage: &Usage) -> f64 {
        match (self.input_per_mtok, self.output_per_mtok) {
            (Some(i), Some(o)) => {
                (usage.input_tokens as f64) / 1e6 * i + (usage.output_tokens as f64) / 1e6 * o
            }
            _ => 0.0,
        }
    }
}

/// Goal state machine plus the steering queue the agent loop drains.
///
/// The store is swappable because the tool registry is built before a session file is chosen, and
/// because `/new`, resume and fork move the session under a running app.
pub struct GoalRuntime {
    store: std::sync::Mutex<Arc<GoalStore>>,
    accounting: Arc<GoalAccountingState>,
    enabled: AtomicBool,
    pricing: std::sync::Mutex<Pricing>,
    /// Hidden messages waiting to be injected before the next model request.
    pending_steering: std::sync::Mutex<Vec<String>>,
}

impl GoalRuntime {
    pub fn new(store: Arc<GoalStore>, enabled: bool) -> Self {
        GoalRuntime {
            store: std::sync::Mutex::new(store),
            accounting: Arc::new(GoalAccountingState::default()),
            enabled: AtomicBool::new(enabled),
            pricing: std::sync::Mutex::new(Pricing::default()),
            pending_steering: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// A disabled, file-less runtime (tests, `goal.enabled = false`).
    pub fn disabled() -> Self {
        GoalRuntime::new(Arc::new(GoalStore::ephemeral()), false)
    }

    pub fn store(&self) -> Arc<GoalStore> {
        self.store
            .lock()
            .map(|s| Arc::clone(&s))
            .unwrap_or_else(|e| Arc::clone(&e.into_inner()))
    }

    /// Point the runtime at a different session's goal file (new session, resume, fork).
    pub fn rebind(&self, store: Arc<GoalStore>) {
        if let Ok(mut slot) = self.store.lock() {
            *slot = store;
        }
        self.accounting.clear_active_goal();
        if let Ok(mut queue) = self.pending_steering.lock() {
            queue.clear();
        }
    }

    pub fn goal(&self) -> Option<Goal> {
        self.store().get()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn set_pricing(&self, pricing: Pricing) {
        if let Ok(mut slot) = self.pricing.lock() {
            *slot = pricing;
        }
    }

    fn pricing(&self) -> Pricing {
        self.pricing.lock().map(|p| *p).unwrap_or_default()
    }

    fn push_steering(&self, text: String) {
        if let Ok(mut queue) = self.pending_steering.lock() {
            queue.push(text);
        }
    }

    /// Hidden messages to inject before the next model request, if any.
    pub fn take_pending_steering(&self) -> Vec<String> {
        self.pending_steering
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default()
    }

    // -- turn life cycle ------------------------------------------------------------------------

    /// Begin a turn: clear a deferred continuation and attach the goal so its work is charged.
    pub fn on_turn_start(&self, turn_id: &str, usage_at_start: Usage) {
        if !self.is_enabled() {
            return;
        }
        self.store().clear_continuation_deferral();
        let cost = self.pricing().cost_usd(&usage_at_start);
        self.accounting.start_turn(turn_id, usage_at_start, cost);
        if let Some(goal) = self.store().get()
            && matches!(goal.status, GoalStatus::Active | GoalStatus::BudgetLimited)
        {
            self.accounting.mark_turn_goal_active(turn_id, goal.goal_id);
        }
    }

    /// Record the running usage totals for a turn.
    pub fn on_token_usage(&self, turn_id: &str, total_usage: Usage) {
        if !self.is_enabled() {
            return;
        }
        let cost = self.pricing().cost_usd(&total_usage);
        self.accounting.record_usage(turn_id, total_usage, cost);
    }

    /// Count one completed model round-trip toward the goal's turn cap.
    pub fn on_iteration(&self, turn_id: &str) {
        if self.is_enabled() {
            self.accounting.record_iteration(turn_id);
        }
    }

    /// Charge a finished tool call. When this is what pushes the goal over a cap, the wrap-up
    /// prompt is queued once so the model is told to land the plane before the turn ends.
    pub async fn on_tool_finish(&self, turn_id: &str, tool_name: &str) {
        // `update_goal` is bookkeeping about the goal, not progress on it.
        if !self.is_enabled() || tool_name == "update_goal" {
            return;
        }
        let Some(goal) = self
            .account_progress(
                turn_id,
                GoalAccountingMode::ActiveOnly,
                BudgetDisposition::KeepActive,
            )
            .await
        else {
            return;
        };
        if goal.status != GoalStatus::BudgetLimited {
            return;
        }
        if self
            .accounting
            .mark_budget_limit_reported_if_new(&goal.goal_id)
        {
            self.push_steering(budget_limit_prompt(&goal));
        }
    }

    /// End a turn normally: charge what is left and detach the goal.
    pub async fn on_turn_stop(&self, turn_id: &str) {
        if !self.is_enabled() {
            return;
        }
        self.account_progress(
            turn_id,
            GoalAccountingMode::ActiveOnly,
            BudgetDisposition::ClearActive,
        )
        .await;
        self.accounting.finish_turn(turn_id);
    }

    /// End an interrupted turn. Same accounting as a normal stop — the work still happened.
    pub async fn on_turn_abort(&self, turn_id: &str) {
        self.on_turn_stop(turn_id).await;
    }

    /// End a failed turn and stop the goal so the automatic loop doesn't retry forever.
    pub async fn on_turn_error(&self, turn_id: &str, reason: StopReason) {
        if !self.is_enabled() {
            return;
        }
        if !self.accounting.turn_is_current_active_goal(turn_id) {
            self.accounting.finish_turn(turn_id);
            return;
        }
        self.account_progress(
            turn_id,
            GoalAccountingMode::ActiveOnly,
            BudgetDisposition::ClearActive,
        )
        .await;

        let status = match reason {
            StopReason::TurnError => GoalStatus::Blocked,
            StopReason::UsageLimit => GoalStatus::UsageLimited,
        };
        if let Some(goal) = self.store().get() {
            let stoppable = goal.status == GoalStatus::Active
                || (goal.status == GoalStatus::BudgetLimited && status == GoalStatus::UsageLimited);
            if stoppable {
                self.store().update(GoalUpdate {
                    status: Some(status),
                    expected_goal_id: Some(goal.goal_id),
                    ..Default::default()
                });
            }
        }
        self.accounting.clear_active_goal();
        self.accounting.finish_turn(turn_id);
    }

    /// The hidden continuation prompt when the session is idle with an active goal, or `None` when
    /// nothing should run on its own.
    pub async fn continue_if_idle(&self) -> Option<String> {
        if !self.is_enabled() {
            self.accounting.clear_active_goal();
            return None;
        }
        // A goal restored from disk waits for the user before spending anything.
        if self.store().has_continuation_deferral() {
            return None;
        }
        let goal = self.store().get()?;
        if goal.status != GoalStatus::Active {
            self.accounting.clear_active_goal();
            return None;
        }
        self.accounting.mark_idle_goal_active(goal.goal_id.clone());
        Some(continuation_prompt(&goal))
    }

    /// Re-attach idle wall-clock accounting after a session is resumed.
    pub fn restore_after_resume(&self) {
        if !self.is_enabled() {
            return;
        }
        match self.store().get() {
            Some(goal) if goal.status == GoalStatus::Active => {
                self.accounting.mark_idle_goal_active(goal.goal_id);
            }
            _ => self.accounting.clear_active_goal(),
        }
    }

    // -- user-driven changes --------------------------------------------------------------------

    /// Start (or replace a completed) goal.
    pub async fn create_goal(&self, objective: &str, limits: GoalLimits) -> Result<Goal, String> {
        let objective = objective.trim();
        validate_objective(objective)?;
        validate_limits(&limits)?;
        self.flush_outstanding_progress().await;
        let goal = self.store()
            .insert(objective, GoalStatus::Active, limits)
            .ok_or_else(|| {
                "cannot create a new goal because this session has an unfinished goal; complete the existing goal first"
                    .to_string()
            })?;
        if self
            .accounting
            .mark_current_turn_goal_active(goal.goal_id.clone())
            .is_none()
        {
            self.accounting.mark_idle_goal_active(goal.goal_id.clone());
        }
        Ok(goal)
    }

    /// Replace the objective of the existing goal, steering a running turn onto it.
    pub async fn set_objective(&self, objective: &str) -> Result<Goal, String> {
        let objective = objective.trim();
        validate_objective(objective)?;
        let Some(existing) = self.store().get() else {
            return Err("no goal to edit".to_string());
        };
        self.flush_outstanding_progress().await;
        let changed = existing.objective != objective;
        let goal = self
            .store()
            .update(GoalUpdate {
                objective: Some(objective.to_string()),
                status: Some(GoalStatus::Active),
                expected_goal_id: Some(existing.goal_id),
                ..Default::default()
            })
            .ok_or_else(|| "no goal to edit".to_string())?;
        self.attach_after_external_change(&goal);
        if changed && self.accounting.current_turn_id().is_some() {
            self.push_steering(objective_updated_prompt(&goal));
        }
        Ok(goal)
    }

    /// Change the caps on the existing goal.
    pub async fn set_limits(&self, limits: GoalLimits) -> Result<Goal, String> {
        validate_limits(&limits)?;
        self.flush_outstanding_progress().await;
        let goal = self
            .store()
            .update(GoalUpdate {
                limits: Some(limits),
                ..Default::default()
            })
            .ok_or_else(|| "no goal to update".to_string())?;
        self.attach_after_external_change(&goal);
        Ok(goal)
    }

    /// Stop the automatic loop, keeping the goal for later.
    pub async fn pause(&self) -> Result<Goal, String> {
        self.flush_outstanding_progress().await;
        let goal = self
            .store()
            .pause_active()
            .ok_or_else(|| "no active goal to pause".to_string())?;
        self.accounting.clear_active_goal();
        Ok(goal)
    }

    /// Put a paused/blocked/limited goal back to work.
    ///
    /// A goal stopped by its own budget needs a bigger budget first, otherwise it would land right
    /// back on `budget_limited`.
    pub async fn resume(&self) -> Result<Goal, String> {
        let Some(existing) = self.store().get() else {
            return Err("no goal to resume".to_string());
        };
        if existing.status == GoalStatus::Complete {
            return Err("the goal is already complete".to_string());
        }
        if existing.status == GoalStatus::Active {
            self.attach_after_external_change(&existing);
            return Ok(existing);
        }
        self.flush_outstanding_progress().await;
        let goal = self
            .store()
            .update(GoalUpdate {
                status: Some(GoalStatus::Active),
                expected_goal_id: Some(existing.goal_id),
                ..Default::default()
            })
            .ok_or_else(|| "no goal to resume".to_string())?;
        if goal.status == GoalStatus::BudgetLimited {
            return Err(format!(
                "the goal is out of {} — raise the budget before resuming",
                goal.budget_cause().unwrap_or("budget")
            ));
        }
        self.attach_after_external_change(&goal);
        Ok(goal)
    }

    /// Drop the goal entirely.
    pub async fn clear(&self) -> Option<Goal> {
        self.flush_outstanding_progress().await;
        let previous = self.store().delete();
        self.accounting.clear_active_goal();
        previous
    }

    fn attach_after_external_change(&self, goal: &Goal) {
        match goal.status {
            GoalStatus::Active => {
                if self
                    .accounting
                    .mark_current_turn_goal_active(goal.goal_id.clone())
                    .is_none()
                {
                    self.accounting.mark_idle_goal_active(goal.goal_id.clone());
                }
            }
            _ => self.accounting.clear_active_goal(),
        }
    }

    // -- accounting plumbing --------------------------------------------------------------------

    /// Charge everything outstanding before the goal changes underneath the accounting state.
    async fn flush_outstanding_progress(&self) {
        if !self.is_enabled() {
            return;
        }
        match self.accounting.current_turn_id() {
            Some(turn_id) => {
                self.account_progress(
                    &turn_id,
                    GoalAccountingMode::ActiveOnly,
                    BudgetDisposition::ClearActive,
                )
                .await;
            }
            None => self.account_idle_progress().await,
        }
    }

    /// Snapshot → store write → watermark, all under the accounting permit so concurrent tool
    /// completions can't charge the same tokens twice.
    async fn account_progress(
        &self,
        turn_id: &str,
        mode: GoalAccountingMode,
        disposition: BudgetDisposition,
    ) -> Option<Goal> {
        let _permit = self.accounting.progress_permit().await?;
        let snapshot = self.accounting.progress_snapshot(turn_id)?;
        let outcome = self.store().account_usage(
            snapshot.delta,
            mode,
            Some(snapshot.expected_goal_id.as_str()),
        );
        match outcome {
            GoalAccountingOutcome::Updated(goal) => {
                self.accounting.mark_progress_accounted(
                    turn_id,
                    &snapshot,
                    goal.status,
                    disposition,
                );
                Some(goal)
            }
            GoalAccountingOutcome::Unchanged(_) => None,
        }
    }

    async fn account_idle_progress(&self) {
        let Some(_permit) = self.accounting.progress_permit().await else {
            return;
        };
        let Some(snapshot) = self.accounting.idle_progress_snapshot() else {
            return;
        };
        let outcome = self.store().account_usage(
            snapshot.delta,
            GoalAccountingMode::ActiveOnly,
            Some(snapshot.expected_goal_id.as_str()),
        );
        match outcome {
            GoalAccountingOutcome::Updated(goal) => self.accounting.mark_idle_progress_accounted(
                &snapshot,
                goal.status,
                BudgetDisposition::ClearActive,
            ),
            GoalAccountingOutcome::Unchanged(_) => {
                self.accounting.reset_idle_baseline_and_clear_active_goal();
            }
        }
    }

    /// Charge usage from the model's own `update_goal` call, which must be able to bill the goal it
    /// is about to finish.
    pub async fn account_for_goal_tool(&self, status: GoalStatus) {
        let Some(turn_id) = self.accounting.current_turn_id() else {
            return;
        };
        let mode = match status {
            GoalStatus::Complete => GoalAccountingMode::ActiveOrComplete,
            _ => GoalAccountingMode::ActiveOrStopped,
        };
        self.account_progress(&turn_id, mode, BudgetDisposition::ClearActive)
            .await;
    }

    /// Detach the goal from the live turn after the model finished or blocked it.
    pub fn clear_current_turn_goal(&self) {
        self.accounting.clear_current_turn_goal();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> GoalRuntime {
        GoalRuntime::new(Arc::new(GoalStore::ephemeral()), true)
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn tool_usage_is_charged_to_the_goal() {
        let rt = runtime();
        rt.create_goal("ship it", GoalLimits::tokens(Some(1_000)))
            .await
            .unwrap();

        rt.on_turn_start("t1", Usage::default());
        rt.on_token_usage("t1", usage(300, 50));
        rt.on_tool_finish("t1", "read").await;

        let goal = rt.goal().unwrap();
        assert_eq!(goal.tokens_used, 350);
        assert_eq!(goal.status, GoalStatus::Active);
        assert!(rt.take_pending_steering().is_empty());
    }

    #[tokio::test]
    async fn crossing_the_budget_queues_the_wrap_up_prompt_once() {
        let rt = runtime();
        rt.create_goal("ship it", GoalLimits::tokens(Some(100)))
            .await
            .unwrap();
        rt.on_turn_start("t1", Usage::default());

        rt.on_token_usage("t1", usage(200, 0));
        rt.on_tool_finish("t1", "read").await;
        let steering = rt.take_pending_steering();
        assert_eq!(steering.len(), 1);
        assert!(steering[0].contains("do not start new substantive work"));
        assert_eq!(rt.goal().unwrap().status, GoalStatus::BudgetLimited);

        rt.on_token_usage("t1", usage(400, 0));
        rt.on_tool_finish("t1", "read").await;
        assert!(
            rt.take_pending_steering().is_empty(),
            "the wrap-up prompt is not repeated for the same goal"
        );
    }

    #[tokio::test]
    async fn update_goal_calls_do_not_count_as_progress() {
        let rt = runtime();
        rt.create_goal("ship it", GoalLimits::default())
            .await
            .unwrap();
        rt.on_turn_start("t1", Usage::default());
        rt.on_token_usage("t1", usage(500, 0));
        rt.on_tool_finish("t1", "update_goal").await;
        assert_eq!(rt.goal().unwrap().tokens_used, 0);
    }

    #[tokio::test]
    async fn idle_continuation_only_fires_for_an_active_goal() {
        let rt = runtime();
        assert!(rt.continue_if_idle().await.is_none(), "no goal, no loop");

        rt.create_goal("ship it", GoalLimits::default())
            .await
            .unwrap();
        let prompt = rt.continue_if_idle().await.expect("active goal continues");
        assert!(prompt.contains("Continue working toward the active session goal."));

        rt.pause().await.unwrap();
        assert!(rt.continue_if_idle().await.is_none());

        rt.resume().await.unwrap();
        assert!(rt.continue_if_idle().await.is_some());
    }

    #[tokio::test]
    async fn a_restored_goal_waits_for_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.goal.json");
        {
            let rt = GoalRuntime::new(Arc::new(GoalStore::open(&path)), true);
            rt.create_goal("ship it", GoalLimits::default())
                .await
                .unwrap();
        }
        let resumed = GoalRuntime::new(Arc::new(GoalStore::open(&path)), true);
        resumed.restore_after_resume();
        assert!(
            resumed.continue_if_idle().await.is_none(),
            "resuming a session must not auto-start a turn"
        );

        resumed.on_turn_start("t1", Usage::default());
        assert!(
            resumed.continue_if_idle().await.is_some(),
            "the deferral clears once the user runs a turn"
        );
    }

    #[tokio::test]
    async fn a_turn_error_blocks_the_goal_and_stops_the_loop() {
        let rt = runtime();
        rt.create_goal("ship it", GoalLimits::default())
            .await
            .unwrap();
        rt.on_turn_start("t1", Usage::default());
        rt.on_turn_error("t1", StopReason::TurnError).await;

        assert_eq!(rt.goal().unwrap().status, GoalStatus::Blocked);
        assert!(rt.continue_if_idle().await.is_none());
    }

    #[tokio::test]
    async fn a_usage_limit_marks_the_goal_usage_limited() {
        let rt = runtime();
        rt.create_goal("ship it", GoalLimits::default())
            .await
            .unwrap();
        rt.on_turn_start("t1", Usage::default());
        rt.on_turn_error("t1", StopReason::UsageLimit).await;
        assert_eq!(rt.goal().unwrap().status, GoalStatus::UsageLimited);
    }

    #[tokio::test]
    async fn editing_the_objective_steers_a_running_turn() {
        let rt = runtime();
        rt.create_goal("ship it", GoalLimits::default())
            .await
            .unwrap();
        rt.on_turn_start("t1", Usage::default());
        rt.set_objective("ship it, but with tests").await.unwrap();

        let steering = rt.take_pending_steering();
        assert_eq!(steering.len(), 1);
        assert!(steering[0].contains("ship it, but with tests"));
        assert!(steering[0].contains("supersedes any previous session goal objective"));
    }

    #[tokio::test]
    async fn a_second_goal_needs_the_first_one_finished() {
        let rt = runtime();
        rt.create_goal("first", GoalLimits::default())
            .await
            .unwrap();
        let err = rt
            .create_goal("second", GoalLimits::default())
            .await
            .unwrap_err();
        assert!(err.contains("unfinished goal"));
    }

    #[tokio::test]
    async fn a_budget_limited_goal_cannot_resume_without_a_bigger_budget() {
        let rt = runtime();
        rt.create_goal("ship it", GoalLimits::tokens(Some(100)))
            .await
            .unwrap();
        rt.on_turn_start("t1", Usage::default());
        rt.on_token_usage("t1", usage(500, 0));
        rt.on_tool_finish("t1", "read").await;
        rt.on_turn_stop("t1").await;

        assert!(rt.resume().await.is_err());
        rt.set_limits(GoalLimits::tokens(Some(10_000)))
            .await
            .unwrap();
        let resumed = rt.resume().await.unwrap();
        assert_eq!(resumed.status, GoalStatus::Active);
    }

    #[tokio::test]
    async fn turn_caps_stop_the_loop() {
        let rt = runtime();
        rt.create_goal(
            "ship it",
            GoalLimits {
                max_iterations: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        for turn in ["t1", "t2"] {
            rt.on_turn_start(turn, Usage::default());
            rt.on_iteration(turn);
            rt.on_turn_stop(turn).await;
        }
        let goal = rt.goal().unwrap();
        assert_eq!(goal.iterations_used, 2);
        assert_eq!(goal.status, GoalStatus::BudgetLimited);
        assert!(rt.continue_if_idle().await.is_none());
    }
}
