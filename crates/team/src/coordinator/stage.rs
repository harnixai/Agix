//! One stage's members: each is delegated from the lead's session under the
//! stage's concurrency limit, with its own mailbox and team-memory tools.

use base::interface::delegate::{
    CallSite, DelegateEnd, DelegateMode, DelegateRequest, Delegated, Delegator, MemberLifetime,
    StopsWith, TeamSeat, Workdir,
};
use base::tool::ToolContext;
use std::sync::Arc;

use super::MemberEnd;
use crate::tool::TeamStage;

struct MemberCall {
    seat: TeamSeat,
    stage: String,
    prompt: String,
    agent_type: Option<String>,
    extra_tools: Vec<Arc<dyn base::tool::Tool>>,
    permission_mode: base::permission::PermissionMode,
}

pub(super) struct StageRun<'a> {
    pub delegator: &'a dyn Delegator,
    pub ctx: &'a ToolContext,
    pub team: &'a str,
    pub team_id: &'a str,
    pub stage: &'a TeamStage,
    pub stage_index: usize,
    pub stage_count: usize,
    pub later_stages: &'a [String],
    pub scratchpad: &'a str,
    pub team_memory: &'a str,
    pub mailbox: &'a Arc<crate::mailbox::MailboxStore>,
    pub memory: &'a Arc<crate::team_memory::TeamMemoryStore>,
    pub permission_mode: base::permission::PermissionMode,
    pub concurrency: usize,
}

impl StageRun<'_> {
    pub(super) async fn run_members(&self) -> Vec<(String, String, MemberEnd)> {
        // `.max(1)`: a zero-permit semaphore never grants, so every acquire would block forever.
        let permits = Arc::new(tokio::sync::Semaphore::new(self.concurrency.max(1)));
        let futs = self.stage.agents.iter().map(|agent_spec| {
            let permits = permits.clone();
            let label = agent_spec.label.clone();
            let call = self.member_call(&label, agent_spec);
            async move {
                let _permit = permits.acquire_owned().await.expect("semaphore closed");
                if self.ctx.cancel.is_cancelled() {
                    return (label, String::new(), MemberEnd::Cancelled);
                }
                let (text, end) = run_member(self.delegator, call, self.ctx).await;
                (label, text, end)
            }
        });
        futures::future::join_all(futs).await
    }

    fn member_call(&self, label: &str, agent_spec: &crate::tool::TeamAgentSpec) -> MemberCall {
        let peers: Vec<String> = self
            .stage
            .agents
            .iter()
            .map(|a| a.label.clone())
            .filter(|l| l != label)
            .collect();
        let prompt = crate::prompt::build_worker_prompt(
            &crate::prompt::WorkerContext {
                team: self.team,
                team_id: self.team_id,
                label,
                stage: &self.stage.name,
                stage_index: self.stage_index,
                stage_count: self.stage_count,
                peers: &peers,
                later_stages: self.later_stages,
                scratchpad: self.scratchpad,
                team_memory: self.team_memory,
            },
            &agent_spec.prompt,
        );
        // The lead is blocked awaiting the whole stage, so these mailbox
        // tools are for siblings only.
        let extra_tools: Vec<Arc<dyn base::tool::Tool>> = vec![
            Arc::new(crate::mailbox::SendMessageTool::new(
                self.mailbox.clone(),
                label.to_string(),
            )),
            Arc::new(crate::mailbox::ReadMailTool::new(
                self.mailbox.clone(),
                label.to_string(),
            )),
            Arc::new(crate::mailbox::ListPeersTool::new(self.mailbox.clone())),
            Arc::new(crate::team_memory::TeamMemorySaveTool::new(
                self.memory.clone(),
                format!("{}/{label}", self.team_id),
            )),
            Arc::new(crate::team_memory::TeamMemoryReadTool::new(
                self.memory.clone(),
            )),
        ];
        MemberCall {
            seat: TeamSeat {
                team: self.team.to_string(),
                member: label.to_string(),
            },
            stage: self.stage.name.clone(),
            prompt,
            agent_type: agent_spec.agent_type.clone(),
            extra_tools,
            permission_mode: self.permission_mode,
        }
    }
}

/// One stage member, delegated and waited on. It stops with the lead's call.
async fn run_member(
    delegator: &dyn Delegator,
    call: MemberCall,
    ctx: &ToolContext,
) -> (String, MemberEnd) {
    let mode = DelegateMode::TeamMember {
        seat: call.seat,
        permission_mode: Some(call.permission_mode),
        lifetime: MemberLifetime::Stage {
            stage: call.stage,
            extra_tools: call.extra_tools,
        },
    };
    let mut req = DelegateRequest::new(call.prompt, mode, StopsWith::Call(ctx.cancel.clone()));
    req.agent_type = call.agent_type;
    req.workdir = Workdir::At(ctx.cwd.clone());
    req.called_from = Some(CallSite {
        turn: ctx.turn_no,
        tool_use_id: ctx.tool_use_id.clone(),
    });
    match delegator.delegate(req).await {
        Ok(Delegated::Finished(outcome)) => match outcome.end {
            DelegateEnd::Completed | DelegateEnd::CutShort(_) => (outcome.text, MemberEnd::Done),
            DelegateEnd::Cancelled => (outcome.text, MemberEnd::Cancelled),
            DelegateEnd::Failed(e) => (format!("ERROR: {e}"), MemberEnd::Failed),
        },
        Ok(Delegated::Started(_)) => (
            "ERROR: a stage member was started instead of waited on".into(),
            MemberEnd::Failed,
        ),
        Err(e) => (format!("ERROR: {e}"), MemberEnd::Failed),
    }
}
