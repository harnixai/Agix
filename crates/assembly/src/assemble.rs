//! Building a running engine out of a configuration.
//!
//! The order these pieces go together in, and what they default to, is the
//! engine's knowledge rather than its embedder's: read the settings, resolve
//! the provider routing, build the credentials and the client, place the
//! history roots, build the [`SessionPool`], then run
//! the startup steps a session silently misses without — plugin components,
//! MCP connection, the janitor. A step nobody performs is no compile error, so
//! this is the one place that knows: `main.rs` is a command line and a set of
//! listeners on top of it, and a host embedding the daemon hands over a
//! [`DaemonConfig`] and gets back an engine.
//!
//! What stays out: anything with a process to itself. Discovery locks,
//! listeners, signal handling and startup telemetry belong to whatever *runs*
//! a daemon, and a host embedding one has its own answers for all four.

use std::path::PathBuf;
use std::sync::Arc;

use base::interface::credentials::CredentialSource;
use base::interface::permission::PermissionPolicy;
use base::interface::scene::SceneSpec;
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient, NoCredentialsClient};
use tracing::{info, warn};

use crate::config::DaemonConfig;
pub use crate::session_pool::McpOptions;
use crate::session_pool::{SessionCustomizer, SessionPool};

/// The parts of an assembly a caller may legitimately differ on.
///
/// Deliberately small. Every field here is either something the process knows
/// and the configuration cannot (the working directory), or a seam a test or
/// a host genuinely substitutes (the model, the store, the permission). The
/// rest of the assembly is not configurable, because a host choosing its own
/// order for it is the problem this module exists to remove.
#[derive(Default)]
pub struct Assembly {
    /// The project new sessions belong to when they name none. `None` is the
    /// process's own working directory.
    pub cwd: Option<PathBuf>,
    /// Scenes to activate beyond the one the pool is built with. An unknown
    /// name fails the assembly rather than quietly starting with fewer
    /// scenes than asked for.
    pub extra_scenes: Vec<String>,
    /// Scenes the host defines beyond the built-in ones. Registered before
    /// `extra_scenes` is read, so they can be named there; otherwise they are
    /// available to `scene.activate`.
    pub scenes: Vec<Arc<SceneSpec>>,
    /// Where model calls go. `None` builds an Anthropic client keyed from
    /// `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`, else from `credentials`
    /// asked for provider `"default"`.
    pub model_client: Option<Arc<dyn AnthropicClient>>,
    /// Keys for whatever the configuration leaves unset: a provider entry
    /// with neither `api_key` nor `api_key_env` is answered from here, as is
    /// the default client when the environment has no key. For a host that
    /// keeps credentials in a vault or keychain rather than on disk.
    pub credentials: Option<Arc<dyn base::interface::credentials::CredentialSource>>,
    /// Where transcripts live.
    pub transcripts: Transcripts,
    /// Over the settings' `app.history` for the store this assembly
    /// opens (`Transcripts::UnderGlobalRoot`); `None` leaves it to them.
    pub history: Option<settings::HistoryLocation>,
    /// The host's own say on every call, weighed as one more tier of rules:
    /// its deny stands in every mode, its permit is one allow rule.
    pub host_policy: Option<Arc<dyn PermissionPolicy>>,
    /// Rust plugins every session this pool builds is built with, in order.
    pub plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
    /// The host's own plugin packages, each an unpacked package directory:
    /// checked like an install and loaded in place of an installed package
    /// of the same name. One that does not load fails the assembly; with
    /// plugins switched off by settings none is loaded.
    pub host_plugins: Vec<PathBuf>,
    /// The JavaScript libraries the host gives plugin code: a package's
    /// commands and functions get those its `uses` names, and a package
    /// naming one not here does not install or load. A library that cannot
    /// be installed — its namespace not a new global, the engine's name or
    /// another library's, a function path not dotted identifiers or given
    /// twice — fails the assembly.
    pub js_libs: Vec<base::interface::host_js::HostJsLib>,
    /// The host's last word on every session engine — see
    /// [`SessionCustomizer`].
    pub customize_session: Option<SessionCustomizer>,
    /// MCP: where sign-ins are kept, the OAuth client, the enterprise IdP
    /// and the policy. An empty struct in a build without the `mcp` module.
    pub mcp: McpOptions,
}

/// Where a pool keeps transcripts.
///
/// `Nowhere` is a real mode, not a test shortcut: it is what a daemon falls
/// into when the store cannot be opened at startup, and a session read or
/// branch has no possible answer there. Expressible here so that mode is
/// reachable through this entry point rather than only by assembling a pool
/// by hand — which is the thing this module exists to stop.
#[derive(Default)]
pub enum Transcripts {
    /// Under the config's own global root.
    #[default]
    UnderGlobalRoot,
    /// This store, wherever the caller put it.
    In(Arc<dyn history::store::HistoryStore>),
    /// No persistence at all.
    Nowhere,
}

