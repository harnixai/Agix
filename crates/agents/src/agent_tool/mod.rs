//! `AgentTool` — spawn sub-agents using the agent's own `Agent` engine.
//!
//! Implements `base::tool::Tool` for the `Agent` invocable by the model.
//! Each sub-agent is a session of its own built with `Builder::build()`; it
//! inherits the parent's model client but gets a restricted tool set.
//!
//! # Agent type registry
//!
//! The module defines built-in agent types (`builtin_agent_types()`) and can
//! load user-defined types from a directory (e.g. `~/.agix/scenes/<scope>/agents/*.md`
//! or `<project>/<project dir>/agents/*.md`) via `load_agent_types_from_dir()`. Each
//! type specifies a system prompt and an allowed tool set, which
//! `delegate_selector()` holds a sub-agent's session to.

#[cfg(test)]
use ::settings::Settings;
use ::settings::{LoadedSettings, SessionConfig};
use anyhow::anyhow;
use async_trait::async_trait;
#[cfg(test)]
use base::context::EngineConfig;
use base::event::AgentEvent;
use base::interface::model::Effort;
use base::interface::model::Model;
#[cfg(test)]
use base::interface::model::ThinkingMode;
use base::interface::permission::{PermissionFacts, PermissionSet, Reach, Subject};
use base::interface::scene::SceneSpec;
#[cfg(test)]
use base::paths::PathSettings;
use base::paths::PromptFiles;
use base::tool::ProgressSender;
use base::tool::ToolContext;
use base::tool::{PathStyle, ToolSpec};
use history::store::HistoryStore;
use runtime::agent::{Agent, Builder, EventReceiver, InputSender};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use telemetry::TelemetryHandle;
use tools::worktree::create_worktree;

pub use base::agent_type::{parse_permission_mode, AgentTypeDefinition, AgentTypeSource};

mod build;
mod catalog;
mod permission;
mod resolve;
mod settings;
mod spawn_meta;
mod task;
#[cfg(test)]
mod tests;
mod turn;

use self::settings::*;
use crate::delegate::{Admission, Delegator, HookAgentRunning, AGENT_TOOL_NAME};
pub use crate::delegate::{SubagentSlots, DEFAULT_MAX_CONCURRENT_SUBAGENTS};
use base::interface::delegate::Delegator as _;
use base::interface::delegate::{
    AskRoute, CallSite, DelegateEnd, DelegateError, DelegateMode, DelegateRequest, Delegated,
    MemberLifetime, StopsWith, TeamSeat, Workdir,
};
use base::interface::tool_selector::{ToolPattern, ToolSelector};
pub(crate) use build::*;
pub use catalog::*;
pub(crate) use permission::*;
pub(crate) use spawn_meta::*;
pub(crate) use task::*;
pub(crate) use turn::*;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentInput {
    pub prompt: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, alias = "subagentType")]
    pub subagent_type: Option<String>,
    /// Run the sub-agent in a git worktree of its own named `<name>`, on a
    /// branch of its own (the tool's guide names both). Removed when the
    /// agent ends if nothing was left in it; otherwise kept, and the result
    /// names its path and branch.
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default, alias = "run_in_background", alias = "runInBackground")]
    pub background: bool,
    #[serde(default)]
    pub model: Option<String>,
    /// The provider the sub-agent's calls go to first, over its type's; the
    /// rest follow `model.order`.
    #[serde(default)]
    pub provider: Option<String>,
    /// A JSON Schema the sub-agent's final answer must satisfy; the answer is
    /// then also returned as structured content.
    #[serde(default)]
    pub output_schema: Option<Value>,
    /// A team to join, together with `name`. If that member is alive, `prompt`
    /// is queued as its next message instead of spawning a new one. Needs
    /// team mode enabled for the session.
    // The schema cannot hide this per session, so `AgentTool::call` rejects it
    // when team mode is off.
    #[serde(default)]
    pub team_name: Option<String>,
    /// This member's name within `team_name`. Reuse the same pair to keep
    /// messaging the same member.
    #[serde(default)]
    pub name: Option<String>,
    /// With `team_name` + `name`: the permission mode a new member runs under
    /// for its whole lifetime (default `plan`, read-only). The first member
    /// spawned under a team fixes it for the team; messaging a live member
    /// ignores it. `bubble` is rejected.
    // Fixed at spawn because a persistent member's lead is not around to
    // answer per-call prompts (see `AgentTool::build_team_member_agent`), and
    // `bubble` needs that lead.
    #[serde(default)]
    pub permission_mode: Option<base::permission::PermissionMode>,
}

