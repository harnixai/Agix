//! Engine struct and builder — the central AGENT orchestrator.

use base::event::AgentEvent;
use base::interface::model::Model;
use base::interface::permission::PermissionSet;
use base::interface::scene::SceneSpec;
use base::tool::InMemoryToolRegistry;
use compaction::compact::{Compactor, DefaultCompactor};
use hooks::{HookEvent, HookRunner};
use memory::MemoryStore;
use session::session_memory::SessionMemory;
use session::transcript::Transcript;
use settings::LoadedSettings;
use std::path::PathBuf;
use std::sync::Arc;
use telemetry::perf::PerfCollector;
use telemetry::{TelemetryHandle, TelemetryRecorder};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing;

mod builder;
mod commands;
mod instruments;
mod messages;
mod run;
mod tooling;
pub mod turn;
mod view;
use crate::turn::{Compaction, Gate, Memory, ModelRoute, PromptContext};
pub use builder::{default_run_slots, Builder, HostContracts, HostLayers, HostSkillDirs};
use instruments::Telemetry;
pub use messages::*;
#[cfg(test)]
pub(crate) use tooling::sync_mcp_adapters;
use tooling::Tooling;

/// The `task_models` key that routes `WebSearch` to a provider of its own.
const WEB_SEARCH_TASK: &str = "web_search";

/// The `task_models` key that turns on `WebFetch` page extraction.
const WEB_FETCH_TASK: &str = "web_fetch";

/// The one tool in `NEEDS_A_USER` an attended session also needs a host for:
/// see [`Builder::question_prompts`].
const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// What a session's end sets off. Its tools close over the session — the
/// `Agent` tool over the registry it sits in, the team tools over the spawner
/// that reaches it — so the registry is emptied here, or none of it would ever
/// be freed; whatever outlives a turn on the session's behalf (team members)
/// watches `ended`; and the engine takes itself off the session's state.
struct SessionEnd {
    ended: CancellationToken,
    /// The token [`Agent::run`] was handed: cancelled once the engine is
    /// being stopped, while `ended` waits for it to have stopped.
    stopping: CancellationToken,
    tools: Arc<InMemoryToolRegistry>,
    state: Arc<session_state::SessionState>,
    attachment: session_state::EngineAttachment,
    reason: EndReasonSlot,
}

impl Drop for SessionEnd {
    fn drop(&mut self) {
        self.ended.cancel();
        self.tools.clear();
        self.state.detach(self.attachment.generation());
    }
}

pub struct Agent {
    scene: Arc<SceneSpec>,
    settings: Arc<LoadedSettings>,
    /// Derived from `settings`; every tool call's `ToolExecCtx.config` is a
    /// clone of this, so tools see the configured sandbox/permission/shell
    /// settings rather than defaults.
    config: Arc<base::context::EngineConfig>,
    session: Transcript,
    /// Distinct from `session`: `Transcript` owns the *transcript*, this
    /// owns everything tools read and write across calls. One instance for the
    /// whole session, handed to every `ToolContext` — see `Builder::build()`.
    session_state: Arc<session_state::SessionState>,
    /// How this session's tool calls reach the machine: built once from
    /// `config` and the builder's process-group ledger, and bound to
    /// `session_state` for the permission that judges those calls.
    exec: base::interface::exec::ExecProviders,
    /// How the engine runs its own git and helpers for this session, with
    /// the session's temporary directory: made once, at build.
    engine_git: Arc<base::engine_exec::EngineExec>,
    /// Outlives the turn it names: what is announced between turns (a
    /// `/compact`, a turn error) carries the last turn's id.
    current_turn_id: String,
    session_end: SessionEnd,
    identity: Identity,
    llm: ModelRoute,
    tooling: Tooling,
    gate: Gate,
    prompt: PromptContext,
    policies: Policies,
    memory: Memory,
    compaction: Compaction,
    turn: crate::turn::TurnState,
    /// Read by the *next* turn, to decide whether to rescan for skills; starts
    /// `true` so the first turn scans.
    last_had_tool_uses: bool,
    /// Keyed by `session.turn_count`, which only a completed turn advances, so
    /// a cancelled turn's counts carry into the next.
    repeat_guard: crate::repeat_guard::RepeatGuard,
    telemetry: Telemetry,
    channels: Channels,
}

/// Who this session is among others: its place in delegation and teams.
pub(crate) struct Identity {
    agent_depth: u32,
    agent_type: Option<String>,
    team_seat: Option<base::interface::delegate::TeamSeat>,
    drive_goals: bool,
    skip_warmup: bool,
    attended: bool,
    start_source: Option<StartSource>,
}

impl Identity {
    pub(crate) fn attended(&self) -> bool {
        self.attended
    }

    pub(crate) fn agent_depth(&self) -> u32 {
        self.agent_depth
    }

    pub(crate) fn agent_type(&self) -> Option<&str> {
        self.agent_type.as_deref()
    }

