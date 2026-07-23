//! The model's view of the session goal: `get_goal`, `create_goal`, `update_goal`.
//!
//! These are deliberately narrow. The model may start a goal when asked to and may declare one
//! complete or genuinely blocked — everything else (pausing, resuming, raising a budget, clearing)
//! belongs to the user, because those are the levers that decide how much unattended work happens.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::core::goal::runtime::GoalRuntime;
use crate::core::goal::{Goal, GoalLimits, GoalStatus, GoalUpdate};
use crate::core::types::ToolOutput;
use crate::tools::{Tool, ToolCtx};

/// Serialize a goal the way the model sees it (camelCase, budget figures included).
fn goal_json(goal: &Goal) -> Value {
    json!({
        "objective": goal.objective,
        "status": goal.status.as_str(),
        "tokenBudget": goal.limits.token_budget,
        "tokensUsed": goal.tokens_used,
        "timeUsedSeconds": goal.time_used_seconds,
        "createdAt": goal.created_at,
        "updatedAt": goal.updated_at,
    })
}

fn response(goal: Option<&Goal>, completion_report: bool) -> ToolOutput {
    let body = json!({
        "goal": goal.map(goal_json),
        "remainingTokens": goal.and_then(|g| g.remaining_tokens()),
        "completionBudgetReport": completion_report
            .then(|| completion_budget_report(goal))
            .flatten(),
    });
    ToolOutput::ok(body.to_string())
}

/// Nudge the model to report the final spend when a budgeted goal lands.
fn completion_budget_report(goal: Option<&Goal>) -> Option<String> {
    let goal = goal?;
    if goal.status != GoalStatus::Complete {
        return None;
    }
    if goal.limits.is_unbounded() && goal.time_used_seconds <= 0 {
        return None;
    }
    Some(
        "Goal achieved. Report final usage from this tool result's structured goal fields. If \
         `goal.tokenBudget` is present, include token usage from `goal.tokensUsed` and \
         `goal.tokenBudget`. If `goal.timeUsedSeconds` is greater than 0, summarize elapsed time \
         in a concise, human-friendly form appropriate to the response language."
            .to_string(),
    )
}

/// Read the current goal.
pub struct GetGoal {
    runtime: Arc<GoalRuntime>,
}

impl GetGoal {
    pub fn new(runtime: Arc<GoalRuntime>) -> Self {
        GetGoal { runtime }
    }
}

#[async_trait]
impl Tool for GetGoal {
    fn name(&self) -> &str {
        "get_goal"
    }

    fn description(&self) -> &str {
        "Get the current goal for this session, including status, budgets, token and elapsed-time \
         usage, and remaining token budget."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn run(&self, _input: Value, _ctx: &ToolCtx) -> ToolOutput {
        response(
            self.runtime.goal().as_ref(),
            /*completion_report*/ false,
        )
    }
}

/// Start a new goal.
pub struct CreateGoal {
    runtime: Arc<GoalRuntime>,
}

impl CreateGoal {
    pub fn new(runtime: Arc<GoalRuntime>) -> Self {
        CreateGoal { runtime }
    }
}

#[async_trait]
impl Tool for CreateGoal {
    fn name(&self) -> &str {
        "create_goal"
    }

    fn description(&self) -> &str {
        "Create a goal only when explicitly requested by the user or system/developer \
         instructions; do not infer goals from ordinary tasks.\nSet token_budget only when an \
         explicit token budget is requested. Fails if an unfinished goal exists; use update_goal \
         only for status."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "Required. The concrete objective to start pursuing. This starts a new active goal when no goal exists or replaces the current goal when it is complete."
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Positive token budget for the new goal. Omit unless explicitly requested."
                }
            },
            "required": ["objective"],
            "additionalProperties": false
        })
    }

    async fn run(&self, input: Value, _ctx: &ToolCtx) -> ToolOutput {
        let Some(objective) = input["objective"].as_str() else {
            return ToolOutput::error("create_goal: requires `objective`");
        };
        let limits = GoalLimits::tokens(input["token_budget"].as_i64());
        match self.runtime.create_goal(objective, limits).await {
            Ok(goal) => response(Some(&goal), /*completion_report*/ false),
            Err(e) => ToolOutput::error(e),
        }
    }
}

/// Mark the goal complete or blocked.
pub struct UpdateGoal {
    runtime: Arc<GoalRuntime>,
}

impl UpdateGoal {
    pub fn new(runtime: Arc<GoalRuntime>) -> Self {
        UpdateGoal { runtime }
    }
}

#[async_trait]
impl Tool for UpdateGoal {
    fn name(&self) -> &str {
        "update_goal"
    }

