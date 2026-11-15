//! Coordinator tests: shared stubs here, stage dispatch in `stages`, what a run records and reports in `records`.

use super::*;
use crate::tool::AggregateMode;
use async_trait::async_trait;
use base::interface::delegate::{
    DelegateEnd, DelegateMode, DelegateRequest, Delegated, MemberLifetime, Workdir,
};
use base::interface::delegate::{DelegateError, DelegateOutcome};
use base::interface::model::StreamParams;
use base::interface::model::{ModelError, ModelStream};
use base::prompt::PromptBlock;
use base::provider::ApiType;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// A `Model` stub that is never actually invoked by these tests (all
/// test stages use `aggregate: None`, so `aggregate()` never calls it).
struct UnusedModel;

#[async_trait]
impl Model for UnusedModel {
    fn api_type(&self) -> ApiType {
        ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<base::interface::model::ModelMessage>,
        _params: StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        panic!("UnusedModel::stream should not be called by these tests");
    }
}

fn finished_as(text: impl Into<String>, end: DelegateEnd) -> Result<Delegated, DelegateError> {
    Ok(Delegated::Finished(DelegateOutcome {
        text: text.into(),
        end,
        structured: None,
        session_id: "member-session".into(),
    }))
}

fn finished(text: impl Into<String>) -> Result<Delegated, DelegateError> {
    finished_as(text, DelegateEnd::Completed)
}

fn cwd_of(req: &DelegateRequest) -> std::path::PathBuf {
    match &req.workdir {
        Workdir::At(dir) => dir.clone(),
        other => panic!("a stage member runs in the lead's cwd, not {other:?}"),
    }
}

fn stage_tools(req: DelegateRequest) -> Vec<Arc<dyn base::tool::Tool>> {
    match req.mode {
        DelegateMode::TeamMember {
            lifetime: MemberLifetime::Stage { extra_tools, .. },
            ..
        } => extra_tools,
        _ => panic!("only stage members are delegated here"),
    }
}

/// A fake `Delegator` whose `delegate` sleeps for a duration
/// encoded in the agent's task text (as a plain millisecond count),
/// tracking in-flight concurrency via an `AtomicUsize`. Records every
/// prompt it was handed so tests can inspect the injected team context.
struct FakeDelegator {
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
    prompts: std::sync::Mutex<Vec<String>>,
    agent_types: std::sync::Mutex<Vec<Option<String>>>,
}

impl FakeDelegator {
    fn new() -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
            peak_in_flight: AtomicUsize::new(0),
            prompts: std::sync::Mutex::new(Vec::new()),
            agent_types: std::sync::Mutex::new(Vec::new()),
        }
    }
}

/// The coordinator prepends a `<team-context>` block, so the task
/// text a test encoded is the last line of what the delegator receives.
fn task_text(prompt: &str) -> String {
    prompt
        .trim_end()
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

#[async_trait]
impl Delegator for FakeDelegator {
    async fn stop_member(&self, _: &TeamSeat) {}

    fn live_members(&self) -> Vec<base::interface::delegate::LiveMember> {
        Vec::new()
    }

    async fn delegate(&self, request: DelegateRequest) -> Result<Delegated, DelegateError> {
        let prompt = request.prompt;
        let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_in_flight.fetch_max(cur, Ordering::SeqCst);
        self.prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(prompt.clone());
        self.agent_types
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.agent_type);

        let task = task_text(&prompt);
        let delay_ms: u64 = task.parse().unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;

        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        finished(format!("done-{task}"))
    }
}

fn make_ctx() -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::tempdir().expect("tempdir");
    let ctx = session_state::testing::tool_context_for_test(
        platform::local_providers(),
        dir.path().to_path_buf(),
    );
    (dir, ctx)
}

fn make_request(ctx: ToolContext, stages: Vec<TeamStage>) -> OrchestrateRequest {
    OrchestrateRequest {
        model: Arc::new(UnusedModel),
        config: Arc::new(EngineConfig::defaults_for("test")),
        stages,
        name: "test-team".to_string(),
        scratchpad: None,
        ctx,
        permission_mode: None,
    }
}

/// Each spawn waits until all `n` of a stage are in flight at once.
struct GatheringDelegator {
    gathered: tokio::sync::Barrier,
}

#[async_trait]
impl Delegator for GatheringDelegator {
    async fn stop_member(&self, _: &TeamSeat) {}

    fn live_members(&self) -> Vec<base::interface::delegate::LiveMember> {
        Vec::new()
    }

    async fn delegate(&self, _: DelegateRequest) -> Result<Delegated, DelegateError> {
        self.gathered.wait().await;
        finished("done")
    }
}

mod records;
mod stages;