pub struct AgentTool {
    delegator: Arc<Delegator>,
}

impl AgentTool {
    pub(crate) fn new(delegator: Arc<Delegator>) -> Self {
        Self { delegator }
    }

    async fn message_member(
        &self,
        input: AgentInput,
        mut req: DelegateRequest,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        let (Some(team), Some(member)) = (input.team_name, input.name) else {
            return Err(base::error::ToolError::Validation(
                "team_name and name must both be given together, or neither".into(),
            ));
        };
        // The schema offers `team_name`/`name` whether or not this session
        // has team coordination on; with it off they are refused here, as
        // `TeamCreate` is by not being registered.
        if self.delegator.members.registry.is_none() {
            return Err(base::error::ToolError::Validation(
                "team_name/name require team coordination to be enabled for this \
                 session (team.enabled)"
                    .into(),
            ));
        }
        if matches!(
            input.permission_mode,
            Some(base::permission::PermissionMode::Bubble)
        ) {
            return Err(base::error::ToolError::Validation(
                "permission_mode: \"bubble\" is not valid here — it needs an available \
                 lead, which a persistent member's lead (only runs code during its own \
                 turns) doesn't reliably have"
                    .into(),
            ));
        }
        req.prompt = input.prompt;
        req.stops_with = StopsWith::Session;
        req.mode = DelegateMode::TeamMember {
            seat: TeamSeat { team, member },
            permission_mode: input.permission_mode,
            lifetime: MemberLifetime::Persistent,
        };
        match self.delegator.delegate(req).await.map_err(tool_error)? {
            Delegated::Started(started) => {
                Ok(bg_result(&started.task_id, "queued for team member"))
            }
            Delegated::Finished(_) => Err(base::error::ToolError::Execution(anyhow!(
                "a team member answered instead of queuing the message"
            ))),
        }
    }

    async fn make_worktree(
        &self,
        ctx: &ToolContext,
        slug: &str,
    ) -> Result<tools::worktree::WorktreeHandle, base::error::ToolError> {
        // From the project, not the working directory: a sub-agent working in
        // a worktree would find the worktree as the repository, whose git
        // directory a sandboxed git refuses.
        let repo = self
            .delegator
            .env
            .session_config
            .settings()
            .paths
            .project_root()
            .map_or_else(|| ctx.session.env().cwd.clone(), Path::to_path_buf);
        create_worktree(
            &ctx.exec,
            &ctx.engine_git,
            &repo,
            slug,
            &self.delegator.env.session_config.settings().paths.layout,
        )
        .await
        .map_err(|e| base::error::ToolError::Execution(anyhow!("worktree: {e}")))
    }

    async fn start_in_background(
        &self,
        mut req: DelegateRequest,
        worktree: Option<tools::worktree::WorktreeHandle>,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        req.mode = DelegateMode::Background {
            notify_parent: true,
        };
        match self.delegator.dispatch(req, worktree).await {
            Ok(Delegated::Started(started)) => Ok(bg_result(&started.task_id, "spawned")),
            Ok(Delegated::Finished(_)) => Err(base::error::ToolError::Execution(anyhow!(
                "a background sub-agent finished before it was started"
            ))),
            Err(e) => Err(tool_error(e)),
        }
    }
}

