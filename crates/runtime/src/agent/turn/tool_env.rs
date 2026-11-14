//! 工具运行环境：沙箱、出口、引擎 git 的装配，工具调用上下文的唯一构造点，子调用与内联命令的分发。

use super::tool_exec::{execute_tool_with_telemetry, Caller, ToolExecCtx};
use super::tool_result::PendingToolImage;
use crate::agent::Agent;
use base::tool::ToolContext;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Lower `EngineConfig`'s sandbox policy into the cross-crate view a tool
/// receives.
///
/// `deny_read`'s `None` means "use the built-in credential deny defaults",
/// and `SandboxSettings` has no way to say that — it carries a plain `Vec`,
/// with the same empty-means-defaults convention that
/// `tools::bash::to_sandbox_policy` reads on the other side. So `None` and
/// `Some(vec![])` both flatten to an empty vec here, and the distinction is
/// preserved where it is actually decided (`EngineConfig::from_loaded`).
pub(super) fn sandbox_settings_from(
    config: &base::context::EngineConfig,
) -> base::tool::SandboxSettings {
    base::tool::SandboxSettings {
        allow_read: config.sandbox_policy.allow_read.clone(),
        deny_read: config.sandbox_policy.deny_read.clone().unwrap_or_default(),
        allowed_domains: config.sandbox_policy.allowed_domains.clone(),
        network_mode: config.sandbox_policy.network_mode,
        deny_write: config.sandbox_policy.deny_write.clone(),
        allow_write: config.sandbox_policy.allow_write.clone(),
        paths: config.sandbox_policy.paths.clone(),
        require_enforcement: config.sandbox_policy.require_enforcement,
        unattended_requires_network_policy: config
            .sandbox_policy
            .unattended_requires_network_policy,
    }
}

/// How the engine starts its own git and helpers for this session: through
/// its providers, in the sandbox its tool calls run in whenever it confines
/// them.
pub(crate) fn engine_exec_from(
    config: &base::context::EngineConfig,
    exec: &base::interface::exec::ExecProviders,
    session_id: &str,
    trust: base::permission::ProjectTrust,
) -> base::engine_exec::EngineExec {
    tools::bash::engine_exec_for(
        &sandbox_settings_from(config),
        exec,
        config.dangerously_disable_sandbox,
        session_id,
        trust,
    )
}

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

/// The execution providers a session's tool calls reach the machine through,
/// with every process group a command starts recorded in `process_groups`.
///
/// Only the egress is configured from `EngineConfig`; the other three are
/// this machine either way. `sandbox.network_mode` is a statement about where
/// the *model* may reach, which is as true of a `WebFetch` as it is of a
/// `curl` inside a sandboxed command.
pub(crate) fn exec_providers_from(
    config: &base::context::EngineConfig,
    process_groups: Option<&Arc<platform::GroupLedger>>,
) -> base::interface::exec::ExecProviders {
    let env = config.subprocess_env.clone();
    match process_groups {
        Some(ledger) => platform::local_providers_with_ledger(env, ledger.clone()),
        None => platform::local_providers_with(env),
    }
    .with_network(Arc::new(platform::LocalNetwork::for_agent_policy(
        config.sandbox_policy.network_mode,
        config.sandbox_policy.allowed_domains.clone(),
    )))
}

/// Calls a tool makes on its session's behalf, through the same dispatch as
/// the model's own calls: a `RunCode` program's sub-calls, and a skill's
/// `` !`cmd` ``. Each runs under its own tool-use id derived from the
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
    session: &base::context::SessionState,
    caller: Caller,
) -> base::interface::permission::Attendance {
    use base::interface::permission::Attendance;
    if session.env().unattended {
        return Attendance::Unattended;
    }
    match caller {
        Caller::Model => Attendance::Attended,
        Caller::Code => Attendance::NoOneToAsk(
            "this call needs the user's approval, which a program cannot ask for — make this \
             call directly instead of from code",
        ),
        Caller::InlineCommand => Attendance::NoOneToAsk(
            "it needs the user's approval, which a skill's inline command cannot ask for; run \
             it with the Bash tool instead",
        ),
    }
}

/// What the session's sandbox holds a command to, probed the first time a
/// call of the session asks.
fn confinement_of(
    session: &base::context::SessionState,
    config: &base::context::EngineConfig,
    exec: &base::interface::exec::ExecProviders,
    cwd: &std::path::Path,
    session_id: &str,
) -> base::interface::exec::ConfinementReport {
    session.confinement(|| {
        tools::bash::confinement_report(
            &sandbox_settings_from(config),
            exec,
            config.dangerously_disable_sandbox,
            cwd,
            session_id,
        )
    })
}

/// The context a call runs with: the same for validating its input and for
/// judging it as for running it. `spec` is the called tool's; `None` where
/// no tool is called, only something of the session's run under a call's
/// context (a `/name` command's expansion, the turn's baseline).
pub(super) fn tool_context_for(
    ctx: &ToolExecCtx,
    spec: Option<&base::tool::ToolSpec>,
) -> ToolContext {
    let additional_writable_dirs = ctx.session_state.env().additional_writable_dirs.clone();
    let mut tool_ctx = ToolContext::new(
        ctx.cwd.clone(),
        ctx.cancel.clone(),
        ctx.exec.clone(),
        ctx.config.clone(),
        ctx.session_state.clone(),
    );
    tool_ctx.session_id = ctx.session_id.clone();
    tool_ctx.turn_no = ctx.turn_no;
    tool_ctx.sandbox = sandbox_settings_from(&ctx.config);
    tool_ctx.additional_writable_dirs = additional_writable_dirs;
    tool_ctx.running_tasks = Some(ctx.session_state.clone());
    // The *live* mode, not the session's opening value — plan mode is
    // entered and left mid-session.
    tool_ctx.permission_mode = ctx.session_state.permission_mode();
    tool_ctx.tool_use_id = ctx.tool_use_id.clone();
    tool_ctx.agent_depth = ctx.agent_depth;
    tool_ctx.elicitation = Some(Arc::clone(&ctx.elicitation));
    tool_ctx.dispatch = spec.is_some_and(|s| s.needs_dispatch).then(|| {
        Arc::new(SubCallDispatcher::for_code(ctx))
            as Arc<dyn base::interface::tool_dispatch::ToolDispatch>
    });
    tool_ctx.from_code = ctx.caller == Caller::Code;
    tool_ctx.confinement = confinement_of(
        &ctx.session_state,
        &ctx.config,
        &ctx.exec,
        &ctx.cwd,
        &ctx.session_id,
    );
    tool_ctx.attendance = attendance_of(&ctx.session_state, ctx.caller);
    tool_ctx.turn = ctx.turn.clone();
    tool_ctx.inline_commands = Some(Arc::new(SubCallDispatcher::for_inline_commands(ctx)));
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
            permission: self.gate.permission().clone(),
            session_state: Arc::clone(&self.session_state),
            elicitation: Arc::clone(self.gate.elicitation()),
            tool_middleware: Arc::clone(self.tooling.tool_middleware()),
            result_transformers: Arc::clone(self.tooling.result_transformers()),
            discontinued: Arc::clone(discontinued),
            images: Arc::clone(images),
            caller: Caller::Model,
            events: Some(self.channels.event_tx.clone()),
            turn: Arc::clone(&self.turn.scope),
        }
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
