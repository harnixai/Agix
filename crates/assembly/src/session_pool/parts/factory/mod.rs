//! 造引擎的 `EngineFactory`：按一个会话的规格、它那一对的绑定和门面备好的部件
//! （会话的状态与 steer 队列、场景表、定时任务表、MCP 模块、进程组账本、插件脚本）
//! 配好 `Builder` 建出 Agent。建出来的引擎还没挂到会话状态上，由门面在上一代引擎
//! 停下之后 `attach_session`。只负责构造，不往会话表里插，也不写日志；不持锁。
//! 宿主连接提供的工具与 hook 的登记处（`HostConnections`）也归它：工具在建引擎
//! 那一刻拷进会话，hook 交给会话的 hook runner 每次现读。

mod namer;
mod permission_scripts;

pub(in crate::session_pool) use namer::SessionNamer;
pub(in crate::session_pool) use permission_scripts::*;

use crate::session_pool::api::SessionOptions;
use crate::session_pool::parts::bindings::ProjectBinding;
use crate::session_pool::parts::hosts::HostConnections;
use crate::session_pool::parts::phase::Generation;
use crate::session_pool::parts::specs::SessionSpec;
use crate::session_pool::pool_telemetry::{otel_sink, recordings_root, session_clock};
use crate::session_pool::{CreateError, SessionCtx, SessionCustomizer};
use base::interface::permission::PermissionPolicy;
use base::interface::scene::SceneSpec;
use base::recorder::{Divergence, RecorderConfig, RecorderMode};
use runtime::agent::{Agent, Builder, EventReceiver, InputSender};
use settings::LoadedSettings;
use std::path::Path;
use std::sync::Arc;
use telemetry::file_recorder::FileRecorder;
use telemetry::recorder::RecorderModel;
use tracing::warn;

pub(in crate::session_pool) struct EngineFactory {
    /// Built-in tool registry (Bash/Read/Write/Edit/Grep/Glob/...), shared
    /// (cheap `Arc` clone) by every session `Builder`. `Builder::build()` on
    /// its own registers only Skill/TaskStop/TaskOutput/Import/Agent/MCP; see
    /// `tools::register_builtin_tools` for what's deliberately excluded.
    tools: Arc<base::tool::InMemoryToolRegistry>,
    /// The host's own policy, one tier of every session's rules (see
    /// `resolve_session_permission`).
    host_policy: Option<Arc<dyn PermissionPolicy>>,
    rust_plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
    customize: Option<SessionCustomizer>,
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
    scripts: ScriptShares,
    background_hooks: hooks::BackgroundHooks,
    hosts: HostConnections,
}

/// What the scripts of every session this pool builds share.
pub(in crate::session_pool) struct ScriptShares {
    /// How many scripts this pool's sessions may run at once, together. One
    /// per pool, so a pool filling it leaves the others in the process their
    /// own.
    pub(in crate::session_pool) run_slots: Arc<tokio::sync::Semaphore>,
    /// The JavaScript libraries the host gives plugin code.
    pub(in crate::session_pool) host_js_libs: Arc<[base::interface::host_js::HostJsLib]>,
}

/// What an engine takes over from the session it runs: the session's state
/// and steer queue, and the number it runs them as.
pub(in crate::session_pool) struct SessionSeed {
    pub(in crate::session_pool) state: Arc<session_state::SessionState>,
    pub(in crate::session_pool) steer: runtime::steering::SteerHandle,
    pub(in crate::session_pool) generation: Generation,
}

/// What the engine starts from besides its settings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::session_pool) enum Restore {
    /// A session with no history yet.
    New,
    /// The session's log: its transcript, and the state it recorded, into a
    /// state no engine has run on.
    FromLog,
    /// The transcript from the log; the state is the one the engine before
    /// this one ran on.
    Kept,
}

