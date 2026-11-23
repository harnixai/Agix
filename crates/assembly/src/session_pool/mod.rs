//! SessionPool —— daemon 多 session 实例管理器。
//!
//! 每个 session 对应一个独立的 Agent 实例（后台 run loop + 独立 event channel）。
//! 池本身是门面：状态按所有权分给 `parts/` 里的协作者（在内存的会话、规格、
//! 绑定、场景、MCP、磁盘上的会话、造引擎、定时任务、守护进程事件），`facade/`
//! 里的方法在锁外编排它们。对外只用 `api/` 的领域类型。组成与锁序见
//! docs/ARCHITECTURE.md「会话池的组成与锁序」。

use base::event::AgentEvent;
use base::id::Id;
use base::interface::permission::{PermissionPolicy, PermissionSet};
use base::interface::scene::SceneSpec;
use base::recorder::{RecorderConfig, RecorderMode};
use model::client::AnthropicClient;
use runtime::agent::{
    Builder, EndReason, EngineCommand, EventReceiver, InputMessage, InputSender, StartSource,
};
use settings::{LoadedSettings, Settings};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub mod api;
mod builder;
mod facade;
#[cfg_attr(feature = "mcp", path = "parts/mcp_hub/live.rs")]
#[cfg_attr(not(feature = "mcp"), path = "parts/mcp_hub/off.rs")]
mod mcp_hub;
mod parts;
mod pool_telemetry;
#[cfg(test)]
mod tests;

use api::{
    emitted_outside_turns, AskKind, Busy, Initiator, PoolError, SessionEvent, SessionFrame,
    SessionOptions, SessionWatcher, Subscribed, TurnOutcome, TurnRequest, WatcherId,
};
use facade::helpers::*;
use facade::history_rpc::*;
use facade::settings_reload::*;
use mcp_hub::McpHub;
pub(crate) use mcp_hub::McpSetup;
pub use mcp_hub::{McpOptions, COMPILED as MCP_COMPILED};
pub(crate) use parts::bindings::SessionSkillDirs;
use parts::bindings::{
    BindingCache, Cached, HostDirTable, HostOverrides, ManagedLayer, ProjectBinding,
    ProjectSceneKey,
};
use parts::directory::SessionDirectory;
use parts::events::DaemonEvents;
use parts::factory::{
    effective_permission_mode, BuildRequest, BuiltEngine, EngineFactory, SessionNamer,
};
use parts::hub::SessionHub;
use parts::live::{LiveSession, LiveSessions, SessionCore};
use parts::phase::{Generation, RunningTurn, TurnSlot, TurnTicket};
use parts::redact::*;
use parts::scenes::SceneCatalog;
use parts::scheduler::{Scheduler, TurnLauncher};
use parts::settings_write::*;
use parts::specs::{SessionSpec, SpecStore};
use pool_telemetry::*;

/// Outcome of comparing a session's recorded `Meta.scene` (if any) against
/// every scene this daemon currently serves (`SceneCatalog::is_active`) —
/// see docs/ARCHITECTURE.md §3.1.
/// Resume/fork/close/delete don't yet take a per-request `scene` param
/// (every caller implicitly means "whichever active scene this session was
/// actually recorded under"), so `Mismatch`'s reported `requested_scene`
/// is always this pool's *default* scene — that only reflects what a
/// mismatch response echoes back, not what counts as a match.
enum SceneCheck {
    /// Recorded scene matches — proceed normally.
    Matches,
    /// Recorded scene differs — callers should reject with
    /// `SCENE_MISMATCH` before touching anything.
    Mismatch(String),
}