    fn description(&self) -> &str {
        "Update the existing goal.\n\
         Use this tool only to mark the goal achieved or genuinely blocked.\n\
         Set status to `complete` only when the objective has actually been achieved and no \
         required work remains.\n\
         Set status to `blocked` only when the same blocking condition has repeated for at least \
         three consecutive goal turns, counting the original/user-triggered turn and any automatic \
         continuations, and the agent cannot make meaningful progress without user input or an \
         external-state change.\n\
         If the user resumes a goal that was previously marked `blocked`, treat the resumed run as \
         a fresh blocked audit. If the same blocking condition then repeats for at least three \
         consecutive resumed goal turns, set status to `blocked` again.\n\
         Once the blocked threshold is satisfied, do not keep reporting that you are still blocked \
         while leaving the goal active; set status to `blocked`.\n\
         Do not use `blocked` merely because the work is hard, slow, uncertain, incomplete, or \
         would benefit from clarification.\n\
         Do not mark a goal complete merely because its budget is nearly exhausted or because you \
         are stopping work.\n\
         You cannot use this tool to pause, resume, budget-limit, or usage-limit a goal; those \
         status changes are controlled by the user or system.\n\
         When marking a budgeted goal achieved with status `complete`, report the final token \
         usage from the tool result to the user."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["complete", "blocked"],
                    "description": "Required. Set to `complete` only when the objective is achieved and no required work remains. Set to `blocked` only after the same blocking condition has recurred for at least three consecutive goal turns and the agent is at an impasse. After a previously blocked goal is resumed, the resumed run starts a fresh blocked audit."
                }
            },
            "required": ["status"],
            "additionalProperties": false
        })
    }

    async fn run(&self, input: Value, _ctx: &ToolCtx) -> ToolOutput {
        let status = match input["status"].as_str() {
            Some("complete") => GoalStatus::Complete,
            Some("blocked") => GoalStatus::Blocked,
            _ => {
                return ToolOutput::error(
                    "update_goal can only mark the existing goal complete or blocked; pause, \
                     resume, budget-limited, and usage-limited status changes are controlled by \
                     the user or system",
                );
            }
        };

        // Charge the work done so far before the goal leaves the accounting-eligible statuses.
        self.runtime.account_for_goal_tool(status).await;
        let Some(goal) = self.runtime.store().update(GoalUpdate {
            status: Some(status),
            ..Default::default()
        }) else {
            return ToolOutput::error("cannot update goal because this session has no goal");
        };
        self.runtime.clear_current_turn_goal();
        response(Some(&goal), status == GoalStatus::Complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::goal::GoalStore;
    use crate::core::types::Usage;

    fn runtime() -> Arc<GoalRuntime> {
        Arc::new(GoalRuntime::new(Arc::new(GoalStore::ephemeral()), true))
    }

    fn body(out: &ToolOutput) -> Value {
        serde_json::from_str(&out.text).expect("tool output is JSON")
    }

    #[tokio::test]
    async fn create_then_get_then_complete() {
        let rt = runtime();
        let ctx = ToolCtx::new(".");

        let out = CreateGoal::new(rt.clone())
            .run(json!({ "objective": "ship it", "token_budget": 500 }), &ctx)
            .await;
        assert!(!out.is_error);
        assert_eq!(body(&out)["goal"]["status"], "active");
        assert_eq!(body(&out)["remainingTokens"], 500);

        let out = GetGoal::new(rt.clone()).run(json!({}), &ctx).await;
        assert_eq!(body(&out)["goal"]["objective"], "ship it");

        rt.on_turn_start("t1", Usage::default());
        let out = UpdateGoal::new(rt.clone())
            .run(json!({ "status": "complete" }), &ctx)
            .await;
        assert_eq!(body(&out)["goal"]["status"], "complete");
        assert!(
            body(&out)["completionBudgetReport"].is_string(),
            "a budgeted goal reports its final spend"
        );
    }

    #[tokio::test]
    async fn update_goal_rejects_statuses_the_user_owns() {
        let rt = runtime();
        let ctx = ToolCtx::new(".");
        CreateGoal::new(rt.clone())
            .run(json!({ "objective": "ship it" }), &ctx)
            .await;

        let out = UpdateGoal::new(rt.clone())
            .run(json!({ "status": "paused" }), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.text.contains("controlled by the user or system"));
        assert_eq!(rt.goal().unwrap().status, GoalStatus::Active);
    }

    #[tokio::test]
    async fn create_goal_rejects_a_non_positive_budget() {
        let rt = runtime();
        let out = CreateGoal::new(rt)
            .run(
                json!({ "objective": "ship it", "token_budget": 0 }),
                &ToolCtx::new("."),
            )
            .await;
        assert!(out.is_error);
        assert!(out.text.contains("must be positive"));
    }

    #[tokio::test]
    async fn get_goal_with_no_goal_returns_null() {
        let out = GetGoal::new(runtime())
            .run(json!({}), &ToolCtx::new("."))
            .await;
        assert!(body(&out)["goal"].is_null());
    }
}