/// A pool that is built, wired and serving.
///
/// Several pools can share one process, each on its own configuration. The
/// one exception is the OpenTelemetry exporter (`otel` feature): it installs
/// process-wide providers, so the first endpoint any pool starts it on is the
/// one every pool reports to, and a different endpoint is warned about once.
///
/// "Serving" is the part a caller cannot see and would not think to do: the
/// returned pool has already loaded its plugin components, kicked off MCP
/// connection in the background, and started its janitor. A pool missing any
/// of those looks identical and behaves subtly worse — sessions with no
/// plugin tools, servers that never connect, sessions that never expire.
pub async fn pool(
    config: &DaemonConfig,
    scene: Arc<SceneSpec>,
    assembly: Assembly,
) -> anyhow::Result<Arc<SessionPool>> {
    crate::plugins::host_libraries::check(&assembly.js_libs, config.paths.layout().engine())?;
    let (managed_path, managed) = managed_settings(config);
    let settings = Arc::new(config.settings.clone().with_managed(managed.clone()));
    platform::configure_system_proxy(settings.values.app.system_proxy);
    for (section, module) in crate::modules::unassembled_settings(&settings.values) {
        tracing::warn!(
            section,
            module,
            "settings configure a module this build does not include; ignored"
        );
    }

    let credentials: Arc<dyn CredentialSource> = match &assembly.credentials {
        Some(host) => Arc::new(base::interface::credentials::ConfigThen(host.clone())),
        None => Arc::new(base::interface::credentials::ConfigCredentials),
    };
    let client = match assembly.model_client {
        Some(client) => client,
        None => default_anthropic_client(assembly.credentials.as_deref(), &config.paths.layout())?,
    };
    // Validation, not a value: every session's router is built by its own
    // `(project, scene)` binding from that pair's settings. What this still
    // does is fail startup loudly on a daemon whose own project's providers
    // do not route, rather than letting every session quietly fall back to
    // the built-in provider.
    validate_task_router_config(config, &client, &credentials)?;
    if matches!(
        settings.values.permissions.mode,
        base::permission::PermissionMode::BypassPermissions
    ) {
        warn!(
            "permissions.mode = bypassPermissions — no permission rule is consulted for \
             any tool call in any session; this daemon is trusting its host to sandbox it"
        );
    }

    let cwd = assembly
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let history_store = match assembly.transcripts {
        Transcripts::UnderGlobalRoot => {
            default_history_store(&settings, &cwd, assembly.history).await
        }
        Transcripts::In(store) => Some(store),
        Transcripts::Nowhere => None,
    };

    let mut pool = SessionPool::builder(
        config.session_cap,
        config.session_idle_timeout_secs,
        client,
        settings,
        scene,
        assembly.host_policy,
        cwd,
        history_store,
        config.paths.clone(),
    )
    .credentials(credentials)
    .managed_settings(managed_path, managed)
    .host_changes(&own_settings_on_disk(config), config)
    .plugins(assembly.plugins)
    .host_libraries(assembly.js_libs);
    if let Some(customize) = assembly.customize_session {
        pool = pool.session_customizer(customize);
    }
    pool = pool.mcp(assembly.mcp.resolve(config)?);
    let pool = pool.build();

    take_up_scenes(&pool, assembly.scenes, &assembly.extra_scenes).await?;
    register_host_plugins(&pool, &assembly.host_plugins).await?;

    // Before anything can create a session: a session built while this is
    // still running would be missing every plugin tool, with nothing to
    // indicate why.
    pool.load_plugins().await;

    // Backgrounded: a slow or unreachable server can never delay startup, and
    // a failure warns and emits `mcp_connect_failed` rather than aborting.
    pool.connect_mcp_servers_in_background();

    pool.start_janitor();
    pool.start_config_watcher();
    pool.start_scheduler();

    Ok(pool)
}

async fn take_up_scenes(
    pool: &SessionPool,
    scenes: Vec<Arc<SceneSpec>>,
    extra_scenes: &[String],
) -> anyhow::Result<()> {
    for scene in scenes {
        pool.register_scene(scene);
    }
    for extra in extra_scenes {
        if extra == pool.scene_id() {
            continue; // already active; not an error to repeat it
        }
        pool.activate_scene(extra)
            .await
            .map_err(|e| anyhow::anyhow!("scene `{extra}`: {e}"))?;
    }
    Ok(())
}

