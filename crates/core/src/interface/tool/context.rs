//! `ToolContext`: everything a tool call may read about its session and turn.

use super::*;

#[derive(Clone)]
#[non_exhaustive]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub session_id: String,
    pub turn_no: u32,
    pub cancel: CancellationToken,
    /// The engine's configuration; the session's sandbox settings are its
    /// `sandbox`.
    pub config: Arc<crate::context::EngineConfig>,
    /// The session the call is made in: its live permission mode, the
    /// directories it may write besides its own, its running tasks.
    pub session: Arc<dyn crate::context::Session>,
    pub tool_use_id: String,
    /// How many delegations deep the calling session is; `0` for the
    /// conversation the user talks to.
    pub agent_depth: u32,
    /// How a tool asks the user something — see
    /// [`crate::interface::elicitation::Elicitation`]. `None` where nothing
    /// can reach a person (a test context, a background run), which every
    /// asking tool must read as a refusal rather than as permission to make
    /// the answer up.
    pub elicitation: Option<Arc<dyn crate::interface::elicitation::Elicitation>>,
    /// How a host-declared tool reaches the host; `None` where no host can
    /// answer, which such a tool reads as failing at once.
    pub host_calls: Option<Arc<dyn crate::interface::host_call::HostCalls>>,
    /// How this tool reaches the machine — see
    /// [`crate::interface::exec`]. In production this machine's
    /// (`platform`); a deployment executing somewhere else replaces the set
    /// and no tool notices.
    pub exec: crate::interface::exec::ExecProviders,
    /// How this tool calls other tools — set only for a tool whose
    /// [`ToolSpec::needs_dispatch`] says it does.
    pub dispatch: Option<Arc<dyn crate::interface::tool_dispatch::ToolDispatch>>,
    /// The call comes from a program (`RunCode`), not from the model: there
    /// is nobody to ask and no conversation to report back to mid-call.
    pub from_code: bool,
    /// Who made the call, as `tool.around` is told: `model`, `user`,
    /// `skill`, `RunCode`, `host`, or the plugin command or function whose
    /// call it is.
    pub via: String,
    /// What the session's sandbox holds a command to.
    pub confinement: crate::interface::exec::ConfinementReport,
    /// How the engine runs its own git and helpers for the session — see
    /// [`crate::interface::exec::SandboxSettings::engine_git`].
    pub engine_git: Arc<crate::engine_exec::EngineExec>,
    /// Whether someone can be asked about what this call does.
    pub attendance: crate::interface::permission::Attendance,
    /// What the policy let this call through on; `None` when nothing judged it.
    pub approval: Option<crate::interface::permission::Approval>,
    pub turn: Arc<TurnScope>,
    /// How a skill's `` !`cmd` `` expanded during this call runs; `None`
    /// where nothing runs them.
    pub inline_commands: Option<Arc<dyn crate::interface::inline_command::InlineCommands>>,
    /// What the permission rules in force deny of a directory walk; `None`
    /// where nothing judges paths.
    pub path_denials: Option<Arc<dyn crate::interface::permission::PathDenials>>,
}

/// What belongs to the one turn a call is made in, and ends with it.
#[derive(Debug, Default)]
pub struct TurnScope {
    /// Who started the turn, which decides what goal operations it may perform.
    pub origin: crate::context::goal::TurnOrigin,
    structured_output: std::sync::Mutex<Option<Value>>,
}

impl TurnScope {
    pub fn new(origin: crate::context::goal::TurnOrigin) -> Self {
        Self {
            origin,
            structured_output: Default::default(),
        }
    }

    pub fn set_structured_output(&self, value: Value) {
        *self
            .structured_output
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(value);
    }

    pub fn take_structured_output(&self) -> Option<Value> {
        self.structured_output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

impl ToolContext {
    pub fn new(
        cwd: PathBuf,
        cancel: CancellationToken,
        exec: crate::interface::exec::ExecProviders,
        config: Arc<crate::context::EngineConfig>,
        session: Arc<dyn crate::context::Session>,
    ) -> Self {
        // Without the session's TMPDIR: the runtime gives every call the
        // engine git it made, with the TMPDIR, once for the session.
        let engine_git = Arc::new(config.sandbox.engine_git(&exec, config.project_trust, None));
        Self {
            cwd,
            session_id: String::new(),
            turn_no: 0,
            cancel,
            config,
            session,
            tool_use_id: String::new(),
            agent_depth: 0,
            elicitation: None,
            host_calls: None,
            exec,
            dispatch: None,
            from_code: false,
            via: "model".into(),
            confinement: Default::default(),
            engine_git,
            attendance: crate::interface::permission::Attendance::Attended,
            approval: None,
            turn: Arc::default(),
            inline_commands: None,
            path_denials: None,
        }
    }

    /// This context's own sandbox settings, to change for it alone: the
    /// session's stay as they are.
    pub fn sandbox_mut(&mut self) -> &mut crate::interface::exec::SandboxSettings {
        Arc::make_mut(&mut Arc::make_mut(&mut self.config).sandbox)
    }

    /// Whether the call is a delegate's — a sub-agent's or a team member's —
    /// rather than the conversation's own.
    pub fn in_delegate(&self) -> bool {
        self.agent_depth > 0
    }
}
