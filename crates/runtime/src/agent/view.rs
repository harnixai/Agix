//! `Agent` 的只读访问器：宿主和各处从引擎上取会话状态、工具表、设置、遥测等句柄。

use super::*;

impl Agent {
    pub fn steer_handle(&self) -> crate::steering::SteerHandle {
        crate::steering::SteerHandle(self.channels.steering.clone())
    }

    /// Where the host says why it stops this engine, for its `SessionEnd`
    /// hooks. Take it before spawning [`Agent::run`], as with
    /// [`Agent::commands`].
    pub fn end_reason(&self) -> EndReasonSlot {
        self.session_end.reason.clone()
    }

    /// The session's own state — its todos, tasks, permission mode — shared
    /// with every tool call, for a host that shows or acts on it.
    pub fn session_state(&self) -> Arc<base::context::SessionState> {
        self.session_state.clone()
    }

    /// Cancelled when the session ends.
    pub(crate) fn session_ended(&self) -> &CancellationToken {
        &self.session_end.ended
    }

    /// How this session delegates, for a host that starts a sub-agent itself
    /// (`DelegateMode::Host`). Refuses when no module provided one.
    pub fn delegator(&self) -> Arc<dyn base::interface::delegate::Delegator> {
        self.tooling.delegator().clone()
    }

    /// The slash commands this session will actually resolve.
    ///
    /// For a host rendering a completion popup or a command palette: this is
    /// the same registry the turn loop dispatches against, and its
    /// skill-derived half is live, so what the user is shown cannot drift
    /// from what typing it will do. Hosts that keep their own catalog to get
    /// lower-latency updates should still reconcile against this one when
    /// [`AgentEvent::SkillsChanged`] arrives.
    ///
    /// Hands out the `Arc`, not a borrow: `run()` takes `&mut self` for the
    /// life of the session, so a host that has spawned the engine has no
    /// `&Agent` left to ask. Take this before spawning and keep it.
    pub fn commands(&self) -> Arc<crate::commands::CommandRegistry> {
        Arc::clone(self.tooling.commands())
    }

    /// The diagnostics registered for this session, for a host that wants to
    /// know whether the engine is well.
    ///
    /// Hands out the `Arc` for the same reason [`Agent::commands`] does:
    /// `run()` holds `&mut self` for the life of the session, so a host that
    /// has spawned the engine has no `&Agent` left to ask. Take this before
    /// spawning, and call [`HealthChecks::report`] whenever something asks —
    /// a status endpoint, a supervisor, a log line every minute.
    ///
    /// [`HealthChecks::report`]: base::interface::health::HealthChecks::report
    pub fn health(&self) -> Arc<base::interface::health::HealthChecks> {
        Arc::clone(&self.policies.health)
    }

    pub fn perf(&self) -> &PerfCollector {
        self.telemetry.perf()
    }

    pub fn tools(&self) -> &dyn base::tool::ToolRegistry {
        self.tooling.tools().as_ref()
    }

    /// The tools this session advertises by name only — see
    /// `EngineConfig::deferred_tools`.
    pub fn deferred_tools(&self) -> &std::collections::HashSet<String> {
        &self.config.deferred_tools
    }

    pub fn scene(&self) -> &Arc<SceneSpec> {
        &self.scene
    }

    pub fn settings(&self) -> &LoadedSettings {
        &self.settings
    }

    pub fn telemetry(&self) -> &dyn TelemetryRecorder {
        self.telemetry.handle()
    }

    /// Recorder status — None when no recording is configured.
    pub fn recorder(&self) -> Option<&base::recorder::RecorderConfig> {
        self.settings.values.recorder.as_ref()
    }

    /// The session's permission: its policy and its grants.
    pub fn permission(&self) -> &PermissionSet {
        self.gate.permission()
    }

    pub fn memory(&self) -> &MemoryStore {
        self.memory.store()
    }

    pub fn skills(&self) -> &dyn base::interface::skill_catalog::SkillCatalog {
        self.tooling.skills().as_ref()
    }

    pub fn hooks(&self) -> &HookRunner {
        self.gate.hooks()
    }

    pub fn mcp(&self) -> &dyn base::interface::mcp_servers::McpServers {
        self.tooling.mcp()
    }
}
