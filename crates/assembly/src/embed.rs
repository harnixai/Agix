//! 嵌入入口：`agixd` 的命令行参数进、协议连接出。一个宿主在自己的进程里起引擎，
//! 在任意字节流或一个 unix socket 上服务协议，除这里的三个类型外不碰 Agix 的
//! 任何其它类型；`agixd` 自己就建在它上面。
//!
//! 这是 0.x 里单独承诺稳定的面：`EmbedConfig` 只加带缺省值的字段（`#[non_exhaustive]`
//! 加 [`EmbedConfig::new`]），`EmbedError` 只加变体，`Embedded` 的方法签名不变，改动写进
//! `docs/agix-sdk.md` 的迁移说明；`tests/guards/tests/embed_surface.rs` 守着它。
//!
//! 数据根、套接字、策略文件只从 `EmbedConfig` 来，不读本进程的环境变量；凭据仍按
//! `ANTHROPIC_*` 与 provider 设置找。不装 tracing subscriber、不装信号处理、不退出进程，
//! 用调用方的 tokio runtime。仍是进程级的几样写在 `docs/agix-sdk.md`。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base::interface::host_js::HostJsLib;
use base::interface::scene::SceneSpec;
pub use settings::HistoryLocation;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::{load_daemon_config, DaemonConfig, StaticDaemonPaths};
use crate::discovery::{self, DaemonLockGuard, InstanceFile};
use crate::server::DaemonServer;
use crate::session_pool::SessionPool;