/// How a delegate that could not be started reads as the tool's error.
pub(crate) fn tool_error(e: DelegateError) -> base::error::ToolError {
    match e {
        DelegateError::Limit(m) | DelegateError::Depth(m) | DelegateError::Invalid(m) => {
            base::error::ToolError::Denied(m)
        }
        DelegateError::Build(m) | DelegateError::Unsupported(m) => {
            base::error::ToolError::Execution(anyhow!(m))
        }
        other => base::error::ToolError::Denied(other.to_string()),
    }
}

fn sub_agent_error(e: impl std::fmt::Display) -> base::tool::ToolResult {
    base::tool::ToolResult::error_text(format!("sub-agent error: {e}"))
}

#[async_trait]
impl base::tool::Tool for AgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
            .callable_from_code(true)
            .code_result("the sub-agent's final text; an object when output_schema is given")
            .concurrency_safe(true)
    }
    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::Delegates)
    }
    fn name(&self) -> &str {
        AGENT_TOOL_NAME
    }
    fn description(&self) -> &str {
        self.delegator.catalog.description()
    }
    fn input_schema(&self, _: PathStyle) -> Value {
        serde_json::to_value(schemars::schema_for!(AgentInput)).unwrap_or(Value::Null)
    }
    async fn guide(&self, ctx: &base::tool::PromptContext) -> Option<String> {
        Some(tools::worktree::name_the_directories(
            include_str!("agent_tool.prompt.md"),
            &ctx.layout,
        ))
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        let inp: AgentInput = serde_json::from_value(input)
            .map_err(|e| base::error::ToolError::Validation(format!("{e}")))?;
        if ctx.from_code {
            if let Some(field) = [
                (inp.background, "background"),
                (
                    inp.team_name.is_some() || inp.name.is_some(),
                    "team_name/name",
                ),
                (inp.worktree.is_some(), "worktree"),
            ]
            .into_iter()
            .find_map(|(given, field)| given.then_some(field))
            {
                return Err(base::error::ToolError::Validation(format!(
                    "`{field}` is not available from a program: agent() runs a sub-agent and \
                     waits for it. Start that sub-agent with a direct Agent call instead."
                )));
            }
        }
        let mut req = DelegateRequest::new(
            String::new(),
            DelegateMode::Wait,
            StopsWith::Call(ctx.cancel.clone()),
        );
        req.description = inp.description.clone();
        req.agent_type = inp.subagent_type.clone();
        req.model = inp.model.clone();
        req.provider = inp.provider.clone();
        req.output_schema = inp.output_schema.clone();
        req.started_by(&ctx);
        if inp.team_name.is_some() || inp.name.is_some() {
            return self.message_member(inp, req).await;
        }
        req.prompt = inp.prompt;
        let worktree = match &inp.worktree {
            Some(slug) => Some(self.make_worktree(&ctx, slug).await?),
            None => None,
        };
        if let Some(h) = &worktree {
            req.workdir = Workdir::Linked(h.linked());
        }
        if inp.background {
            return self.start_in_background(req, worktree).await;
        }
        if ctx.from_code {
            req.asks = AskRoute::Refuse;
        }
        let delegated = self.delegator.delegate(req).await;
        let kept = match worktree {
            Some(h) => h.remove_if_untouched().await,
            None => None,
        };
        let left = kept
            .as_ref()
            .and_then(|kept| worktree_element(&kept_worktree_detail(kept)))
            .map(|element| format!("\n\n{element}"))
            .unwrap_or_default();
        Ok(match delegated {
            Ok(Delegated::Finished(outcome)) => match outcome.end {
                DelegateEnd::Failed(e) => sub_agent_error(format!("{e}{left}")),
                _ => {
                    let mut result = base::tool::ToolResult::text(outcome.text + &left);
                    result.structured_content = outcome.structured;
                    result
                }
            },
            Ok(Delegated::Started(_)) => {
                sub_agent_error(format!("it was started instead of waited on{left}"))
            }
            Err(e) => sub_agent_error(format!("{}{left}", tool_error(e))),
        })
    }
}