/// One session's engine to build, with what the facade gathered for it from
/// the other collaborators.
pub(in crate::session_pool) struct BuildRequest<'a> {
    pub(in crate::session_pool) session_id: &'a str,
    pub(in crate::session_pool) seed: SessionSeed,
    pub(in crate::session_pool) restore: Restore,
    pub(in crate::session_pool) scene: Arc<SceneSpec>,
    pub(in crate::session_pool) binding: &'a ProjectBinding,
    /// `None` for a no-project session.
    pub(in crate::session_pool) project_root: Option<&'a Path>,
    /// The worktree of the project the session's tools work in, when not the
    /// project itself.
    pub(in crate::session_pool) worktree: Option<&'a base::paths::LinkedWorktree>,
    /// What the session's scripts resolve against: its project, else the
    /// pool's own directory.
    pub(in crate::session_pool) script_root: &'a Path,
    pub(in crate::session_pool) spec: &'a SessionSpec,
    pub(in crate::session_pool) scene_registry: Arc<scene::SceneRegistry>,
    pub(in crate::session_pool) cron_store: Arc<tools::cron::CronStore>,
    pub(in crate::session_pool) packages: crate::plugins::SessionPackages,
    pub(in crate::session_pool) mcp_module: Option<Arc<dyn runtime::module::Module>>,
    pub(in crate::session_pool) process_groups: Option<Arc<platform::GroupLedger>>,
    /// The session's host skill directories, and every session's.
    pub(in crate::session_pool) skill_dirs: &'a crate::session_pool::SessionSkillDirs,
    pub(in crate::session_pool) protected_dirs: base::context::LiveDirs,
    /// An engine for one host call and gone after it: it writes no
    /// transcript and no telemetry file.
    pub(in crate::session_pool) transient: bool,
}

pub(in crate::session_pool) struct BuiltEngine {
    pub(in crate::session_pool) agent: Agent,
    pub(in crate::session_pool) event_rx: EventReceiver,
    pub(in crate::session_pool) input_tx: InputSender,
    pub(in crate::session_pool) namer: SessionNamer,
    pub(in crate::session_pool) scripts: Option<Arc<base::interface::script::ScriptLedger>>,
    /// Plugin scripts that could not be bound, by plugin, for the plugin
    /// subsystem's fault records.
    pub(in crate::session_pool) script_faults: Vec<(String, String)>,
    /// The settings the engine was built with: the pair's, with this
    /// session's mode and model written in.
    pub(in crate::session_pool) settings: Arc<LoadedSettings>,
}

impl EngineFactory {
    pub(in crate::session_pool) fn new(
        tools: Arc<base::tool::InMemoryToolRegistry>,
        host_policy: Option<Arc<dyn PermissionPolicy>>,
        rust_plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
        customize: Option<SessionCustomizer>,
        history_store: Option<Arc<dyn history::store::HistoryStore>>,
        host_js_libs: Arc<[base::interface::host_js::HostJsLib]>,
    ) -> Self {
        Self {
            tools,
            host_policy,
            rust_plugins,
            customize,
            history_store,
            scripts: ScriptShares {
                run_slots: runtime::agent::default_run_slots(),
                host_js_libs,
            },
            background_hooks: hooks::BackgroundHooks::new(),
            hosts: HostConnections::default(),
        }
    }

    /// What the host connections provide every session built from now on.
    pub(in crate::session_pool) fn hosts(&self) -> &HostConnections {
        &self.hosts
    }

    /// Whether a session's own tools could be called `name`: one of the
    /// built-in tools or one the engine registers itself.
    pub(in crate::session_pool) fn reserves_tool_name(&self, name: &str) -> bool {
        runtime::host_tools::ENGINE_TOOL_NAMES.contains(&name) || self.tools.get(name).is_some()
    }

    pub(in crate::session_pool) fn run_slots(&self) -> &Arc<tokio::sync::Semaphore> {
        &self.scripts.run_slots
    }

    pub(in crate::session_pool) fn host_js_libs(
        &self,
    ) -> &Arc<[base::interface::host_js::HostJsLib]> {
        &self.scripts.host_js_libs
    }

    /// `builder` with what this pool's scripts share.
    fn sharing_scripts(&self, builder: Builder) -> Builder {
        builder
            .run_slots(self.scripts.run_slots.clone())
            .host_js_libs(self.scripts.host_js_libs.clone())
    }

    /// Where every session's notify-only hooks run, drained at shutdown.
    pub(in crate::session_pool) fn background_hooks(&self) -> &hooks::BackgroundHooks {
        &self.background_hooks
    }