/// How long [`Embedded::shutdown`] waits for sessions to stop.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// What to start: `agixd`'s command line as fields.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct EmbedConfig {
    /// The state root (`AGIX_CONFIG_HOME` to `agixd`): global settings,
    /// scenes, transcripts, plugins. Read from here only.
    pub data_root: PathBuf,
    /// The project a session belongs to when it names none; `None` is this
    /// process's working directory.
    pub cwd: Option<PathBuf>,
    /// The scene sessions default to; also selects `<data_root>/scenes/<scene>/`.
    pub scene: String,
    /// Further scenes to activate.
    pub scenes: Vec<String>,
    pub model: String,
    /// `model.max_tokens` in `settings`, over what `settings` itself says
    /// there.
    pub max_tokens: Option<u32>,
    /// `permissions.prompt_timeout_secs` in `settings`, in whole seconds,
    /// over what `settings` itself says there.
    pub permission_prompt_timeout: Option<Duration>,
    /// Settings over every settings file of every project and scene, as
    /// `agixd --set` gives them: a JSON object of settings keys, the `cli`
    /// layer. It merges as a global tier's file would, `app` included (the
    /// session cap is `app.session_cap`), and its `permissions` rule lists add
    /// to every pair's. A key no setting reads, an old one, or a value that
    /// does not fit fails the start with [`EmbedError::InvalidSetting`].
    pub settings: Option<serde_json::Value>,
    /// `Some`: take the scene's `daemon.lock` and write the `instances.d`
    /// entry under this name; without a `groups_file`, record the commands
    /// sessions leave running at `<data_root>/daemon/groups/<name>.json`.
    /// `None`: no lock and no entry — the host owns its own exclusion.
    pub instance: Option<String>,
    /// Where [`Embedded::serve_unix`] listens; `None` is
    /// `<data_root>/scenes/<scene>/daemon.sock`.
    pub socket: Option<PathBuf>,
    /// The MCP policy file in place of the platform's; it must exist.
    pub mcp_policy: Option<PathBuf>,
    /// The managed settings file in place of the platform's; it must exist.
    pub managed_settings: Option<PathBuf>,
    /// Where to record the process groups sessions leave running, so that the
    /// next start with the same file stops what a killed run left behind;
    /// with or without an `instance`.
    pub groups_file: Option<PathBuf>,
    /// The name of the directory the engine keeps in each project; `None`
    /// is `.<engine>`: the project's settings, agents, hooks, rules, memory.
    /// Only this name is read; a `.agix` beside it is not, and stays guarded
    /// against the model as another agent's configuration. One path
    /// component of `[a-z0-9._-]` starting with `.`, not another tool's
    /// (`base::paths::layout::OTHER_TOOLS_DIRS`) nor the worktrees
    /// directory's default (`base::paths::DEFAULT_WORKTREES_DIR`).
    pub project_dir: Option<String>,
    /// The product's name (`Agix`), wherever the engine names itself: the
    /// model's prompts and the messages it reads, the `client_name` an MCP
    /// authorization server shows, the `User-Agent` of its requests, the
    /// branches of its worktrees and the directory its managed settings and
    /// MCP policy are read from. Only this name's managed files are read.
    /// An ASCII letter, then up to 31 ASCII letters, digits, `-` or `_`,
    /// whose lowercase form is none of the names the engine keeps in a
    /// project's `state/` (`base::paths::STATE_RESERVED_NAMES`):
    /// `state/<that form>/` is the host's (`base::paths::Layout::host_state_dir`).
    pub product: String,
    /// The engine's name in scripts and file names (`agix`): the global the
    /// script base library is under, its errors' name (`<Engine>Error`), the
    /// project directory's default and the state directory under `$HOME`
    /// that is guarded as this engine's. `[a-z][a-z0-9]*`, at most 16 long,
    /// no JavaScript reserved word and no global a script already has.
    pub engine: String,
    /// The host's own plugin packages, each an unpacked package directory
    /// with its `plugin.toml`: loaded at start like an installed package,
    /// listed with `source: "host"`, beyond `plugin.uninstall` and
    /// `plugin.disable`, and loaded in place of an installed package of the
    /// same name. One that does not load fails the start.
    pub plugins: Vec<PathBuf>,
    /// The JavaScript libraries the host gives plugin code, each a global
    /// of its namespace in the commands and functions of the packages whose
    /// `uses` names it, and only there. A package naming one not here is
    /// not installed or loaded. The namespace is `[a-z][a-z0-9]*`, not the
    /// engine's name, no global a script already has and no other
    /// library's; function paths are dotted identifiers, each once.
    /// Otherwise the start fails with [`EmbedError::InvalidHostLibrary`].
    pub js_libs: Vec<HostJsLib>,
    /// Where sessions are kept, over the global settings' `app.history`;
    /// `None` leaves it to them, which is `Global` unless they say otherwise.
    /// `Project` keeps a trusted project's sessions in its project directory's
    /// `state/history/` (a worktree's in its main checkout's), with only an
    /// index under `<data_root>/sessions/`.
    pub history: Option<HistoryLocation>,
    /// Keys for the providers whose `api_key_env` names no variable the
    /// process has — for a host that keeps them in a vault or keychain.
    pub credentials: Option<HostCredentials>,
}

/// The host's credential source, as [`EmbedConfig::credentials`] holds it.
#[derive(Clone)]
pub struct HostCredentials(pub Arc<dyn base::interface::credentials::CredentialSource>);

impl std::fmt::Debug for HostCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostCredentials")
    }
}