/// `session.create`'s `project_root` disambiguated three ways — JSON only
/// gives "key omitted" vs. "key present" for free; `null` vs. a string
/// needs an explicit check the wire layer (`server/`) does before this
/// ever reaches `SessionPool`.
pub enum ProjectSelector {
    /// `project_root` omitted — this pool's default project (`self.cwd()`).
    Default,
    /// `project_root: null` — a genuine no-project session (global tier).
    NoProject,
    /// `project_root: "<path>"` — must exist; `session.create` rejects it
    /// otherwise with `PROJECT_NOT_FOUND` (unlike resume, which tolerates a
    /// moved/missing project — see `settings_for_project`'s doc comment).
    Path(PathBuf),
}

/// What a [`SessionCustomizer`] is told about the session being built.
pub struct SessionCtx<'a> {
    pub session_id: &'a str,
    pub scene: &'a str,
    /// `None` for a no-project session.
    pub project_root: Option<&'a Path>,
    /// Whether the session already has a history this build continues —
    /// a resume, or a rebuild after a settings change or an eviction.
    pub resumed: bool,
}

/// A host's last word on every session engine the pool builds: it is handed
/// the configured [`Builder`] and returns the one to build. Middleware, event
/// sinks, an elicitation handler — whatever `runtime` exposes on `Builder`
/// and the daemon does not.
///
/// Applied on every build of a session, not only the first: a rebuild after a
/// settings change, an eviction or a restart goes through it again, so what
/// it adds is never quietly missing from the rebuilt engine.
pub type SessionCustomizer = Arc<dyn Fn(Builder, &SessionCtx<'_>) -> Builder + Send + Sync>;

/// How long a stopping Agent gets to finish its turn and persist it.
const AGENT_EXIT_GRACE: Duration = Duration::from_secs(5);

/// How long a session's engine gets to write what its log lacks before the
/// session is rebuilt.
const LOG_CATCH_UP_WAIT: Duration = Duration::from_secs(30);

/// How long a process group left behind by an ended session gets on
/// `SIGTERM` before `SIGKILL`.
const PROCESS_GROUP_GRACE: Duration = Duration::from_millis(500);

/// What `session.run_turn` does when the session is already running a turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnBusy {
    /// Refuse with `SESSION_BUSY`; the message is not delivered.
    #[default]
    Reject,
    /// Wait for the running turn to end, then run this one.
    Queue,
    /// Hand the message to the running turn, which adds it at its next step
    /// boundary. Falls back to `Queue` when the turn is already past its
    /// last step, or when the message has attachments.
    Steer,
}

/// Which part of a task's output `session.taskOutput` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutputRead {
    /// The last this many bytes.
    Tail(usize),
    /// From this byte of everything the work printed to the end.
    From(u64),
}

const ENGINE_NS_PREFIX: &str = "agix.";

/// What `session.respondToPrompt` delivers, by `prompt_type`.
#[derive(Debug, Clone)]
pub enum PromptAnswer {
    Permission(runtime::agent::PermissionDecision),
    Question(runtime::agent::QuestionAnswer),
    McpElicitation(base::interface::elicitation::McpElicitationAnswer),
}

impl PromptAnswer {
    pub fn kind(&self) -> AskKind {
        match self {
            Self::Permission(_) => AskKind::Permission,
            Self::Question(_) => AskKind::Question,
            Self::McpElicitation(_) => AskKind::McpElicitation,
        }
    }
}

