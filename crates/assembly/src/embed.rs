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

use base::interface::scene::SceneSpec;
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
    /// `model.max_tokens` where no settings file sets it.
    pub max_tokens: u32,
    pub session_cap: usize,
    pub session_idle_timeout: Duration,
    /// Over every session's `execution.permission_prompt_timeout_secs`.
    pub permission_prompt_timeout: Option<Duration>,
    /// `Some`: take the scene's `daemon.lock`, write the `instances.d` entry
    /// under this name, and record the commands sessions leave running so a
    /// later run can stop them. `None`: none of that — the host owns its own
    /// exclusion.
    pub instance: Option<String>,
    /// Where [`Embedded::serve_unix`] listens; `None` is
    /// `<data_root>/scenes/<scene>/daemon.sock`.
    pub socket: Option<PathBuf>,
    /// The MCP policy file in place of the platform's; it must exist.
    pub mcp_policy: Option<PathBuf>,
    /// The managed settings file in place of the platform's; it must exist.
    pub managed_settings: Option<PathBuf>,
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
            max_tokens: settings::DEFAULT_MAX_TOKENS,
            session_cap: 32,
            session_idle_timeout: Duration::from_secs(3600),
            permission_prompt_timeout: None,
            instance: None,
            socket: None,
            mcp_policy: None,
            managed_settings: None,
        }
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
    /// The process-group record is per process, and `running` holds it.
    #[error(
        "this process already runs the named instance `{running}`; a process runs at most one"
    )]
    InstanceInProcess { running: String },
}

/// The named instance this process runs, if any.
static NAMED_INSTANCE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// This process's one named instance, held until dropped.
struct NamedSlot;

impl NamedSlot {
    fn take(name: &str) -> Result<Self, EmbedError> {
        let mut held = NAMED_INSTANCE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(running) = held.as_ref() {
            return Err(EmbedError::InstanceInProcess {
                running: running.clone(),
            });
        }
        *held = Some(name.to_string());
        Ok(NamedSlot)
    }
}

impl Drop for NamedSlot {
    fn drop(&mut self) {
        *NAMED_INSTANCE
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
    stopped: tokio::sync::OnceCell<()>,
}

/// What a named instance holds while it runs.
struct Instance {
    name: String,
    scenes: Vec<String>,
    daemon_root: PathBuf,
    groups_file: PathBuf,
    /// `None` once released: nothing is announced after that.
    held: std::sync::Mutex<Option<(DaemonLockGuard, NamedSlot)>>,
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
        let scene = resolve_scene(&config.scene, &config.data_root.join("scenes"))?;
        let daemon_config = daemon_config(&config, &scene.id)?;
        let socket = daemon_config.socket_path.clone();
        let instance = match &config.instance {
            Some(name) => Some(claim(&config, name, &daemon_config).await?),
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
                permission_prompt_timeout: config.permission_prompt_timeout,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| EmbedError::Startup(format!("{e:#}")))?;
        let pool_built = startup.checkpoint("pool_built");
        if let Some(instance) = &instance {
            platform::orphans::persist_to(instance.groups_file.clone());
        }
        spawn_import_detection(pool.clone());
        record_startup(&pool, &startup, config_loaded, pool_built);

        let server = Arc::new(DaemonServer::new(pool.clone(), CancellationToken::new()));
        Ok(Embedded {
            server,
            pool,
            socket,
            instance,
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
    /// seconds), then give up the lock and the files a named instance wrote.
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
                if let Some(instance) = &self.instance {
                    instance.release();
                }
                info!("engine stopped");
            })
            .await;
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

/// The scene `name` names: a built-in, or a declarative scene under
/// `user_scenes` (`<data_root>/scenes/<id>/scene.json`).
pub(crate) fn resolve_scene(name: &str, user_scenes: &Path) -> Result<Arc<SceneSpec>, EmbedError> {
    let mut registry = scene::SceneRegistry::new();
    registry.register_builtin();
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

fn daemon_config(config: &EmbedConfig, scope: &str) -> Result<DaemonConfig, EmbedError> {
    let cwd = config
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let paths = StaticDaemonPaths::with_project(config.data_root.join("scenes").join(scope), cwd)
        .with_global(config.data_root.clone());
    let mut daemon_config = load_daemon_config(
        &config.model,
        config.max_tokens,
        config.socket.as_deref(),
        scope,
        &paths,
    );
    daemon_config.session_cap = config.session_cap;
    daemon_config.session_idle_timeout_secs = config.session_idle_timeout.as_secs();
    if config.mcp_policy.is_some() && !crate::modules::compiled().contains(&"mcp") {
        return Err(EmbedError::Startup(
            "mcp_policy: this build has no MCP module (feature `mcp`)".into(),
        ));
    }
    daemon_config.mcp_policy_path = config.mcp_policy.clone();
    daemon_config.managed_settings_path = config.managed_settings.clone();
    Ok(daemon_config)
}

/// Take the scene's lock for instance `name` and the process's one named
/// instance, then stop what a previous run of it left running. This run's
/// commands are recorded in its place once the pool is built, so a start
/// that fails leaves no record behind.
async fn claim(
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
    let slot = NamedSlot::take(name)?;
    let daemon_root = config.data_root.join("daemon");
    let groups_file = daemon_root.join("groups").join(format!("{name}.json"));
    let reaping = groups_file.clone();
    let reaped = tokio::task::spawn_blocking(move || {
        platform::orphans::reap_previous(&reaping, Duration::from_millis(500))
    })
    .await
    .map_err(|e| EmbedError::Startup(format!("stopping a previous run's commands: {e}")))?;
    if !reaped.is_empty() {
        warn!(
            count = reaped.len(),
            groups = ?reaped,
            "stopped process groups a previous run of this instance left running"
        );
    }
    let mut scenes: Vec<String> = std::iter::once(config.scene.clone())
        .chain(config.scenes.iter().cloned())
        .collect();
    scenes.sort();
    scenes.dedup();
    Ok(Instance {
        name: name.to_string(),
        scenes,
        daemon_root,
        groups_file,
        held: std::sync::Mutex::new(Some((lock, slot))),
    })
}

impl Instance {
    fn held(&self) -> std::sync::MutexGuard<'_, Option<(DaemonLockGuard, NamedSlot)>> {
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
        let dir = self.daemon_root.join("instances.d");
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
        let _ = std::fs::remove_file(&self.groups_file);
        discovery::remove_instance_file(&self.daemon_root.join("instances.d"), &self.name);
        drop(held.take());
    }
}

/// Nobody is connected yet to be asked "import from X?", so detection
/// emits `import_detected` instead; the `.imported.json` marker keeps it from
/// asking on every start.
fn spawn_import_detection(pool: Arc<SessionPool>) {
    let cwd = pool.cwd().to_path_buf();
    tokio::spawn(async move {
        if settings::import::already_decided(&cwd).await {
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
            let scene = resolve_scene(id, Path::new("/nonexistent")).expect("a builtin scene");
            assert_eq!(scene.id.as_str(), id);
        }
    }

    #[test]
    fn an_unknown_scene_is_refused_with_the_known_ones() {
        let Err(EmbedError::UnknownScene { scene, known }) =
            resolve_scene("not-a-real-scene", Path::new("/nonexistent"))
        else {
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
            resolve_scene("triage", root.path()).unwrap().id.as_str(),
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

    fn daemon_config_of(config: &EmbedConfig) -> DaemonConfig {
        daemon_config(config, &config.scene).unwrap()
    }
}