impl EmbedConfig {
    /// `agixd`'s defaults over `data_root`.
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            cwd: None,
            scene: "code".into(),
            scenes: Vec::new(),
            model: "claude-sonnet-4-6".into(),
            max_tokens: None,
            permission_prompt_timeout: None,
            settings: None,
            instance: None,
            socket: None,
            mcp_policy: None,
            managed_settings: None,
            groups_file: None,
            project_dir: None,
            product: base::paths::DEFAULT_PRODUCT_NAME.into(),
            engine: base::paths::DEFAULT_ENGINE.into(),
            plugins: Vec::new(),
            js_libs: Vec::new(),
            history: None,
            credentials: None,
        }
    }

    /// The `cli` layer: `settings` with `max_tokens` and
    /// `permission_prompt_timeout` written in; `None` when none of them is
    /// given. `settings` that is not an object is left as it is.
    fn cli_settings(&self) -> Option<serde_json::Value> {
        let mut shorthands = serde_json::Map::new();
        if let Some(max_tokens) = self.max_tokens {
            shorthands.insert(
                "model".into(),
                serde_json::json!({ "max_tokens": max_tokens }),
            );
        }
        if let Some(timeout) = self.permission_prompt_timeout {
            shorthands.insert(
                settings::permissions::KEY.into(),
                serde_json::json!({ "prompt_timeout_secs": timeout.as_secs() }),
            );
        }
        let mut layer = match (&self.settings, shorthands.is_empty()) {
            (None, true) => return None,
            (Some(layer), _) if !layer.is_object() => return Some(layer.clone()),
            (layer, _) => layer.clone().unwrap_or_else(|| serde_json::json!({})),
        };
        settings::merge_json_values(&mut layer, serde_json::Value::Object(shorthands));
        Some(layer)
    }

    /// `groups_file`, else the named instance's own.
    fn groups_file(&self) -> Option<PathBuf> {
        self.groups_file.clone().or_else(|| {
            let name = self.instance.as_ref()?;
            Some(base::paths::StateRoot::new(&self.data_root).groups_file(name))
        })
    }
}

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("unknown scene `{scene}`; known scenes: {}", .known.join(", "))]
    UnknownScene { scene: String, known: Vec<String> },
    #[error("another daemon (pid {pid}) serves this scene at {}", .socket.display())]
    AlreadyRunning { pid: u32, socket: PathBuf },
    #[error("{0}")]
    Startup(String),
    /// The process-group record is per process, and the named instance
    /// `running` holds it.
    #[error(
        "this process already runs the named instance `{running}`; a process runs at most one"
    )]
    InstanceInProcess { running: String },
    /// The process-group record is per process, and an engine without an
    /// instance keeps it at `running`.
    #[error(
        "this process already records its process groups at {}; a process keeps one record",
        .running.display()
    )]
    GroupsInProcess { running: PathBuf },
    /// The groups file names a process that is running, or might be, as its
    /// owner: it is that process's list, not leftovers.
    #[error("{} is the process group record of the running process {owner}", .file.display())]
    GroupsHeld { owner: u32, file: PathBuf },
    /// `EmbedConfig::project_dir` cannot name a project's directory.
    #[error("project_dir `{name}`: {reason}")]
    InvalidProjectDir { name: String, reason: String },
    /// `EmbedConfig::product` cannot name the product.
    #[error("product `{name}`: {reason}")]
    InvalidProductName { name: String, reason: String },
    /// `EmbedConfig::engine` cannot name the engine.
    #[error("engine `{name}`: {reason}")]
    InvalidEngineName { name: String, reason: String },
    /// A library of `EmbedConfig::js_libs` cannot be installed.
    #[error("host library `{namespace}`: {reason}")]
    InvalidHostLibrary { namespace: String, reason: String },
    /// `EmbedConfig::settings` (with `max_tokens` and
    /// `permission_prompt_timeout` written in) holds `key`, dotted, that no
    /// setting reads or a value there that does not fit; `key` is empty when
    /// the settings are not an object.
    #[error("setting `{key}`: {reason}")]
    InvalidSetting { key: String, reason: String },
}

/// Who keeps this process's process-group record: a named instance, or the
/// file of an engine without one.
static RECORDER: std::sync::Mutex<Option<(Option<String>, PathBuf)>> = std::sync::Mutex::new(None);

/// This process's one process-group record, held until dropped.
struct RecordSlot {
    file: PathBuf,
}

impl RecordSlot {
    fn take(instance: Option<&str>, file: &Path) -> Result<Self, EmbedError> {
        let mut held = RECORDER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match held.as_ref() {
            Some((Some(running), _)) => Err(EmbedError::InstanceInProcess {
                running: running.clone(),
            }),
            Some((None, running)) => Err(EmbedError::GroupsInProcess {
                running: running.clone(),
            }),
            None => {
                *held = Some((instance.map(str::to_string), file.to_path_buf()));
                Ok(RecordSlot {
                    file: file.to_path_buf(),
                })
            }
        }
    }
}

