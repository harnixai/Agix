//! Phases 4, 6 and 8: hooks, session state and events, wired before any module runs.

use super::*;

impl Builder {
    pub(super) fn wire_hooks(
        &mut self,
        settings: &LoadedSettings,
        model: &Arc<dyn Model>,
        session: &Transcript,
        exec: &base::interface::exec::ExecProviders,
        telemetry: &TelemetryHandle,
    ) -> HookWiring {
        let delegator = crate::module::LateDelegator::default();
        let session_ended = CancellationToken::new();
        let own_runner = self.ext.hooks.is_none();
        let background = self.ext.background_hooks.take().unwrap_or_default();
        let host_hooks = self.ext.host_hooks.take();
        let hooks = self.ext.hooks.take().unwrap_or_else(|| {
            let mut runner = build_hook_runner(
                settings,
                model.clone(),
                Arc::new(delegator.clone()),
                Some(session.session_id_str().to_string()),
                hook_exec(exec, self.host.exec.is_some(), &settings.paths.layout),
            );
            if let Some(source) = host_hooks {
                runner = runner.with_hosts(source);
            }
            Arc::new(
                runner
                    .with_background(background)
                    .with_observer(hook_run_recorder(telemetry.clone())),
            )
        });
        // A runner handed in belongs to a session above this one, which wired
        // its wake channel already; taking it over would move every rewake of
        // the tree onto this session's tasks.
        let task_wake = match self.ext.wake_rx.take() {
            Some(rx) => {
                hooks.set_wake_receiver(rx);
                None
            }
            None if own_runner && tokio::runtime::Handle::try_current().is_ok() => {
                let (tx, rx) = mpsc::unbounded_channel();
                hooks.set_wake_receiver(rx);
                Some(tx)
            }
            None => None,
        };
        HookWiring {
            delegator,
            session_ended,
            hooks,
            task_wake,
        }
    }

    pub(super) fn engine_config(
        &self,
        settings: &LoadedSettings,
        scene: &SceneSpec,
        session_id: &str,
    ) -> Arc<base::context::EngineConfig> {
        Arc::new({
            let mut c = settings.engine_config();
            let paths = settings.paths.config_paths();
            c.tool_results.dir = self.spec.tool_results_dir.clone().or_else(|| {
                (self.spec.agent_depth == 0 && paths.local_data_dir.is_absolute())
                    .then(|| paths.tool_results_dir(session_id))
            });
            c.path_style = settings
                .values
                .prompt
                .path_style
                .unwrap_or(scene.path_style);
            c.deferred_tools = Arc::new(
                scene
                    .tools
                    .deferred
                    .iter()
                    // `ToolSearch` is the only way back to a deferred schema; a host
                    // that bound an output schema wants `StructuredOutput` whole.
                    .filter(|n| *n != "ToolSearch")
                    .filter(|n| *n != "StructuredOutput" || self.spec.output_schema.is_none())
                    .cloned()
                    .chain(
                        self.spec
                            .host_tools
                            .iter()
                            .filter(|d| d.deferred)
                            .map(|d| d.name.clone()),
                    )
                    .collect(),
            );
            c
        })
    }

