//! Delegation: what a session's delegates are built from, captured once when
//! the session is assembled, and the one dispatch every way of starting one
//! comes through — the model's `Agent` call, a team member, an `"agent"` hook,
//! a forked skill, a host's `session.startAgent`.
//!
//! `Delegator::dispatch` matches the mode once and exhaustively. The mode
//! decides the slot, the task and what comes back; the request decides the
//! rest, the same way for every mode.

mod admission;
mod env;
mod lifecycle;
mod members;
mod run;
mod tools;

use async_trait::async_trait;
use base::interface::delegate::{
    DelegateError, DelegateMode, DelegateRequest, Delegated, LiveMember, MemberLifetime, StopsWith,
    TeamSeat, Workdir,
};

pub(crate) use admission::{Admission, HookAgentRunning, HookAgents, Limits, SlotRule};
pub use admission::{SubagentSlots, DEFAULT_MAX_CONCURRENT_SUBAGENTS};
pub(crate) use env::*;
pub(crate) use lifecycle::{DelegateRun, Start, StopReason};
pub(crate) use members::*;
pub(crate) use tools::*;

use crate::agent_tool::{AgentCatalog, Spec};

pub(crate) struct Delegator {
    pub(crate) env: DelegateEnv,
    pub(crate) catalog: AgentCatalog,
    pub(crate) pool: ToolPool,
    pub(crate) limits: Limits,
    pub(crate) members: PersistentMembers,
    /// What a run that outlives the call — a background delegate, a
    /// persistent member — holds the delegator by.
    me: std::sync::Weak<Delegator>,
}

impl Delegator {
    pub(crate) fn new(
        env: DelegateEnv,
        catalog: AgentCatalog,
        pool: ToolPool,
        limits: Limits,
        members: PersistentMembers,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new_cyclic(|me| Self {
            env,
            catalog,
            pool,
            limits,
            members,
            me: me.clone(),
        })
    }

    fn me(&self) -> Result<std::sync::Arc<Self>, DelegateError> {
        self.me
            .upgrade()
            .ok_or_else(|| DelegateError::Unsupported("the session is closing".into()))
    }

    /// The worktree a background delegate takes along, and its own route.
    async fn prepare(
        &self,
        req: &DelegateRequest,
        worktree: Option<::tools::worktree::WorktreeHandle>,
    ) -> Result<
        (
            Option<::tools::worktree::WorktreeHandle>,
            Option<std::sync::Arc<dyn base::provider::TaskRouting>>,
        ),
        DelegateError,
    > {
        let worktree = Self::taken_along(&req.mode, worktree).await?;
        let def = req
            .agent_type
            .as_deref()
            .and_then(|t| self.agent_type_def(t));
        let by_host = matches!(req.mode, DelegateMode::Host { .. });
        let provider = req.provider.as_deref();
        match self.own_route(provider, by_host, req.model.as_deref(), def.as_ref()) {
            Ok(route) => Ok((worktree, route)),
            Err(e) => {
                if let Some(worktree) = worktree {
                    worktree.remove_if_untouched().await;
                }
                Err(DelegateError::Invalid(e))
            }
        }
    }

    async fn taken_along(
        mode: &DelegateMode,
        worktree: Option<::tools::worktree::WorktreeHandle>,
    ) -> Result<Option<::tools::worktree::WorktreeHandle>, DelegateError> {
        match worktree {
            Some(worktree) if !matches!(mode, DelegateMode::Background { .. }) => {
                worktree.remove_if_untouched().await;
                Err(DelegateError::Invalid(
                    "only a background delegate takes its worktree along".into(),
                ))
            }
            worktree => Ok(worktree),
        }
    }

    /// The delegate's working directory, and where in the repository's git
    /// directory a worktree there commits to.
    fn workdir(
        &self,
        workdir: Workdir,
    ) -> Result<(std::path::PathBuf, Option<base::paths::WorktreeGit>), DelegateError> {
        match workdir {
            Workdir::Inherit => Ok((self.env.parent.state.env().cwd.clone(), None)),
            Workdir::At(dir) => Ok((dir, None)),
            Workdir::Linked(worktree) => Ok((worktree.dir, worktree.git)),
            Workdir::Worktree(_) => Err(DelegateError::Unsupported(
                "a delegate's worktree is made by its caller".into(),
            )),
        }
    }
}

