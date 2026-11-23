//! `GoalCreate` / `GoalGet` / `GoalUpdate` — the model's handle on the
//! session goal (`base::context::goal`). Which operations a call may perform
//! depends on who started the turn it runs in; sub-agents have none.

use async_trait::async_trait;
use base::context::goal::{GoalAction, GoalError, TurnOrigin};
use base::error::ToolError;
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::tool::{PathStyle, ProgressSender, Tool, ToolContext, ToolResult, ToolSpec};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalCreateInput {
    /// What the session should achieve, stated so that its completion can be
    /// checked.
    pub objective: String,
    /// How many automatic rounds the goal may use (default 256, or the most
    /// this deployment allows if that is fewer).
    #[serde(default)]
    pub max_rounds: Option<u32>,
    /// The most tokens the goal's rounds may spend, sub-agents included; it
    /// is blocked once they are spent. Leave it out for no limit.
    #[serde(default)]
    pub token_budget: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalGetInput {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoalUpdateAction {
    Edit,
    Pause,
    Resume,
    Complete,
    Blocked,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalUpdateInput {
    pub goal_id: String,
    /// The revision GoalGet returned; a stale one is refused.
    pub revision: u64,
    /// edit, pause and resume need a direct request from the user; complete
    /// and blocked may also come from a round of this goal.
    pub action: GoalUpdateAction,
    /// With `edit`: the new objective.
    #[serde(default)]
    pub objective: Option<String>,
    /// With `edit`: the new round limit.
    #[serde(default)]
    pub max_rounds: Option<u32>,
    /// With `edit`: the new token budget.
    #[serde(default)]
    pub token_budget: Option<u64>,
    /// With `blocked`: what is in the way.
    #[serde(default)]
    pub blocked_reason: Option<String>,
}

/// Who may act on the goal from this call: nobody below the root session.
fn origin(ctx: &ToolContext) -> Result<TurnOrigin, Box<ToolResult>> {
    if ctx.in_delegate() {
        return Err(Box::new(ToolResult::error_text(
            "goals belong to the top-level session; a sub-agent cannot read or change them",
        )));
    }
    Ok(ctx.turn.origin)
}

fn refused(e: GoalError) -> ToolResult {
    ToolResult::error_text(e.to_string())
}

/// `asked`, held to the most rounds this deployment lets the model give a
/// goal, and a note saying so when it was lowered.
fn rounds_allowed(asked: Option<u32>, ctx: &ToolContext) -> (Option<u32>, u32, String) {
    let limit = ctx.config.model_autonomy.goal_max_rounds;
    match asked {
        Some(n) if n > limit => (
            Some(limit),
            limit,
            format!("max_rounds was lowered to {limit}, the most this deployment allows.\n"),
        ),
        asked => (asked, limit, String::new()),
    }
}

/// The model may tighten a goal's token budget, not raise it: whoever set
/// the budget, a larger one is the host's to give (`session.updateGoal`).
fn raised_budget(asked: Option<u64>, ctx: &ToolContext) -> Option<ToolResult> {
    let asked = asked?;
    let current = ctx.session.goal_view()?.goal.token_budget?;
    (asked > current).then(|| {
        ToolResult::error_text(format!(
            "token_budget can only be lowered here: this goal's is {current}. A larger budget is \
             for the user to give."
        ))
    })
}

fn view_result(view: base::context::goal::GoalView, note: &str) -> ToolResult {
    let view = serde_json::to_value(view).unwrap_or_default();
    let mut result = ToolResult::text(format!(
        "{note}{}",
        serde_json::to_string_pretty(&view).unwrap_or_default()
    ));
    result.structured_content = Some(view);
    result
}

pub struct GoalCreateTool;

#[async_trait]
impl Tool for GoalCreateTool {
    fn name(&self) -> &str {
        "GoalCreate"
    }
    fn description(&self) -> &str {
        "Set a goal that keeps this session working across automatic rounds until it is \
         complete. Use it when the user's request is a long-running objective that one turn \
         won't finish, even if they don't say \"goal\"; not for single-turn work."
    }
    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<GoalCreateInput>()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
    }

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::SessionOnly)
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: GoalCreateInput = serde_json::from_value(input)?;
        let origin = match origin(&ctx) {
            Ok(o) => o,
            Err(r) => return Ok(*r),
        };
        let (max_rounds, max_rounds_limit, lowered) = rounds_allowed(input.max_rounds, &ctx);
        let limits = base::context::goal::GoalLimits {
            max_rounds,
            token_budget: input.token_budget,
            max_rounds_limit,
        };
        Ok(
            match ctx.session.goal_create(input.objective, limits, origin) {
                Ok(view) => view_result(
                    view,
                    &format!(
                        "Goal set. Work on it now; when this turn ends, automatic rounds \
                         continue until you mark it complete.\n{lowered}"
                    ),
                ),
                Err(e) => refused(e),
            },
        )
    }
}