    pub(crate) fn team_seat(&self) -> Option<&base::interface::delegate::TeamSeat> {
        self.team_seat.as_ref()
    }
}

/// Pluggable answers to "how long", "how much" and "is it healthy".
pub(crate) struct Policies {
    turn_policy: Arc<dyn base::interface::turn_policy::TurnPolicy>,
    budget_policy: Arc<dyn base::interface::budget_policy::BudgetPolicy>,
    health: Arc<base::interface::health::HealthChecks>,
    environment: Arc<dyn base::interface::environment::Environment>,
}

impl Policies {
    pub(crate) fn turn_policy(&self) -> &dyn base::interface::turn_policy::TurnPolicy {
        self.turn_policy.as_ref()
    }

    pub(crate) fn budget_policy(&self) -> &dyn base::interface::budget_policy::BudgetPolicy {
        self.budget_policy.as_ref()
    }

    pub(crate) fn environment(&self) -> &Arc<dyn base::interface::environment::Environment> {
        &self.environment
    }
}

/// How input reaches this session and events leave it.
pub(crate) struct Channels {
    pub(crate) input_rx: InputReceiver,
    pub(crate) event_tx: EventSender,
    /// The turns handed to the turn loop, registered when their `User`
    /// message is queued and cancelled by the input demultiplexer, which
    /// holds no `&mut self`.
    pub(crate) current_turn_cancel: Arc<std::sync::Mutex<TurnCancel>>,
    pub(crate) steering: crate::steering::SharedSteerQueue,
    /// Host calls awaiting the host's answer, which the input
    /// demultiplexer resolves.
    pub(crate) pending_host_calls: crate::host_tools::PendingHostCalls,
    pub(crate) pending_host_progress: crate::host_tools::PendingHostProgress,
    pub(crate) host_calls: Arc<dyn base::interface::host_call::HostCalls>,
}

/// Which turn `EngineCommand::CancelTurn` lands on: the one running, else
/// the next queued; by id, the turn of that id, running or queued.
#[derive(Default)]
pub(crate) struct TurnCancel {
    running: Option<(String, CancellationToken)>,
    /// Queued, not yet started, in the order they were queued.
    waiting: std::collections::VecDeque<(String, CancellationToken)>,
    /// Turns cancelled by id before their message was queued.
    cancelled_ahead: std::collections::VecDeque<String>,
}

impl TurnCancel {
    const AHEAD_KEPT: usize = 16;

    pub(crate) fn queued(&mut self, turn_id: &str, token: CancellationToken) {
        if let Some(at) = self.cancelled_ahead.iter().position(|id| id == turn_id) {
            self.cancelled_ahead.remove(at);
            token.cancel();
        }
        self.waiting.push_back((turn_id.to_string(), token));
    }

    pub(crate) fn started(&mut self, turn_id: &str) {
        if let Some(turn) = self.take_waiting(turn_id) {
            self.running = Some(turn);
        }
    }

    /// A queued turn that will not run after all.
    pub(crate) fn dropped(&mut self, turn_id: &str) {
        self.take_waiting(turn_id);
    }

    pub(crate) fn cancel(&mut self, turn_id: Option<&str>) {
        let target = match turn_id {
            None => self.running.as_ref().or(self.waiting.front()),
            Some(id) => self
                .running
                .iter()
                .chain(self.waiting.iter())
                .find(|(turn, _)| turn == id),
        };
        match (target, turn_id) {
            (Some((_, token)), _) => token.cancel(),
            (None, Some(id)) => {
                if self.cancelled_ahead.len() == Self::AHEAD_KEPT {
                    self.cancelled_ahead.pop_front();
                }
                self.cancelled_ahead.push_back(id.to_string());
            }
            (None, None) => {}
        }
    }

    pub(crate) fn ended(&mut self, turn_id: &str) {
        if self.running.as_ref().is_some_and(|(id, _)| id == turn_id) {
            self.running = None;
        }
    }

    fn take_waiting(&mut self, turn_id: &str) -> Option<(String, CancellationToken)> {
        let at = self.waiting.iter().position(|(id, _)| id == turn_id)?;
        self.waiting.remove(at)
    }
}

impl Agent {
    pub(crate) fn emit(&self, event: AgentEvent) {
        let _ = self.channels.event_tx.send(event);
    }
}

pub(crate) fn lock_turn_cancel(
    cancel: &std::sync::Mutex<TurnCancel>,
) -> std::sync::MutexGuard<'_, TurnCancel> {
    cancel
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine shutdown requested")]
    Shutdown,
    #[error("{0}")]
    Internal(String),
    /// The host tools given to [`Builder::host_tools`] cannot be declared as
    /// they are: one is malformed, or its name is taken.
    #[error("{0}")]
    HostTools(String),
    /// The session asked for structured output ([`Builder::output_schema`])
    /// and its scene's deny list takes the tool it is handed back with.
    #[error("{0}")]
    OutputSchema(String),
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