    pub(in crate::session_pool) fn build(
        &self,
        req: BuildRequest<'_>,
    ) -> Result<BuiltEngine, CreateError> {
        let (session_id, scene, binding, spec) = (req.session_id, req.scene, req.binding, req.spec);
        let options = spec.options.as_ref();
        let scene_id = scene.id.clone();
        let (model, task_router, model_name, clock) = models(session_id, binding, spec)?;
        let settings_snapshot = self.snapshot(
            &binding.settings,
            spec.requested_permission_mode(),
            model_name.as_ref(),
            options,
            session_id,
            req.worktree,
        );
        let permission =
            resolve_session_permission(self.host_policy.as_ref(), &settings_snapshot, options);

        let mut builder = self
            .sharing_scripts(Builder::new())
            .scene(scene)
            .model(model)
            .tools(self.tools.clone())
            .settings(settings_snapshot.clone())
            .permission(permission)
            .session_id(session_id.to_string())
            .session_state(req.seed.state, req.seed.generation.get())
            .steering(req.seed.steer)
            .cron_store(req.cron_store)
            .background_hooks(self.background_hooks.clone())
            .attended(options.and_then(|o| o.attended))
            .question_prompts(options.is_some_and(SessionOptions::answers_questions))
            .mcp_prompts(options.is_some_and(SessionOptions::answers_mcp_elicitation));
        if !req.transient {
            builder = self.kept(builder, &settings_snapshot, options, session_id);
        }
        if let Some(clock) = clock {
            builder = builder.environment(clock);
        }
        builder = builder.task_router(task_router);
        builder = builder.scene_registry(req.scene_registry);
        let (mut builder, script_ledger, script_faults) = bind_session_scripts(
            builder.ext_commands(req.packages.commands),
            &settings_snapshot,
            req.script_root,
            &req.packages.scripts,
        );

        if let Some(module) = req.mcp_module {
            builder = builder.module(module);
        }
        builder = builder
            .modules(binding.modules.session_modules(req.skill_dirs))
            .skill_dirs(runtime::agent::HostSkillDirs {
                readable: req.skill_dirs.readable().clone(),
                protected: req.protected_dirs.clone(),
            });

        if let Some(prompt) = spec.prompt() {
            builder = builder.session_prompt(prompt);
        }
        if let Some(schema) = options.and_then(|o| o.output_schema.clone()) {
            builder = builder.output_schema(schema);
        }
        builder = builder
            .host_tools(self.host_tools_of(session_id, options))
            .host_hooks(Arc::new(self.hosts.clone()));
        if let Some(ledger) = &req.process_groups {
            builder = builder.process_groups(ledger.clone());
        }
        for plugin in &self.rust_plugins {
            builder = builder.plugin(plugin.clone());
        }
        if let Some(customize) = &self.customize {
            builder = customize(
                builder,
                &SessionCtx {
                    session_id,
                    scene: &scene_id,
                    project_root: req.project_root,
                    resumed: req.restore != Restore::New,
                },
            );
        }
        let (agent, event_rx, input_tx) = builder
            .goal_rounds_from_owner()
            .build()
            .map_err(build_failed)?;
        let namer = SessionNamer::of(session_id, &agent);
        Ok(BuiltEngine {
            agent,
            event_rx,
            input_tx,
            namer,
            scripts: script_ledger,
            script_faults,
            settings: settings_snapshot,
        })
    }

    /// `builder` writing the session's transcript and telemetry file.
    fn kept(
        &self,
        mut builder: Builder,
        settings: &LoadedSettings,
        options: Option<&SessionOptions>,
        session_id: &str,
    ) -> Builder {
        if let Some(store) = &self.history_store {
            builder = builder.history_store(store.clone());
        }
        if let Some(handle) = session_telemetry(settings, options, session_id) {
            builder = builder.telemetry_handle(handle);
        }
        builder
    }

    /// The session's own host tools, then those the host connections provide
    /// now; a connection's that the session declares itself is left out.
    fn host_tools_of(
        &self,
        session_id: &str,
        options: Option<&SessionOptions>,
    ) -> Vec<base::interface::host_call::HostToolDecl> {
        let mut decls: Vec<_> = options
            .into_iter()
            .flat_map(|o| &o.host_tools)
            .map(|t| t.declaration())
            .collect();
        for provided in self.hosts.tools() {
            if decls.iter().any(|own| own.name == provided.name) {
                warn!(
                    session_id,
                    tool = %provided.name,
                    "a host connection's tool is left out: the session declares its own"
                );
                continue;
            }
            decls.push(provided);
        }
        decls
    }