#[async_trait]
impl base::interface::delegate::Delegator for Delegator {
    async fn delegate(&self, req: DelegateRequest) -> Result<Delegated, DelegateError> {
        self.dispatch(req, None).await
    }

    async fn stop_member(&self, seat: &TeamSeat) {
        self.stop_persistent_member(seat);
    }

    fn live_members(&self) -> Vec<LiveMember> {
        self.live_persistent_members()
    }
}

impl Delegator {
    /// `worktree` is the one `req.workdir` links to, for a background
    /// delegate to take along and finish with; no other mode takes one.
    // Boxed: inlined into `AgentTool::call`, a whole delegate run's future
    // nests deeper than the compiler's query depth limit.
    pub(crate) fn dispatch(
        &self,
        req: DelegateRequest,
        worktree: Option<::tools::worktree::WorktreeHandle>,
    ) -> futures::future::BoxFuture<'_, Result<Delegated, DelegateError>> {
        Box::pin(self.dispatch_unboxed(req, worktree))
    }

    async fn dispatch_unboxed(
        &self,
        req: DelegateRequest,
        worktree: Option<::tools::worktree::WorktreeHandle>,
    ) -> Result<Delegated, DelegateError> {
        let (worktree, route) = self.prepare(&req, worktree).await?;
        let DelegateRequest {
            prompt,
            description,
            agent_type,
            model,
            tools,
            deny_rules,
            grants,
            output_schema,
            workdir,
            stops_with,
            called_from,
            asks,
            tag,
            spend,
            mode,
            ..
        } = req;
        let (cwd, git) = self.workdir(workdir)?;
        let spec = Spec {
            session_id: base::id::Id::new().to_string(),
            agent_type,
            model,
            route,
            tools,
            output_schema,
            permission_mode: None,
            seat: None,
            cwd,
            git,
            called_from,
            spend,
        };
        let stop = match stops_with {
            StopsWith::Call(token) => token.child_token(),
            StopsWith::Session => self.env.parent.ended.child_token(),
        };
        let ask = run::Ask {
            asks,
            rules: delegate_rules(deny_rules, grants),
        };
        match mode {
            DelegateMode::Wait => self.wait(spec, prompt, description, stop, ask).await,
            DelegateMode::Background { notify_parent } => {
                let started = run::Started {
                    description,
                    tag: None,
                    notify_parent,
                    worktree,
                };
                self.start(spec, prompt, stop, ask, started).await
            }
            DelegateMode::Host { notify_parent } => {
                let started = run::Started {
                    description,
                    tag,
                    notify_parent,
                    worktree: None,
                };
                self.start(spec, prompt, stop, ask, started).await
            }
            DelegateMode::TeamMember {
                seat,
                permission_mode,
                lifetime: MemberLifetime::Stage { stage, extra_tools },
            } => {
                let spec = Spec {
                    permission_mode,
                    seat: Some(seat),
                    ..spec
                };
                self.stage_member(spec, prompt, stop, ask, stage, extra_tools)
                    .await
            }
            DelegateMode::TeamMember {
                seat,
                permission_mode,
                lifetime: MemberLifetime::Persistent,
            } => {
                let spec = Spec {
                    permission_mode,
                    ..spec
                };
                self.message_member(seat, spec, prompt).await
            }
            DelegateMode::Hook { judged_session } => {
                self.judge(&judged_session, spec, prompt, stop, ask).await
            }
        }
    }
}

/// Only the request's deny rules hold, with its grants besides.
fn delegate_rules(
    deny_rules: Vec<base::permission::PermissionRule>,
    grants: Vec<base::interface::permission::Grant>,
) -> Vec<base::permission::PermissionRule> {
    deny_rules
        .into_iter()
        .filter(|r| r.behavior == base::permission::RuleBehavior::Deny)
        .chain(grants.into_iter().map(crate::agent_tool::grant_rule))
        .collect()
}