impl Drop for RecordSlot {
    fn drop(&mut self) {
        platform::orphans::stop_persisting(&self.file);
        *RECORDER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// A running engine. `Send + Sync`; share it behind an `Arc` to serve from
/// several tasks.
pub struct Embedded {
    server: Arc<DaemonServer>,
    pool: Arc<SessionPool>,
    socket: PathBuf,
    instance: Option<Instance>,
    groups: Option<Groups>,
    stopped: tokio::sync::OnceCell<()>,
}

/// What a named instance holds while it runs.
struct Instance {
    name: String,
    scenes: Vec<String>,
    daemon_root: PathBuf,
    /// `None` once released: nothing is announced after that.
    held: std::sync::Mutex<Option<DaemonLockGuard>>,
}

/// The record of the process groups this run leaves running.
struct Groups {
    file: PathBuf,
    /// `None` once released.
    slot: std::sync::Mutex<Option<RecordSlot>>,
}

/// What the `instances.d` entry says beyond the unix socket: the listeners
/// and handshake a caller set up around [`Embedded::serve_unix`].
#[derive(Default)]
pub(crate) struct Announced {
    pub(crate) tcp: Option<std::net::SocketAddr>,
    pub(crate) ws: Option<std::net::SocketAddr>,
    pub(crate) unix_socket_auth: bool,
    pub(crate) token_file: Option<PathBuf>,
}

impl Embedded {
    /// Read the configuration, build the pool and have it serving: plugin
    /// components loaded, MCP connecting, the janitor, configuration watcher
    /// and scheduler running. No connection is served until one is handed
    /// over.
    pub async fn start(config: EmbedConfig) -> Result<Embedded, EmbedError> {
        let startup = telemetry::perf::PerfCollector::new();
        let scene = resolve_scene(
            &config.scene,
            &base::paths::StateRoot::new(&config.data_root).scenes_dir(),
            &layout_of(&config)?,
        )?;
        check_host_libraries(&config)?;
        let daemon_config = daemon_config(&config, &scene.id)?;
        let socket = daemon_config.socket_path.clone();
        let instance = match &config.instance {
            Some(name) => Some(claim(&config, name, &daemon_config)?),
            None => None,
        };
        let groups = match config.groups_file() {
            Some(file) => Some(Groups::claim(file, config.instance.as_deref()).await?),
            None => None,
        };
        let config_loaded = startup.checkpoint("config_loaded");

        let pool = crate::assemble::pool(
            &daemon_config,
            scene,
            crate::Assembly {
                cwd: Some(config.cwd.clone().unwrap_or_else(|| {
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                })),
                extra_scenes: config.scenes.clone(),
                host_plugins: config.plugins.clone(),
                js_libs: config.js_libs.clone(),
                history: config.history,
                credentials: config.credentials.as_ref().map(|c| c.0.clone()),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| EmbedError::Startup(format!("{e:#}")))?;
        let pool_built = startup.checkpoint("pool_built");
        if let Some(groups) = &groups {
            platform::orphans::persist_to(groups.file.clone());
        }
        spawn_import_detection(pool.clone());
        record_startup(&pool, &startup, config_loaded, pool_built);

        let server = Arc::new(DaemonServer::new(pool.clone(), CancellationToken::new()));
        Ok(Embedded {
            server,
            pool,
            socket,
            instance,
            groups,
            stopped: tokio::sync::OnceCell::new(),
        })
    }

    /// Serve one connection on `stream`: newline-delimited JSON-RPC with the
    /// frame cap, in-flight bound and outbound queue of a unix socket
    /// connection, without `daemon.auth`. Replies and events come back in one
    /// ordered stream. The task ends when the peer closes its side.
    pub fn serve_stream<S>(&self, stream: S) -> JoinHandle<()>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        tokio::spawn(self.server.clone().serve_byte_stream(
            Box::new(reader),
            Box::new(writer),
            false,
        ))
    }

    /// Listen on the configured socket until [`Self::closed`] — a
    /// `daemon.shutdown`, or [`Self::shutdown`]. A named instance announces
    /// itself in `instances.d` once the socket is bound. Once shutdown has
    /// begun it binds nothing and returns at once; a socket something still
    /// answers on is not taken over (`AddrInUse`).
    pub async fn serve_unix(&self) -> std::io::Result<()> {
        self.serve_unix_announcing(Announced::default()).await
    }

    pub(crate) async fn serve_unix_announcing(&self, announced: Announced) -> std::io::Result<()> {
        if self.server.shutdown_token().is_cancelled() {
            return Ok(());
        }
        let bound = DaemonServer::bind_unix(&self.socket).map_err(|e| {
            e.downcast::<std::io::Error>()
                .unwrap_or_else(std::io::Error::other)
        })?;
        if let Some(instance) = &self.instance {
            instance.announce(&self.socket, announced);
        }
        self.server
            .clone()
            .serve_unix_listener(bound)
            .await
            .map_err(std::io::Error::other)
    }

    /// Returns once the engine has begun shutting down.
    pub async fn closed(&self) {
        self.server.shutdown_token().cancelled().await
    }

    /// Stop accepting connections, stop every session (waiting up to ten
    /// seconds), then remove the process-group record and give up the lock
    /// and the entry a named instance wrote.
    /// Calling it again, or while it runs, waits for the first call.
    pub async fn shutdown(&self) {
        self.stopped
            .get_or_init(|| async {
                self.server.shutdown_token().cancel();
                if tokio::time::timeout(SHUTDOWN_GRACE, self.pool.shutdown_all())
                    .await
                    .is_err()
                {
                    warn!("sessions did not shut down within {SHUTDOWN_GRACE:?}; going on");
                }
                if let Some(groups) = &self.groups {
                    groups.release();
                }
                if let Some(instance) = &self.instance {
                    instance.release();
                }
                info!("engine stopped");
            })
            .await;
    }

    /// The session pool the protocol serves, for what only the Rust interface
    /// reaches: `SessionPool::call_plugin`, `register_host_plugin`, ….
    pub fn pool(&self) -> &Arc<SessionPool> {
        &self.pool
    }

    pub(crate) fn server(&self) -> &Arc<DaemonServer> {
        &self.server
    }

    pub(crate) fn instance_name(&self) -> Option<&str> {
        self.instance.as_ref().map(|i| i.name.as_str())
    }

    pub(crate) fn daemon_root(&self) -> Option<&Path> {
        self.instance.as_ref().map(|i| i.daemon_root.as_path())
    }
}

/// The scene `name` names: a built-in (named for `layout`'s product), or a
/// declarative scene under `user_scenes` (`<data_root>/scenes/<id>/scene.json`).
pub(crate) fn resolve_scene(
    name: &str,
    user_scenes: &Path,
    layout: &base::paths::Layout,
) -> Result<Arc<SceneSpec>, EmbedError> {
    let mut registry = scene::SceneRegistry::new();
    registry.register_builtin(layout);
    for e in registry.register_declarative_dir(user_scenes) {
        warn!(error = %e, "a declarative scene was not loaded");
    }
    registry.resolve(name).ok_or_else(|| {
        let mut known = registry.ids();
        known.sort();
        EmbedError::UnknownScene {
            scene: name.to_string(),
            known,
        }
    })
}

/// The engine, project directory and product `config` names, checked.
fn layout_of(config: &EmbedConfig) -> Result<base::paths::Layout, EmbedError> {
    let layout = base::paths::Layout::default()
        .with_engine(&config.engine)
        .map_err(|e| EmbedError::InvalidEngineName {
            name: e.name,
            reason: e.reason.to_string(),
        })?;
    let layout = match &config.project_dir {
        Some(name) => layout
            .with_project_dir(name)
            .map_err(|e| EmbedError::InvalidProjectDir {
                name: e.name,
                reason: e.reason.to_string(),
            })?,
        None => layout,
    };
    layout
        .with_product_name(&config.product)
        .map_err(|e| EmbedError::InvalidProductName {
            name: e.name,
            reason: e.reason.to_string(),
        })
}

fn check_host_libraries(config: &EmbedConfig) -> Result<(), EmbedError> {
    crate::plugins::host_libraries::check(&config.js_libs, layout_of(config)?.engine()).map_err(
        |e| EmbedError::InvalidHostLibrary {
            namespace: e.namespace,
            reason: e.reason,
        },
    )
}

fn daemon_config(config: &EmbedConfig, scope: &str) -> Result<DaemonConfig, EmbedError> {
    let cwd = config
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let layout = layout_of(config)?;
    let state = base::paths::StateRoot::new(config.data_root.clone());
    let paths = StaticDaemonPaths::with_project(state.scene_dir(scope), cwd)
        .with_global(config.data_root.clone())
        .with_layout(layout);
    let cli_settings = config.cli_settings();
    let mut daemon_config = load_daemon_config(
        &config.model,
        cli_settings.clone(),
        config.socket.as_deref(),
        scope,
        &paths,
    );
    if let Some(layer) = &cli_settings {
        if let Some((key, reason)) =
            crate::settings_source::cli_layer_refusal(layer, &daemon_config.settings)
        {
            return Err(EmbedError::InvalidSetting { key, reason });
        }
    }
    if config.mcp_policy.is_some() && !crate::modules::compiled().contains(&"mcp") {
        return Err(EmbedError::Startup(
            "mcp_policy: this build has no MCP module (feature `mcp`)".into(),
        ));
    }
    daemon_config.mcp_policy_path = config.mcp_policy.clone();
    daemon_config.managed_settings_path = config.managed_settings.clone();
    Ok(daemon_config)
}

/// Take the scene's lock for instance `name`.
fn claim(
    config: &EmbedConfig,
    name: &str,
    daemon_config: &DaemonConfig,
) -> Result<Instance, EmbedError> {
    let socket = &daemon_config.socket_path;
    if let Some(parent) = socket.parent() {
        discovery::create_private_dir(parent)
            .map_err(|e| EmbedError::Startup(format!("cannot create {}: {e}", parent.display())))?;
    }
    let lock =
        discovery::write_lock_file(&daemon_config.lock_path, socket).map_err(|e| match e {
            discovery::LockFileError::AnotherDaemonRunning { pid, socket_path } => {
                EmbedError::AlreadyRunning {
                    pid,
                    socket: socket_path,
                }
            }
            other => EmbedError::Startup(format!("could not claim the daemon lock: {other}")),
        })?;
    info!(pid = lock.pid, "wrote discovery lock");
    let mut scenes: Vec<String> = std::iter::once(config.scene.clone())
        .chain(config.scenes.iter().cloned())
        .collect();
    scenes.sort();
    scenes.dedup();
    Ok(Instance {
        name: name.to_string(),
        scenes,
        daemon_root: base::paths::StateRoot::new(&config.data_root).daemon_dir(),
        held: std::sync::Mutex::new(Some(lock)),
    })
}

impl Groups {
    /// Take the process's one record, then stop what a previous run that kept
    /// `file` left running. This run's commands are recorded there once the
    /// pool is built, so a start that fails leaves no record behind.
    async fn claim(file: PathBuf, instance: Option<&str>) -> Result<Groups, EmbedError> {
        let slot = RecordSlot::take(instance, &file)?;
        let reaping = file.clone();
        let (reaped, holder) = tokio::task::spawn_blocking(move || {
            let reaped = platform::orphans::reap_previous(&reaping, Duration::from_millis(500));
            (reaped, platform::orphans::live_owner(&reaping))
        })
        .await
        .map_err(|e| EmbedError::Startup(format!("stopping a previous run's commands: {e}")))?;
        if let Some(owner) = holder {
            return Err(EmbedError::GroupsHeld { owner, file });
        }
        if !reaped.is_empty() {
            warn!(
                count = reaped.len(),
                groups = ?reaped,
                file = %file.display(),
                "stopped process groups a previous run left running"
            );
        }
        Ok(Groups {
            file,
            slot: std::sync::Mutex::new(Some(slot)),
        })
    }

    fn release(&self) {
        let slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(slot);
    }
}

impl Instance {
    fn held(&self) -> std::sync::MutexGuard<'_, Option<DaemonLockGuard>> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Under the same lock [`Self::release`] takes: an entry is never
    /// written after the release removed it.
    fn announce(&self, socket: &Path, announced: Announced) {
        let held = self.held();
        if held.is_none() {
            return;
        }
        let identity = platform::process_identity::ProcessIdentity::current();
        let entry = InstanceFile {
            instance: self.name.clone(),
            pid: identity.pid,
            pid_start_time: identity.started_at,
            socket: socket.to_path_buf(),
            tcp: announced.tcp.map(|a| a.to_string()),
            ws: announced.ws.map(|a| a.to_string()),
            unix_socket_auth: announced.unix_socket_auth,
            token_file: announced.token_file,
            scenes: self.scenes.clone(),
            protocol_version: discovery::INSTANCE_PROTOCOL_VERSION,
            started_at: base::clock::now_rfc3339(),
        };
        let dir = self.daemon_root.join(base::paths::entry::INSTANCES);
        match discovery::write_instance_file(&dir, &self.name, &entry) {
            Ok(()) => info!(instance = %self.name, "wrote instances.d discovery entry"),
            Err(e) => warn!(
                instance = %self.name,
                error = %e,
                "failed to write instances.d discovery entry; discovery clients won't see this daemon"
            ),
        }
    }

    fn release(&self) {
        let mut held = self.held();
        discovery::remove_instance_file(
            &self.daemon_root.join(base::paths::entry::INSTANCES),
            &self.name,
        );
        drop(held.take());
    }
}

/// Nobody is connected yet to be asked "import from X?", so detection
/// emits `import_detected` instead; the `.imported.json` marker keeps it from
/// asking on every start.
fn spawn_import_detection(pool: Arc<SessionPool>) {
    let cwd = pool.cwd().to_path_buf();
    let layout = pool.layout();
    tokio::spawn(async move {
        if settings::import::already_decided(&cwd, &layout).await {
            return;
        }
        let sources = settings::import::detect_import_sources(&cwd).await;
        if sources.is_empty() {
            return;
        }
        pool.ask_import(crate::session_pool::api::DaemonEvent::ImportDetected {
            sources: sources
                .iter()
                .map(|s| crate::session_pool::api::ImportSource {
                    source: s.kind().as_str().to_string(),
                    description: s.describe(),
                })
                .collect(),
        });
    });
}

fn record_startup(
    pool: &SessionPool,
    perf: &telemetry::perf::PerfCollector,
    config_loaded_ms: f64,
    pool_built_ms: f64,
) {
    let total_startup_ms = perf.elapsed_ms() as u64;
    let timing = telemetry::events::StartupTimingPayload {
        total_startup_ms,
        config_load_ms: config_loaded_ms as u64,
        pool_build_ms: (pool_built_ms - config_loaded_ms).max(0.0) as u64,
    };
    info!(total_ms = total_startup_ms, ?timing, "startup complete");
    pool.record_startup(timing);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_can_be_shared_across_tasks() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Embedded>();
        send_sync::<EmbedError>();
    }

    #[test]
    fn every_builtin_scene_resolves() {
        for id in ["code", "harnix", "mini"] {
            let scene = resolve_scene(id, Path::new("/nonexistent"), &Default::default())
                .expect("a builtin scene");
            assert_eq!(scene.id.as_str(), id);
        }
    }

    #[test]
    fn an_unknown_scene_is_refused_with_the_known_ones() {
        let Err(EmbedError::UnknownScene { scene, known }) = resolve_scene(
            "not-a-real-scene",
            Path::new("/nonexistent"),
            &Default::default(),
        ) else {
            panic!("an unknown scene resolved");
        };
        assert_eq!(scene, "not-a-real-scene");
        for id in ["code", "harnix", "mini"] {
            assert!(known.iter().any(|k| k == id), "{known:?}");
        }
    }

    #[test]
    fn a_declarative_scene_under_the_data_root_resolves() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("triage");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("scene.json"),
            r#"{"id": "triage", "name": "Triage"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("prompt.md"), "Sort incoming issues.").unwrap();
        assert_eq!(
            resolve_scene("triage", root.path(), &Default::default())
                .unwrap()
                .id
                .as_str(),
            "triage"
        );
    }