pub struct SessionPool {
    /// This pool, for the tasks it starts that outlive the call that started
    /// them. Set by `Arc::new_cyclic` in the builder, so always there.
    this: std::sync::Weak<SessionPool>,
    /// The sessions in memory, and what brings them up and takes them down.
    live: LiveSessions,
    /// Each session's [`SessionSpec`]: what every rebuild of it applies.
    specs: SpecStore,
    /// This daemon's own settings, every `(project, scene)` pair's binding,
    /// and the watch on the files they come from.
    bindings: BindingCache,
    /// The default scene, the scenes served now, and every scene this pool
    /// can serve.
    scenes: SceneCatalog,
    /// MCP: the connections per `(project, scene)`, sign-ins and the policy
    /// servers are admitted by. A stub in a build without the `mcp` module.
    mcp: McpHub,
    /// The plugin subsystem — see `crate::plugins`. Everything plugins
    /// contribute arrives through it, and in a build without the `plugins`
    /// feature it is a stub that contributes nothing.
    plugins: Arc<crate::plugins::PluginSubsystem>,
    /// The sessions on disk: what their logs record, and the projects this
    /// pool has served.
    directory: SessionDirectory,
    /// Builds each session's engine.
    factory: EngineFactory,
    /// Every session's scheduled jobs, fired as turns of their sessions
    /// whether or not the session is in memory.
    scheduler: Scheduler,
    /// Daemon-level notifications (`daemon.subscribeEvents`) and the
    /// pool-level telemetry.
    events: DaemonEvents,
    /// Process-scope diagnostics, reported by `daemon.doctor` — the plugin
    /// fault records, plus whatever a plugin registers. Not the same set as a
    /// session's: a session's checks come and go with the session, and one
    /// shared registry would grow an entry per session forever.
    health: std::sync::RwLock<base::interface::health::HealthChecks>,
}

/// session.list 返回的单条记录。
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct SessionInfo {
    pub session_id: String,
    pub name: Option<String>,
    pub preview: Option<String>,
    pub message_count: u32,
    pub created_at: String,
    pub last_active: String,
    pub status: SessionStatus,
    #[schemars(with = "api::SessionKindName")]
    pub session_kind: history::entry::SessionKind,
    /// Scene the session was created under, and the project it is bound to.
    /// Both always present, `null` when the log does not record them (pre-v2
    /// files) — a client grouping by either can then tell "no project" from
    /// "not known" without a presence check.
    pub scene: Option<String>,
    pub project_root: Option<String>,
    /// How many times this session's context has been compacted. Counted as
    /// the log is written, so a session that is only on disk answers too.
    pub compact_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// `false` only for a sidechain that already ran its one-shot task to
    /// conclusion — `session.resume` against it returns `SIDECHAIN_TERMINAL`.
    pub resumable: bool,
}

#[derive(Debug, Clone, Copy, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Inactive,
}

/// What `project.status` answers for `root`, from the settings its sessions
/// are built with.
fn project_trust_report(
    root: &Path,
    settings: &LoadedSettings,
    plugin_agent_types: &[base::agent_type::AgentTypeDefinition],
) -> api::ProjectStatus {
    #[cfg_attr(not(any(feature = "agents", feature = "skills")), allow(unused_mut))]
    let mut requires_trust = settings.requires_trust();
    #[cfg(feature = "agents")]
    requires_trust.extend(agents::agent_tool::project_agent_types_requiring_trust(
        settings,
        plugin_agent_types,
    ));
    #[cfg(not(feature = "agents"))]
    let _ = plugin_agent_types;
    #[cfg(feature = "skills")]
    requires_trust.extend(skills::module::project_skills_requiring_trust(settings));
    api::ProjectStatus {
        project_root: root.to_path_buf(),
        trust: settings.trust().into(),
        matched: settings.trust_pattern().map(str::to_string),
        trust_source: settings.trust_source(),
        default: if settings.values.app.trust_all_projects {
            settings::trust::TrustLevel::Trusted
        } else {
            settings::trust::TrustLevel::Untrusted
        },
        requires_trust,
    }
}

impl SessionPool {
    /// Subscribe to daemon-level async notifications (MCP connect
    /// outcomes, import auto-detection, ...) — see `daemon.subscribeEvents`.
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<api::DaemonEvent> {
        self.events.subscribe()
    }

    /// Publish a daemon-level notification to every current subscriber. A
    /// `send` error just means nobody's listening right now — not a real
    /// failure, so this is intentionally infallible from the caller's side.
    pub fn emit_event(&self, event: api::DaemonEvent) {
        self.events.emit(event);
    }

    /// Publish the `import_detected` question and keep it for subscribers
    /// that connect later, until an import answers it.
    pub fn ask_import(&self, event: api::DaemonEvent) {
        self.events.ask_import(event);
    }