    pub(super) fn config_and_exec(
        &self,
        settings: &LoadedSettings,
        scene: &SceneSpec,
        session_id: &str,
    ) -> Result<
        (
            Arc<base::context::EngineConfig>,
            base::interface::exec::ExecProviders,
        ),
        EngineError,
    > {
        let config = self.engine_config(settings, scene, session_id);
        let exec = match (&self.host.exec, &self.spec.process_groups) {
            (Some(_), Some(_)) => Err(EngineError::Internal(
                "Builder::exec and Builder::process_groups are both set; record groups in \
                 the providers you pass (platform::local_providers_with_ledger)"
                    .into(),
            )),
            (Some(exec), None) => Ok(exec.clone()),
            (None, groups) => Ok(crate::exec::providers_for(&config, groups.as_ref())),
        }?;
        Ok((config, exec))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn wire_session_state(
        &self,
        settings: &LoadedSettings,
        config: &Arc<base::context::EngineConfig>,
        attended: bool,
        session: &Transcript,
        hooks: &Arc<HookRunner>,
        instruction_files: &[PathBuf],
        exec: base::interface::exec::ExecProviders,
    ) -> Result<SessionWiring, EngineError> {
        let agent_depth = self.spec.agent_depth;
        // One `SessionState` per session, shared by every tool call: none of
        // its state works unless it survives from one call to the next.
        prepare_scratchpad(&settings.paths.config_paths().scratchpad_dir());
        prepare_local_data_gitignore(&settings.paths);
        prepare_working_dir(&settings.paths);
        let session_env = session_env(settings, config, attended, &self.host.skill_dirs);
        let (session_state, generation, injected_state) = match self.spec.session_state.clone() {
            Some((state, generation)) => (state, generation, true),
            None => (Arc::new(session_state::SessionState::new()), 0, false),
        };
        let hook_ambient = hook_ambient(
            session.session_id_str().to_string(),
            settings.paths.working_dir().display().to_string(),
            agent_depth,
            session_state.clone(),
        );

        // One watcher per session tree, started by its root: a delegate shares
        // the root's runner, and a watcher of its own would fire every
        // `FileChanged` once more for each sub-agent ever spawned. The files a
        // session loads as instructions are added when it loads them
        // (`inject_session_context`); the instruction file is known now.
        if agent_depth == 0 && hooks.has_hooks_for(hooks::HookEvent::FileChanged) {
            if let Err(e) = hooks.enable_file_watching(instruction_files, 300, hook_ambient.clone())
            {
                tracing::warn!(error = %e, "failed to start FileChanged watcher");
            }
        }
        Ok(SessionWiring {
            state: session_state,
            generation,
            injected: injected_state,
            env: session_env,
            mode_ceiling: mode_ceiling(settings),
            engine_git: Arc::new(crate::exec::engine_git_for(
                config,
                &exec,
                session.session_id_str(),
            )),
            exec,
            ambient: hook_ambient,
        })
    }

    pub(super) fn wire_events(&mut self) -> EventWiring {
        let (primary_tx, event_rx) = mpsc::unbounded_channel();
        let event_tx = crate::event_bus::EventBus::with_sinks(
            primary_tx,
            std::mem::take(&mut self.ext.event_sinks),
        );
        // Created before the modules run, because a sub-agent's permission
        // prompts are registered in this same map — see
        // `agents::delegate::DelegateEnv::capture`.
        let pending_permissions: PendingPermissions =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let pending_questions: crate::elicitation::PendingQuestions = Default::default();
        let pending_mcp: crate::elicitation::PendingMcpAnswers = Default::default();
        let pending_host_calls: crate::host_tools::PendingHostCalls = Default::default();
        let pending_host_progress: crate::host_tools::PendingHostProgress = Default::default();
        let host_calls = self.ext.host_calls.take().unwrap_or_else(|| {
            Arc::new(crate::host_tools::ChannelHostCalls::new(
                event_tx.clone(),
                pending_host_calls.clone(),
                pending_host_progress.clone(),
            ))
        });
        EventWiring {
            tx: event_tx,
            rx: event_rx,
            pending_permissions,
            pending_questions,
            pending_mcp,
            pending_host_calls,
            pending_host_progress,
            host_calls,
        }
    }
}

/// Hooks run their commands on the session's process provider. Their HTTP
/// goes through the host's network when the host supplied the providers; the
/// session's own egress policy bounds where the model may reach, which is not
/// where an operator's hook endpoint (often `localhost`) may be.
fn hook_exec(
    exec: &base::interface::exec::ExecProviders,
    host_supplied: bool,
    layout: &base::paths::Layout,
) -> hooks::HookExec {
    hooks::HookExec {
        process: exec.process.clone(),
        network: if host_supplied {
            exec.network.clone()
        } else {
            Arc::new(platform::LocalNetwork::default().with_user_agent(layout.user_agent()))
        },
    }
}

pub(super) fn bind_tools(
    session_env: base::context::SessionEnv,
    ceiling: session_state::ModeCeiling,
    exec: &base::interface::exec::ExecProviders,
    generation: u64,
    tools: &Arc<InMemoryToolRegistry>,
    session_state: &Arc<session_state::SessionState>,
    injected_state: bool,
) -> session_state::EngineAttachment {
    let attachment = session_state::EngineAttachment::new(session_env, exec.clone(), generation)
        .with_tools(&(tools.clone() as Arc<dyn base::tool::ToolRegistry>))
        .with_mode_ceiling(ceiling);
    if !injected_state {
        session_state.attach(attachment.clone());
    }
    attachment
}

/// The widest mode a host could put a session in under `settings`, as the
/// daemon clamps `options.settings.permissions.mode` and `session.setPermissionMode`.
fn mode_ceiling(settings: &LoadedSettings) -> session_state::ModeCeiling {
    let widest = if settings.values.permissions.client_may_loosen {
        base::permission::PermissionMode::BypassPermissions
    } else {
        settings.values.permissions.mode
    };
    session_state::ModeCeiling::new(widest, settings.bypass_disabled())
}

/// What this engine's settings and scene make of the session's env.
pub(super) fn session_env(
    settings: &LoadedSettings,
    config: &base::context::EngineConfig,
    attended: bool,
    skill_dirs: &super::super::host::HostSkillDirs,
) -> base::context::SessionEnv {
    let cwd = settings.paths.working_dir().to_path_buf();
    let resolve = |dir: &std::path::PathBuf| cwd.join(dir);
    let worktree_git = settings.paths.worktree_git();
    base::context::SessionEnv {
        additional_writable_dirs: settings
            .values
            .files
            .extra_dirs
            .iter()
            .map(resolve)
            .chain(config.sandbox.paths.state_working_dirs())
            .chain(
                worktree_git
                    .iter()
                    .flat_map(base::paths::WorktreeGit::writable),
            )
            .collect(),
        read_directories: settings
            .values
            .files
            .read_dirs_listed()
            .map(resolve)
            .chain(config.sandbox.paths.readable_state_root())
            .chain(worktree_git.map(|git| git.dir))
            .collect(),
        read_anywhere: settings.values.files.reads_anywhere(),
        cache_directories: settings.values.sandbox.cache_dirs.paths(&cwd),
        unattended: !attended,
        base_mode: config.permission_mode,
        skill_dirs: skill_dirs.readable.clone(),
        protected_dirs: skill_dirs.protected.clone(),
        refusal_rule_roots: settings
            .paths
            .worktree
            .as_ref()
            .and(settings.paths.project_root())
            .map(std::path::Path::to_path_buf)
            .into_iter()
            .collect(),
        cwd,
    }
}

/// A session without a project works in a directory of its own, which has to
/// exist before its first command runs there.
pub(super) fn prepare_working_dir(paths: &base::paths::PathSettings) {
    if let base::paths::Workspace::Scratch(dir) = &paths.workspace {
        if dir.is_absolute() {
            let _ = std::fs::create_dir_all(dir);
        }
    }
}

/// The project directory's `.gitignore` keeps out what the session will
/// write there for this machine alone ([`base::paths::local_data_gitignore`]),
/// before any of it is written. Without a project the directory is the state
/// root, which no repository holds.
pub(super) fn prepare_local_data_gitignore(paths: &base::paths::PathSettings) {
    let dir = &paths.local_data_dir;
    if let Some(project) = paths.project_root().filter(|_| dir.is_absolute()) {
        base::paths::ensure_project_data_gitignore(dir, project);
    }
}

/// The prompt tells the model the scratchpad is there to write into, so it
/// has to exist before a shell redirect into it; its `.gitignore` keeps it out
/// of the project's git status. A relative `local_data_dir` (tests, embedders
/// that never set `paths`) would land in the process's working directory, so
/// nothing is created then.
pub(super) fn prepare_scratchpad(dir: &std::path::Path) {
    if !dir.is_absolute() || dir.is_dir() {
        return;
    }
    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::write(dir.join(".gitignore"), "*\n");
    }
}