    #[test]
    fn the_paths_come_from_the_config_alone() {
        let root = tempfile::tempdir().unwrap();
        let mut config = EmbedConfig::new(root.path());
        config.cwd = Some(root.path().join("work"));
        let daemon_config = daemon_config(&config, "code").unwrap();
        assert_eq!(
            daemon_config.socket_path,
            root.path().join("scenes").join("code").join("daemon.sock")
        );
        assert_eq!(daemon_config.paths.global_root(), root.path());
        assert_eq!(
            daemon_config.settings.paths.global_data_dir,
            root.path().to_path_buf()
        );
        config.socket = Some(root.path().join("elsewhere.sock"));
        assert_eq!(
            daemon_config_of(&config).socket_path,
            root.path().join("elsewhere.sock")
        );
    }

    #[test]
    fn the_project_directory_is_the_one_the_config_names() {
        let root = tempfile::tempdir().unwrap();
        let mut config = EmbedConfig::new(root.path());
        let work = root.path().join("work");
        config.cwd = Some(work.clone());
        assert_eq!(
            daemon_config_of(&config).settings.paths.local_data_dir,
            work.join(".agix")
        );
        config.project_dir = Some(".codix".into());
        let renamed = daemon_config_of(&config);
        assert_eq!(renamed.settings.paths.local_data_dir, work.join(".codix"));
        assert_eq!(renamed.paths.layout().project_dir_name(), ".codix");
        assert_eq!(
            renamed.settings.engine_config().layout,
            renamed.paths.layout()
        );
        for bad in ["../x", ".git", "a/b", ""] {
            config.project_dir = Some(bad.into());
            assert!(
                matches!(
                    daemon_config(&config, "code"),
                    Err(EmbedError::InvalidProjectDir { ref name, .. }) if name == bad
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_product_is_the_one_the_config_names() {
        let root = tempfile::tempdir().unwrap();
        let mut config = EmbedConfig::new(root.path());
        config.cwd = Some(root.path().join("work"));
        assert_eq!(
            daemon_config_of(&config).paths.layout().product_name(),
            "Agix"
        );
        config.product = "Codix".into();
        let renamed = daemon_config_of(&config);
        assert_eq!(renamed.paths.layout().product_name(), "Codix");
        assert_eq!(
            renamed.settings.engine_config().layout,
            renamed.paths.layout()
        );
        for bad in ["", "1x", "a b", "a/b", "a.b"] {
            config.product = bad.into();
            assert!(
                matches!(
                    daemon_config(&config, "code"),
                    Err(EmbedError::InvalidProductName { ref name, .. }) if name == bad
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_engine_names_the_project_directory_unless_the_config_names_one() {
        let root = tempfile::tempdir().unwrap();
        let mut config = EmbedConfig::new(root.path());
        let work = root.path().join("work");
        config.cwd = Some(work.clone());
        assert_eq!(daemon_config_of(&config).paths.layout().engine(), "agix");
        config.engine = "foo".into();
        let renamed = daemon_config_of(&config);
        assert_eq!(renamed.paths.layout().engine(), "foo");
        assert_eq!(renamed.paths.layout().product_name(), "Agix");
        assert_eq!(renamed.settings.paths.local_data_dir, work.join(".foo"));
        assert_eq!(
            renamed.settings.engine_config().layout,
            renamed.paths.layout()
        );
        config.project_dir = Some(".codix".into());
        let named = daemon_config_of(&config);
        assert_eq!(named.settings.paths.local_data_dir, work.join(".codix"));
        assert_eq!(named.paths.layout().engine(), "foo");
        config.project_dir = None;
        for bad in [
            "", "Foo", "1x", "a-b", "class", "tools", "escape", "codex", "wkts",
        ] {
            config.engine = bad.into();
            assert!(
                matches!(
                    daemon_config(&config, "code"),
                    Err(EmbedError::InvalidEngineName { ref name, .. }) if name == bad
                ),
                "{bad:?}"
            );
        }
        config.engine = "a".repeat(17);
        assert!(matches!(
            daemon_config(&config, "code"),
            Err(EmbedError::InvalidEngineName { .. })
        ));
    }

    fn daemon_config_of(config: &EmbedConfig) -> DaemonConfig {
        daemon_config(config, &config.scene).unwrap()
    }
}
