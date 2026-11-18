//! 工具调用上下文的唯一构造点，子调用与内联命令的分发，会话快照的采集。

use super::tool_exec::{execute_tool_with_telemetry, Caller, CommandCalls, ToolExecCtx};
use super::tool_result::PendingToolImage;
use crate::agent::Agent;
use base::context::Session;
use base::tool::ToolContext;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The session's frozen environment snapshot, taken in its working directory
/// — looked up in as a project only when it is one.
pub(crate) async fn collect_frozen(
    paths: &base::paths::PathSettings,
    environment: &dyn base::interface::environment::Environment,
    engine_git: &base::engine_exec::EngineExec,
) -> base::frozen::FrozenContext {
    use base::frozen::CollectOptions;
    let config_paths = base::paths::ConfigPaths::from_settings(paths);
    let cwd = paths.working_dir().to_path_buf();
    let opts = CollectOptions {
        outside_project: paths.project_root().is_none(),
        ..CollectOptions::defaults()
    };
    super::frozen::collect(cwd, opts, &config_paths, environment, engine_git).await
}

/// Calls a tool makes on its session's behalf, through the same dispatch as
/// the model's own calls: a `RunCode` program's or a plugin command's
/// sub-calls, and a skill's `` !`cmd` ``. Each runs under its own tool-use id derived from the
/// caller's and cannot ask a person anything. Neither reports progress: the
/// host never saw a `tool_use` under a derived id.
pub(super) struct SubCallDispatcher {
    ctx: ToolExecCtx,
    prefix: String,
    next: std::sync::atomic::AtomicU64,
}

impl SubCallDispatcher {
    /// A program's sub-calls leave the round's batch alone: a hook ending
    /// the turn or an image a sub-call returned are the caller's to report,
    /// not the round's; and the program's own progress reaches the host as
    /// its task's.
    fn for_code(ctx: &ToolExecCtx) -> Self {
        let mut ctx = ctx.clone();
        ctx.elicitation = Arc::new(NoAsking);
        ctx.host_calls = None;
        ctx.discontinued = Arc::new(std::sync::atomic::AtomicBool::new(false));
        ctx.images = Arc::default();
        ctx.caller = Caller::Code;
        ctx.events = None;
        Self::new(ctx)
    }

    /// A skill's inline command ends the turn of the `Skill` call it runs in
    /// when a `PostToolUse` hook says so, and the host hears what it left
    /// behind; the images it returns are not the round's.
    fn for_inline_commands(ctx: &ToolExecCtx) -> Self {
        let mut ctx = ctx.clone();
        ctx.elicitation = Arc::new(NoAsking);
        ctx.images = Arc::default();
        ctx.caller = Caller::InlineCommand;
        Self::new(ctx)
    }