    /// The pair's settings with this session's permission mode, model,
    /// turn limits, scratch workspace and worktree written in.
    fn snapshot(
        &self,
        base_settings: &LoadedSettings,
        requested_mode: Option<base::permission::PermissionMode>,
        model_name: Option<&String>,
        options: Option<&SessionOptions>,
        session_id: &str,
        worktree: Option<&base::paths::LinkedWorktree>,
    ) -> Arc<LoadedSettings> {
        // The mode is decided once and written into the snapshot as well:
        // the gate, the hook payloads and `Meta` all read it from there, and a
        // snapshot still holding the settings-file mode made every one of them
        // report a mode the session was not running in.
        let permission_mode = effective_permission_mode(base_settings, requested_mode);
        {
            let mut snap = base_settings.clone();
            snap.values.permissions.mode = permission_mode;
            if let Some(name) = model_name {
                snap.values.model.model_name = name.clone();
            }
            if let Some(options) = options {
                options.tighten_turn(&mut snap.values.turn);
            }
            if let base::paths::Workspace::Scratch(_) = snap.paths.workspace {
                snap.paths.workspace = base::paths::Workspace::Scratch(
                    base::paths::session_scratch_dir(&snap.paths.global_data_dir, session_id),
                );
            }
            snap.paths.worktree = worktree.cloned();
            Arc::new(snap)
        }
    }
}

/// The conversation's model and the session's own router, its clients
/// wrapped in the session's recorder when it asked for one, the model name
/// chosen for it, and the clock it keeps time by.
#[allow(clippy::type_complexity)]
fn models(
    session_id: &str,
    binding: &ProjectBinding,
    spec: &SessionSpec,
) -> Result<
    (
        Arc<dyn base::interface::model::Model>,
        Arc<base::provider::TaskRouter>,
        Option<String>,
        Option<Arc<dyn base::interface::environment::Environment>>,
    ),
    CreateError,
> {
    let options = spec.options.as_ref();
    let base_settings = &binding.settings;
    let recorder_config = options
        .and_then(|o| o.recorder.as_ref())
        .map(|recorder| recorder_config(session_id, recorder, base_settings));
    let clock = session_clock(options, recorder_config.as_ref(), session_id);
    if let Some(refusal) = &binding.routing.refusal {
        return Err(CreateError::Unroutable(refusal.clone()));
    }
    let router = binding
        .task_router
        .for_session(spec.provider().as_deref())?;
    let model_name = match spec.model() {
        Some(name) => router.resolve_model(&name),
        None => router
            .model_name_for(base::provider::TaskTiers::MAIN)
            .unwrap_or_else(|| base_settings.values.model.model_name.clone()),
    };
    // Every provider's client is wrapped, sharing one `Recorder`, so
    // sub-agents, compaction and memory extraction — everything reached
    // through the router — record into the same recording as the
    // conversation.
    let router = match recorder_config {
        None => router,
        Some(config) => {
            let shared = spec.recorder.clone().unwrap_or_default();
            router.map_models(|provider_id, inner| {
                Arc::new(
                    RecorderModel::shared(
                        inner,
                        Some(config.clone()),
                        env!("CARGO_PKG_VERSION"),
                        shared.clone(),
                    )
                    .with_provider_id(provider_id),
                ) as Arc<dyn base::interface::model::Model>
            })
        }
    };
    let model = router.model_for(base::provider::TaskTiers::MAIN);
    Ok((model, Arc::new(router), Some(model_name), clock))
}

/// What the session's `recorder` option asks the model wrapper to do.
fn recorder_config(
    session_id: &str,
    recorder: &crate::session_pool::api::RecorderOptions,
    settings: &LoadedSettings,
) -> RecorderConfig {
    RecorderConfig {
        mode: match recorder.mode.as_str() {
            "record" => RecorderMode::Record,
            // `rerun` is driven from a recording, not from a live
            // session, so a session asking for it just talks to the
            // provider — see `telemetry::recorder::rerun`.
            "rerun" => RecorderMode::Rerun,
            _ => RecorderMode::Replay,
        },
        // Recording, a session and its sub-agents write into one
        // recording, named after the session.
        name: recorder
            .name
            .clone()
            .or_else(|| recorder.records().then(|| session_id.to_string())),
        root: recordings_root(settings, recorder.dir.as_deref()),
        on_divergence: if recorder.strict {
            Divergence::Strict
        } else {
            Divergence::Warn
        },
    }
}