    /// The `import_detected` question still waiting for an answer.
    pub fn pending_import_question(&self) -> Option<api::DaemonEvent> {
        self.events.import_question()
    }

    /// [`Self::subscribe_events`] with the `import_detected` question still
    /// waiting, which the subscriber gets once either way.
    pub fn subscribe_events_with_question(
        &self,
    ) -> (
        tokio::sync::broadcast::Receiver<api::DaemonEvent>,
        Option<api::DaemonEvent>,
    ) {
        self.events.subscribe_with_question()
    }

    /// Record how long the daemon took to start, in the pool-level
    /// telemetry.
    pub fn record_startup(&self, timing: telemetry::StartupTimingPayload) {
        self.events.record_startup(timing);
    }

    /// The project directory this pool's sessions run in.
    pub fn cwd(&self) -> &Path {
        self.bindings.cwd()
    }

    pub(crate) fn layout(&self) -> base::paths::Layout {
        self.bindings.paths().layout()
    }

    /// The `RunCode` budget this pool's sessions share: how many programs
    /// their conversations may run at once. Other pools have their own.
    pub fn run_slots(&self) -> Arc<tokio::sync::Semaphore> {
        self.factory.run_slots().clone()
    }

    /// Full slash command catalog — built-in locals + skill-derived
    /// (see `build_shared_commands`), and plugin commands when the daemon's
    /// scene has `RunExt` to run them — for the
    /// `commands.list` RPC. Executing one is not a separate RPC: send
    /// `/name args` as the `message` of `session.run_turn`, which already
    /// intercepts and runs it (see `runtime::turn::process_turn`).
    pub async fn list_commands(&self) -> Vec<api::CommandInfo> {
        let scene = self.bindings.startup_scene().await;
        let mut commands = self
            .binding_for(Some(self.cwd()), &scene)
            .await
            .commands
            .list_detailed();
        let runs_ext = self
            .scenes
            .resolve(Some(&scene))
            .is_some_and(|spec| runtime::commands::scene_runs_ext_commands(&spec));
        if runtime::PTC_ENABLED && runs_ext {
            for command in self.plugins.commands() {
                if commands.iter().any(|c| c.name == command.name) {
                    continue;
                }
                commands.push(runtime::commands::CommandInfo {
                    name: command.name,
                    description: command.description,
                    kind: "ext",
                    source: "plugin",
                    tier: None,
                    dir: None,
                });
            }
            commands.sort_by(|a, b| a.name.cmp(&b.name));
        }
        commands.into_iter().map(api::CommandInfo::from).collect()
    }

    /// The slash commands `session_id` resolves: its scene's built-ins, its
    /// host skill directories and its MCP prompts with the rest. Only a
    /// session in memory has them; one out of it is `SessionNotFound` until
    /// it is brought back.
    pub async fn list_session_commands(
        &self,
        session_id: &str,
    ) -> Result<Vec<api::CommandInfo>, PoolError> {
        let commands = self
            .live
            .with_live(session_id, |live| live.engine.commands.clone())
            .await;
        match commands {
            Some(commands) => Ok(commands
                .list_detailed()
                .into_iter()
                .map(api::CommandInfo::from)
                .collect()),
            None if self.session_exists(session_id).await => Err(PoolError::session_gone(
                session_id,
                format!("session `{session_id}` is not in memory; session.resume it first"),
            )),
            None => Err(PoolError::session_not_found(session_id)),
        }
    }
}

/// A session in `Rebuilding`, the entry of `generation`.
struct HeldSlot {
    generation: Generation,
}

/// Why [`SessionPool::rebuild_idle`] did not rebuild.
enum Rebuild {
    NotLive,
    Busy(Busy),
    Failed(CreateError),
}