async fn register_host_plugins(pool: &SessionPool, dirs: &[PathBuf]) -> anyhow::Result<()> {
    if dirs.is_empty() {
        return Ok(());
    }
    if pool.plugin_status() != crate::plugins::PluginStatus::Enabled {
        warn!(
            count = dirs.len(),
            status = pool.plugin_status().as_str(),
            "plugins are off; the host's own plugin packages are not loaded"
        );
        return Ok(());
    }
    for dir in dirs {
        pool.register_host_plugin(dir)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(())
}

/// The daemon's own pair's settings as they are on disk, read the way
/// `load_daemon_config` read them — the `cli` layer included, the managed
/// layer not — so what differs from `config.settings` is what the host
/// changed afterwards.
fn own_settings_on_disk(config: &DaemonConfig) -> settings::LoadedSettings {
    let paths = &config.paths;
    crate::settings_source::read_pair(
        paths.global_root(),
        paths.config_root(),
        Some(&paths.project_root()),
        &paths.layout(),
        &config.settings.paths.scope,
        &config.settings.default_model,
        crate::settings_source::Above {
            cli: config.cli_settings.as_ref(),
            managed: None,
        },
    )
}

/// The managed settings file this pool reads — the one the config names,
/// which must exist, else the platform's — and what is in it. Settings that
/// already carry a managed layer (an embedder's `Tier::Managed` source) keep
/// it when there is no file.
fn managed_settings(config: &DaemonConfig) -> (PathBuf, Option<base::permission::ManagedSettings>) {
    let (path, required) = match &config.managed_settings_path {
        Some(path) => (path.clone(), true),
        None => (config.paths.layout().managed_settings_file(), false),
    };
    let managed = crate::settings_source::read_managed(&path, required)
        .or_else(|| config.settings.managed.clone());
    (path, managed)
}

/// Whether the daemon's own pair's providers route and build.
fn validate_task_router_config(
    config: &DaemonConfig,
    client: &Arc<dyn AnthropicClient>,
    credentials: &Arc<dyn CredentialSource>,
) -> anyhow::Result<()> {
    let settings = &config.settings;
    let router = crate::model_router::build_task_router(
        &settings.values,
        client,
        credentials,
        &config.paths.layout(),
        settings
            .paths
            .global_data_dir
            .join(base::paths::entry::QUOTA),
    )
    .map_err(|e| anyhow::anyhow!("the providers in settings do not route: {e}"))?;
    info!(order = ?router.order(), "model providers");
    Ok(())
}

/// The client of the provider the engine supplies when the settings define
/// none: keyed from the environment, else from the host's source, asked for
/// provider `"default"`.
///
/// With no key anywhere (an empty one counts as none) it is a
/// [`NoCredentialsClient`]: a daemon that cannot reach the default provider
/// can still serve sessions routed to a configured one, and a turn that does
/// reach it fails with `no_credentials` before anything is sent.
fn default_anthropic_client(
    host: Option<&dyn CredentialSource>,
    layout: &base::paths::Layout,
) -> anyhow::Result<Arc<dyn AnthropicClient>> {
    let none = base::provider::ProviderConfig::default();
    let api_key = base::interface::credentials::EnvCredentials::anthropic()
        .api_key("default", &none)
        .or_else(|env| match host {
            Some(host) => host.api_key("default", &none),
            None => Err(env),
        })
        .ok()
        .map(|key| key.expose().to_string())
        .filter(|key| !key.is_empty());
    let base = match std::env::var("ANTHROPIC_BASE_URL").ok() {
        Some(mut url) => {
            // Ensure trailing slash so Url::join appends instead of replacing
            if !url.ends_with('/') {
                url.push('/');
            }
            Some(
                reqwest::Url::parse(&url)
                    .map_err(|e| anyhow::anyhow!("invalid ANTHROPIC_BASE_URL: {e}"))?,
            )
        }
        None => None,
    };
    let Some(api_key) = api_key else {
        warn!(
            "no ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY, and the host supplied no \
             credentials: turns on the default provider will fail with no_credentials"
        );
        return Ok(Arc::new(NoCredentialsClient::new(
            "provider `default` has no API key: set ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY \
             (any non-empty value for an ANTHROPIC_BASE_URL gateway that needs none), configure \
             a provider with `api_key_env` in settings, or have the host supply one through its \
             CredentialSource; the request was not sent",
        )));
    };
    let auth = AuthMode::ApiKey(api_key);
    let client = match base {
        Some(base) => HttpAnthropicClient::with_base(auth, base)?,
        None => HttpAnthropicClient::new(auth)?,
    };
    Ok(Arc::new(client.with_network(Arc::new(
        platform::LocalNetwork::default().with_user_agent(layout.user_agent()),
    ))))
}

/// Transcripts under the global root, or in their projects where `location`
/// (else `app.history`) and the project's trust say.
///
/// `None` — logged, not fatal — when the store cannot be opened: a daemon
/// that runs without persistence is degraded, and one that refuses to start
/// is unavailable.
async fn default_history_store(
    settings: &settings::LoadedSettings,
    cwd: &std::path::Path,
    location: Option<settings::HistoryLocation>,
) -> Option<Arc<dyn history::store::HistoryStore>> {
    let global_dir = &settings.paths.global_data_dir;
    let projects = crate::project_history::ProjectHistoryPolicy::new(
        location.unwrap_or(settings.values.app.history),
        settings.paths.layout.clone(),
        global_dir.clone(),
    );
    let roots = history::path::HistoryRoots::under(global_dir);
    match history::store::JsonlHistoryStore::with_roots(cwd, roots).await {
        Ok(store) => Some(Arc::new(store.with_project_history(Arc::new(projects)))),
        Err(e) => {
            warn!(error = %e, "failed to initialize session history store; sessions will be in-memory only for this run");
            None
        }
    }
}