pub struct GoalGetTool;

#[async_trait]
impl Tool for GoalGetTool {
    fn name(&self) -> &str {
        "GoalGet"
    }
    fn description(&self) -> &str {
        "Read this session's goal, including the id and revision GoalUpdate needs."
    }
    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<GoalGetInput>()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().read_only(true).concurrency_safe(true)
    }

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::SessionOnly)
    }
    async fn call(
        &self,
        _: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        if let Err(r) = origin(&ctx) {
            return Ok(*r);
        }
        Ok(match ctx.session.goal_view() {
            None => ToolResult::text("This session has no goal."),
            Some(view) => view_result(view, ""),
        })
    }
}

pub struct GoalUpdateTool;

#[async_trait]
impl Tool for GoalUpdateTool {
    fn name(&self) -> &str {
        "GoalUpdate"
    }
    fn description(&self) -> &str {
        "Change this session's goal: edit, pause, resume, complete or blocked. Mark complete \
         only when the whole objective is achieved; difficulty or remaining work is not \
         blocked."
    }
    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<GoalUpdateInput>()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
    }

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::SessionOnly)
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: GoalUpdateInput = serde_json::from_value(input)?;
        let origin = match origin(&ctx) {
            Ok(o) => o,
            Err(r) => return Ok(*r),
        };
        if let Some(refusal) = raised_budget(input.token_budget, &ctx) {
            return Ok(refusal);
        }
        let (max_rounds, max_rounds_limit, lowered) = rounds_allowed(input.max_rounds, &ctx);
        let edited = format!("Goal updated.\n{lowered}");
        let (action, note) = match input.action {
            GoalUpdateAction::Edit => (
                GoalAction::Edit {
                    objective: input.objective,
                    max_rounds,
                    token_budget: input.token_budget,
                },
                edited.as_str(),
            ),
            GoalUpdateAction::Pause => (GoalAction::Pause, "Goal paused.\n"),
            GoalUpdateAction::Resume => (
                GoalAction::Resume,
                "Goal resumed; automatic rounds continue after this turn.\n",
            ),
            GoalUpdateAction::Complete => (
                GoalAction::Complete,
                "Goal complete. Write your final message to the user now — what was achieved \
                 and the evidence — and call no more tools.\n",
            ),
            GoalUpdateAction::Blocked => {
                let Some(reason) = input.blocked_reason else {
                    return Ok(ToolResult::error_text(
                        "blocked needs a blocked_reason saying what is in the way",
                    ));
                };
                (
                    GoalAction::Blocked { reason },
                    "Goal blocked. Tell the user what is in the way and what would unblock it, \
                     and call no more tools.\n",
                )
            }
        };
        Ok(
            match ctx.session.goal_update(
                &input.goal_id,
                input.revision,
                action,
                origin,
                max_rounds_limit,
            ) {
                Ok(view) => view_result(view, note),
                Err(e) => refused(e),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    async fn call(tool: &dyn Tool, input: Value, ctx: &ToolContext) -> ToolResult {
        tool.call(input, ctx.clone(), ProgressSender::noop("t"))
            .await
            .unwrap()
    }

    fn goal_json(ctx: &ToolContext) -> Value {
        serde_json::to_value(ctx.session.goal_view()).unwrap()
    }

    fn text(r: &ToolResult) -> String {
        match &r.content {
            base::tool::ToolResultContent::Text(s) => s.clone(),
            other => format!("{other:?}"),
        }
    }

    fn context_over(state: &Arc<session_state::SessionState>) -> ToolContext {
        let mut ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            "/tmp".into(),
        );
        ctx.session = state.clone();
        ctx
    }

    #[tokio::test]
    async fn the_model_lowers_a_goals_token_budget_but_does_not_raise_it() {
        let state = Arc::new(session_state::SessionState::new());
        let ctx = context_over(&state);
        let created = call(
            &GoalCreateTool,
            json!({"objective": "tests green", "token_budget": 50000}),
            &ctx,
        )
        .await;
        assert!(!created.is_error, "{}", text(&created));
        let edit = |budget: u64| {
            let view = goal_json(&ctx);
            json!({"goal_id": view["id"], "revision": view["revision"],
                   "action": "edit", "token_budget": budget})
        };
        let raised = call(&GoalUpdateTool, edit(90000), &ctx).await;
        assert!(raised.is_error, "{}", text(&raised));
        assert_eq!(goal_json(&ctx)["token_budget"], 50000);
        let lowered = call(&GoalUpdateTool, edit(20000), &ctx).await;
        assert!(!lowered.is_error, "{}", text(&lowered));
        assert_eq!(goal_json(&ctx)["token_budget"], 20000);
    }

    #[tokio::test]
    async fn create_get_and_complete() {
        let state = Arc::new(session_state::SessionState::new());
        let ctx = context_over(&state);
        let created = call(&GoalCreateTool, json!({"objective": "tests green"}), &ctx).await;
        assert!(!created.is_error, "{}", text(&created));
        let goal = call(&GoalGetTool, json!({}), &ctx).await;
        let view = goal.structured_content.unwrap();
        assert_eq!(view["phase"], "active");
        assert_eq!(view["armed"], true);
        assert_eq!(view["max_rounds"], 256);

        let done = call(
            &GoalUpdateTool,
            json!({"goal_id": view["id"], "revision": view["revision"], "action": "complete"}),
            &ctx,
        )
        .await;
        assert!(!done.is_error, "{}", text(&done));
        assert!(text(&done).contains("call no more tools"));
        assert_eq!(goal_json(&ctx)["phase"], "complete");
        assert_eq!(state.unrecorded_goal_changes().await.changes.len(), 2);
    }

    #[tokio::test]
    async fn a_sub_agent_has_no_goal() {
        let mut ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            "/tmp".into(),
        );
        ctx.agent_depth = 1;
        let r = call(&GoalCreateTool, json!({"objective": "x"}), &ctx).await;
        assert!(r.is_error);
        assert!(ctx.session.goal_view().is_none());
    }

    #[tokio::test]
    async fn an_automatic_round_cannot_pause() {
        let state = Arc::new(session_state::SessionState::new());
        let mut ctx = context_over(&state);
        call(&GoalCreateTool, json!({"objective": "x"}), &ctx).await;
        let round = state.goal_next_round().unwrap();
        ctx.turn = std::sync::Arc::new(base::tool::TurnScope::new(TurnOrigin::from_turn_id(
            &round.turn_id,
        )));
        let view = goal_json(&ctx);
        let r = call(
            &GoalUpdateTool,
            json!({"goal_id": view["id"], "revision": view["revision"], "action": "pause"}),
            &ctx,
        )
        .await;
        assert!(r.is_error);
        assert!(text(&r).contains("only a direct request from the user"));
    }

    #[tokio::test]
    async fn blocked_needs_a_reason() {
        let ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            "/tmp".into(),
        );
        call(&GoalCreateTool, json!({"objective": "x"}), &ctx).await;
        let view = goal_json(&ctx);
        let r = call(
            &GoalUpdateTool,
            json!({"goal_id": view["id"], "revision": view["revision"], "action": "blocked"}),
            &ctx,
        )
        .await;
        assert!(r.is_error);
    }
}