    fn new(ctx: ToolExecCtx) -> Self {
        Self {
            prefix: ctx.tool_use_id.clone(),
            ctx,
            next: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn callable_tool(&self, name: &str) -> Option<Arc<dyn base::tool::Tool>> {
        self.ctx
            .tools
            .get(name)
            .filter(|t| t.spec().callable_from_code)
    }

    async fn dispatch(
        &self,
        name: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<base::interface::tool_dispatch::SubCallAnswer, String> {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut ctx = self.ctx.clone();
        ctx.tool_use_id = format!("{}.{n}", self.prefix);
        ctx.cancel = cancel;
        execute_tool_with_telemetry(&ctx, name, input)
            .await
            .map(|a| base::interface::tool_dispatch::SubCallAnswer {
                text: a.text,
                structured: a.structured,
                reported_failure: a.reported_failure,
            })
    }
}

#[async_trait::async_trait]
impl base::interface::tool_dispatch::ToolDispatch for SubCallDispatcher {
    fn callable(&self) -> Vec<base::interface::tool_dispatch::CallableTool> {
        let mut tools: Vec<_> = self
            .ctx
            .tools
            .list()
            .into_iter()
            .filter_map(|t| {
                let spec = t.spec();
                spec.callable_from_code
                    .then(|| base::interface::tool_dispatch::CallableTool {
                        name: t.name().to_string(),
                        description: spec
                            .short_description
                            .unwrap_or_else(|| t.description().to_string()),
                        input_schema: base::tool::compact_schema(
                            t.input_schema(self.ctx.config.path_style),
                        ),
                        result: spec.code_result.map(str::to_string),
                    })
            })
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    fn concurrency_safe(&self, name: &str, input: &serde_json::Value) -> bool {
        self.callable_tool(name)
            .is_some_and(|t| t.call_traits(input).concurrency_safe)
    }

    async fn call(
        &self,
        name: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<base::interface::tool_dispatch::SubCallAnswer, String> {
        if self.callable_tool(name).is_none() {
            return Err(format!("{name} cannot be called from code in this session"));
        }
        self.dispatch(name, input, cancel).await
    }

    fn for_command(
        &self,
        scope: base::interface::tool_dispatch::CommandScope,
    ) -> Option<Arc<dyn base::interface::tool_dispatch::ToolDispatch>> {
        let mut ctx = self.ctx.clone();
        ctx.caller = Caller::Command(Arc::new(CommandCalls::new(scope, &ctx.cwd)));
        Some(Arc::new(Self::new(ctx)))
    }
}

#[async_trait::async_trait]
impl base::interface::inline_command::InlineCommands for SubCallDispatcher {
    async fn run_bash(
        &self,
        command: &str,
        cancel: CancellationToken,
    ) -> Result<base::interface::tool_dispatch::SubCallAnswer, String> {
        self.dispatch("Bash", serde_json::json!({ "command": command }), cancel)
            .await
    }
}

/// The approval a sub-call would need cannot be asked for from inside a
/// program; the model has to make that call itself.
pub(super) struct NoAsking;

#[async_trait::async_trait]
impl base::interface::elicitation::Elicitation for NoAsking {
    async fn ask(
        &self,
        _: base::interface::elicitation::ElicitRequest,
    ) -> base::interface::elicitation::ElicitOutcome {
        base::interface::elicitation::ElicitOutcome::declined(
            "this call needs the user's approval, which a program cannot ask for — \
             make this call directly instead of from code",
        )
    }
}

/// Whether anyone can be asked about a call the session makes. The one place
/// that reads `unattended` to decide it.
pub(crate) fn attendance_of(
    session: &session_state::SessionState,
    caller: &Caller,
) -> base::interface::permission::Attendance {
    use base::interface::permission::Attendance;
    if matches!(caller, Caller::Person) {
        return Attendance::Attended;
    }
    if session.env().unattended {
        return Attendance::Unattended;
    }
    match caller {
        Caller::Model | Caller::Person => Attendance::Attended,
        Caller::Code => Attendance::NoOneToAsk(
            "this call needs the user's approval, which a program cannot ask for — make this \
             call directly instead of from code",
        ),
        Caller::Command(_) => Attendance::NoOneToAsk(
            "this call needs the user's approval, which a plugin command cannot ask for — run \
             it directly instead",
        ),
        Caller::InlineCommand => Attendance::NoOneToAsk(
            "it needs the user's approval, which a skill's inline command cannot ask for; run \
             it with the Bash tool instead",
        ),
    }
}

/// The context a call runs with: the same for validating its input and for
/// judging it as for running it. `spec` is the called tool's; `None` where
/// no tool is called, only something of the session's run under a call's
/// context (a `/name` command's expansion, the turn's baseline).
pub(super) fn tool_context_for(
    ctx: &ToolExecCtx,
    spec: Option<&base::tool::ToolSpec>,
) -> ToolContext {
    let mut tool_ctx = ToolContext::new(
        ctx.cwd.clone(),
        ctx.cancel.clone(),
        ctx.exec.clone(),
        ctx.config.clone(),
        ctx.session_state.clone(),
    );
    tool_ctx.session_id = ctx.session_id.clone();
    tool_ctx.turn_no = ctx.turn_no;
    tool_ctx.tool_use_id = ctx.tool_use_id.clone();
    tool_ctx.agent_depth = ctx.agent_depth;
    tool_ctx.elicitation = Some(Arc::clone(&ctx.elicitation));
    tool_ctx.host_calls = ctx.host_calls.clone();
    tool_ctx.dispatch = spec.is_some_and(|s| s.needs_dispatch).then(|| {
        Arc::new(SubCallDispatcher::for_code(ctx))
            as Arc<dyn base::interface::tool_dispatch::ToolDispatch>
    });
    tool_ctx.from_code = ctx.caller.is_code();
    tool_ctx.confinement = crate::exec::confinement_of(
        &ctx.session_state,
        &ctx.config,
        &ctx.exec,
        &ctx.cwd,
        &ctx.session_id,
    );
    tool_ctx.engine_git = Arc::clone(&ctx.engine_git);
    tool_ctx.attendance = attendance_of(&ctx.session_state, &ctx.caller);
    tool_ctx.turn = ctx.turn.clone();
    tool_ctx.inline_commands = Some(Arc::new(SubCallDispatcher::for_inline_commands(ctx)));
    tool_ctx.path_denials = ctx.permission.path_denials.clone();
    tool_ctx
}

impl Agent {
    /// What this turn's calls run with, before any one call's id is set.
    pub(super) fn tool_exec_ctx(
        &self,
        cancel: &CancellationToken,
        discontinued: &Arc<std::sync::atomic::AtomicBool>,
        images: &Arc<std::sync::Mutex<Vec<PendingToolImage>>>,
    ) -> ToolExecCtx {
        ToolExecCtx {
            tool_use_id: String::new(),
            tools: Arc::clone(self.tooling.tools()),
            cwd: self.settings.paths.working_dir().to_path_buf(),
            session_id: self.session.session_id.clone(),
            turn_no: self.session.turn_count,
            agent_depth: self.identity.agent_depth(),
            telemetry_handle: self.telemetry.handle().clone(),
            turn_id: self.current_turn_id.clone(),
            cancel: cancel.clone(),
            hooks: Arc::clone(self.gate.hooks()),
            config: Arc::clone(&self.config),
            exec: self.exec.clone(),
            engine_git: Arc::clone(&self.engine_git),
            permission: self.gate.permission().clone(),
            session_state: Arc::clone(&self.session_state),
            elicitation: Arc::clone(self.gate.elicitation()),
            host_calls: Some(Arc::clone(&self.channels.host_calls)),
            tool_middleware: Arc::clone(self.tooling.tool_middleware()),
            result_transformers: Arc::clone(self.tooling.result_transformers()),
            discontinued: Arc::clone(discontinued),
            images: Arc::clone(images),
            caller: Caller::Model,
            events: Some(self.channels.event_tx.clone()),
            turn: Arc::clone(&self.turn.scope),
            checkpoints: self.tooling.checkpoints().cloned(),
        }
    }

    /// What a plugin command given as `/name args` runs with, under
    /// `tool_use_id`, which no round's batch holds: the user's own call when
    /// a person wrote this turn, otherwise the model's, decided as any.
    pub(super) fn typed_command_ctx(
        &self,
        cancel: &CancellationToken,
        tool_use_id: String,
    ) -> ToolExecCtx {
        let mut ctx = self.tool_exec_ctx(cancel, &Arc::default(), &Arc::default());
        ctx.tool_use_id = tool_use_id;
        if self.turn_written_by_a_person() {
            ctx.caller = Caller::Person;
        }
        ctx
    }

    /// Whether this turn's text came from the person at the host. A
    /// sub-agent's or team member's turns are what a model wrote; a goal
    /// round, a cron or scheduled job's are the engine's.
    pub(super) fn turn_written_by_a_person(&self) -> bool {
        turn_written_by_a_person(self.identity.agent_depth(), &self.current_turn_id)
    }

    /// The context of something the turn runs outside any tool call: a
    /// `/name` command's expansion, whose inline commands' ids derive from
    /// `tool_use_id`, or the turn's nested-repository baseline. There is no
    /// round yet, so a `PostToolUse` hook's `continue: false` has none to end.
    pub(super) fn context_outside_a_call(
        &self,
        cancel: &CancellationToken,
        tool_use_id: String,
    ) -> ToolContext {
        let mut ctx = self.tool_exec_ctx(cancel, &Arc::default(), &Arc::default());
        ctx.tool_use_id = tool_use_id;
        tool_context_for(&ctx, None)
    }
}

fn turn_written_by_a_person(agent_depth: u32, turn_id: &str) -> bool {
    agent_depth == 0
        && base::context::goal::TurnOrigin::from_turn_id(turn_id)
            == base::context::goal::TurnOrigin::Human
}

#[cfg(test)]
mod tests {
    use super::turn_written_by_a_person;

    #[test]
    fn only_a_root_turn_the_host_sent_is_written_by_a_person() {
        assert!(turn_written_by_a_person(0, "7f0c2a1e-turn"));
        assert!(!turn_written_by_a_person(1, "7f0c2a1e-turn"));
        assert!(!turn_written_by_a_person(0, "cron-job1"));
        assert!(!turn_written_by_a_person(0, "schedule-job1-x"));
        assert!(!turn_written_by_a_person(0, "goal-3-2-x"));
    }
}