/// Why a session was not brought up under its id.
#[derive(Debug)]
pub(super) enum CreateError {
    /// The engine the id last ran on is still running past
    /// [`AGENT_EXIT_GRACE`]; a second one would write the same transcript.
    PreviousEngineRunning {
        session_id: String,
        message: String,
    },
    /// The session's log lacks some of what the session holds, and its engine
    /// could not write it: rebuilding from the log would lose it.
    PersistFailed(String),
    /// The session's scene was deactivated while the session was built.
    SceneNotActive(String),
    /// Entries of the session's `options` that cannot stand, each named.
    InvalidOptions(Vec<api::InvalidOption>),
    /// The session's history is kept in a project not trusted now.
    ProjectUntrusted(String),
    /// The pair's settings define providers and none of them routes.
    Unroutable(String),
    Failed(String),
}

impl From<String> for CreateError {
    fn from(e: String) -> Self {
        Self::Failed(e)
    }
}

impl From<PoolError> for CreateError {
    fn from(e: PoolError) -> Self {
        match e {
            PoolError::ProjectUntrusted(m) => Self::ProjectUntrusted(m),
            PoolError::Internal(m) => Self::Failed(m),
            other => Self::Failed(other.to_string()),
        }
    }
}

impl CreateError {
    pub(super) fn previous_engine_running(sid: &str) -> Self {
        Self::PreviousEngineRunning {
            session_id: sid.to_string(),
            message: format!(
                "session `{sid}` is still stopping on its previous engine; try again once it has"
            ),
        }
    }

    pub(super) fn context(self, outer: impl FnOnce(String) -> String) -> Self {
        match self {
            Self::PreviousEngineRunning {
                session_id,
                message,
            } => Self::PreviousEngineRunning {
                session_id,
                message: outer(message),
            },
            Self::PersistFailed(e) => Self::PersistFailed(outer(e)),
            Self::SceneNotActive(scene) => Self::SceneNotActive(scene),
            Self::InvalidOptions(invalid) => Self::InvalidOptions(invalid),
            Self::ProjectUntrusted(e) => Self::ProjectUntrusted(outer(e)),
            Self::Unroutable(e) => Self::Unroutable(outer(e)),
            Self::Failed(e) => Self::Failed(outer(e)),
        }
    }
}

impl From<CreateError> for PoolError {
    fn from(e: CreateError) -> Self {
        match e {
            CreateError::PreviousEngineRunning {
                session_id,
                message,
            } => Self::SessionBusy(Busy::previous_engine(&session_id, message)),
            CreateError::PersistFailed(e) => Self::Engine(base::event::TurnFailure::new(
                base::event::FailureKind::PersistFailed,
                e,
            )),
            CreateError::SceneNotActive(scene) => Self::SceneNotActive { scene },
            CreateError::InvalidOptions(invalid) => Self::InvalidOptions(invalid),
            CreateError::ProjectUntrusted(e) => Self::ProjectUntrusted(e),
            CreateError::Unroutable(e) => Self::InvalidRequest(e),
            CreateError::Failed(e) => Self::Internal(e),
        }
    }
}

impl Rebuild {
    fn into_error(self, session_id: &str) -> PoolError {
        match self {
            Rebuild::Busy(busy) => PoolError::SessionBusy(busy),
            Rebuild::NotLive => PoolError::session_gone(session_id, "session is no longer running"),
            Rebuild::Failed(e) => e.into(),
        }
    }
}

/// What a host asks of a session's goal.
#[derive(Debug, Clone)]
pub enum GoalChange {
    Set {
        objective: String,
        max_rounds: Option<u32>,
        token_budget: Option<u64>,
    },
    Edit {
        objective: Option<String>,
        max_rounds: Option<u32>,
        token_budget: Option<u64>,
    },
    Pause,
    Resume,
    Clear,
}

/// Run `work` — disk reads and directory scans — on the blocking pool rather
/// than on a runtime worker.
async fn off_the_runtime<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => value,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => panic!("blocking work did not finish: {e}"),
    }
}