/// The session's telemetry pipeline: its JSONL file, plus OTel when an
/// endpoint is configured. `None` records nothing.
fn session_telemetry(
    settings_snapshot: &LoadedSettings,
    options: Option<&SessionOptions>,
    session_id: &str,
) -> Option<telemetry::TelemetryHandle> {
    // On by default: `Builder::build()`'s fallback handle drops its
    // receiver on the spot, so without a recorder every event is lost.
    // The caller's `output` only chooses *where*, and
    // `telemetry.enabled: false` in settings.json opts out.
    //
    // `<session-id>.jsonl` in the project's `state/telemetry/` keeps
    // one file per session, next to the transcripts, so a recording is
    // findable by session id without the caller having had to plan for
    // it in advance.
    // The default location is only used when `local_data_dir` is a real
    // absolute path. It is not always set (tests, embedders that never
    // configure `paths`), and joining onto an empty or relative value
    // would scatter `telemetry/` directories into whatever the process's
    // working directory happens to be — including a user's repository.
    // Recording nothing is much better than writing somewhere surprising.
    let telemetry_path = match options.and_then(|o| o.telemetry.as_ref()) {
        Some(t) => Some(std::path::PathBuf::from(&t.output)),
        None => {
            let paths = settings_snapshot.paths.config_paths();
            paths
                .local_data_dir
                .is_absolute()
                .then(|| paths.telemetry_dir().join(format!("{session_id}.jsonl")))
        }
    };
    if let (true, Some(telemetry_path)) =
        (settings_snapshot.values.telemetry.enabled, telemetry_path)
    {
        match FileRecorder::new(&telemetry_path) {
            Ok(rec) => {
                let data_dir = &settings_snapshot.paths.local_data_dir;
                if data_dir.is_absolute() && telemetry_path.starts_with(data_dir) {
                    base::paths::ensure_local_data_gitignore(data_dir);
                }
                // `telemetry` crate never touches the network itself —
                // this is where the daemon decides where events go:
                // always the per-session JSONL file, plus OTel when an
                // endpoint is configured. `start_otel` installs the
                // global meter/tracer providers, so that one is a
                // process-level side effect done once, guarded by a
                // `OnceLock` rather than per session (see `otel_sink`).
                let mut recorders: Vec<std::sync::Arc<dyn telemetry::TelemetryRecorder>> =
                    vec![std::sync::Arc::new(rec)];
                if let Some(otel) = otel_sink(settings_snapshot) {
                    recorders.push(otel);
                }
                let telemetry_config = telemetry::TelemetryConfig {
                    enabled: true,
                    mode: telemetry::TelemetryMode::Enabled,
                    default_event_enabled: true,
                    ..Default::default()
                };
                match telemetry::spawn(telemetry_config, recorders) {
                    Ok((handle, consumer)) => {
                        tokio::spawn(consumer);
                        return Some(handle);
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to start telemetry pipeline; this session records nothing");
                    }
                }
            }
            Err(e) => {
                warn!(
                    path = %telemetry_path.display(),
                    error = %e,
                    "could not open the session telemetry file; this session records nothing"
                );
            }
        }
    }
    None
}

/// The session's scripts — the settings' own and the plugins' — bound into
/// `builder`, what they did kept in the returned ledger, and the plugin
/// scripts that could not be bound.
#[allow(clippy::type_complexity)]
fn bind_session_scripts(
    builder: Builder,
    settings_snapshot: &Arc<LoadedSettings>,
    script_root: &Path,
    package_scripts: &[crate::plugins::PackageScripts],
) -> (
    Builder,
    Option<Arc<base::interface::script::ScriptLedger>>,
    Vec<(String, String)>,
) {
    #[cfg_attr(not(feature = "scripts"), allow(unused_mut))]
    let mut builder = builder;
    let script_faults: Vec<(String, String)>;
    #[cfg(feature = "scripts")]
    let script_ledger = {
        let (bound, faults) = bind_scripts(settings_snapshot, script_root, package_scripts);
        script_faults = faults;
        match bound {
            Some(bound) => {
                let ledger = bound.ledger.clone();
                builder = builder.plugin(Arc::new(script_host::ScriptPlugin::new(bound)));
                Some(ledger)
            }
            None => None,
        }
    };
    #[cfg(not(feature = "scripts"))]
    let script_ledger = {
        let (_, faults) = bind_scripts(settings_snapshot, script_root, package_scripts);
        script_faults = faults;
        None
    };
    (builder, script_ledger, script_faults)
}

fn build_failed(e: runtime::agent::EngineError) -> CreateError {
    match e {
        runtime::agent::EngineError::HostTools(error) => {
            CreateError::InvalidOptions(vec![crate::session_pool::api::InvalidOption::new(
                "host_tools",
                error,
            )])
        }
        runtime::agent::EngineError::OutputSchema(error) => {
            CreateError::InvalidOptions(vec![crate::session_pool::api::InvalidOption::new(
                "output_schema",
                error,
            )])
        }
        e => CreateError::Failed(format!("build agent: {e}")),
    }
}
