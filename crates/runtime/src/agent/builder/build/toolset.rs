//! Phases 5, 7 and 11: the engine's own tools, the session's tools, and the final tool surface.

use super::*;

const STRUCTURED_OUTPUT: &str = "StructuredOutput";

impl Builder {
    /// The model `WebSearch` searches through and the model name it asks for:
    /// the session's own, asking for the model the conversation asks for at
    /// the time of the search, unless `task_models.web_search` routes it.
    fn web_search_model(
        &self,
        settings: &LoadedSettings,
        model: &Arc<dyn Model>,
        telemetry_handle: &TelemetryHandle,
        session_state: &Arc<session_state::SessionState>,
    ) -> (Arc<dyn Model>, Arc<dyn Fn() -> String + Send + Sync>) {
        let routed = self
            .host
            .task_router
            .as_ref()
            .and_then(|router| Some((router, router.model_name_for(WEB_SEARCH_TASK)?)));
        if let Some((router, name)) = routed {
            let name = name.to_string();
            let model = telemetry::model::TelemetryModel::new(
                router.model_for(WEB_SEARCH_TASK),
                telemetry_handle.clone(),
                settings.values.model.model_name.clone(),
            );
            return (Arc::new(model), Arc::new(move || name.clone()));
        }
        let state = Arc::downgrade(session_state);
        let configured = settings.values.model.model_name.clone();
        let asked_for = move || {
            state
                .upgrade()
                .and_then(|state| base::context::Session::model_choice(&*state))
                .unwrap_or_else(|| configured.clone())
        };
        (model.clone(), Arc::new(asked_for))
    }

    pub(super) fn register_session_tools(
        &self,
        r: &Resolved,
        session_state: &Arc<session_state::SessionState>,
    ) -> CronWiring {
        let (tools, settings, telemetry_handle) = (&r.tools, &r.settings, &r.telemetry_handle);
        register_task_tools(tools);
        let agent_depth = self.spec.agent_depth;
        let (search_model, search_model_name) =
            self.web_search_model(settings, &r.model, telemetry_handle, session_state);
        tools::register_web_search(tools, search_model, search_model_name);

        // Extraction runs only on a model routed for it: the session's own
        // model is too costly to spend reading whole pages.
        let fetch_extraction = self.host.task_router.as_ref().and_then(|router| {
            let name = router.model_name_for(WEB_FETCH_TASK)?;
            let model = Arc::new(telemetry::model::TelemetryModel::new(
                router.model_for(WEB_FETCH_TASK),
                telemetry_handle.clone(),
                settings.values.model.model_name.clone(),
            )) as Arc<dyn Model>;
            Some((model, name.to_string()))
        });
        tools::register_web_fetch(tools, fetch_extraction);

        // One registry per session: it holds at most one active worktree.
        tools::register_worktree_tools(
            tools,
            Arc::new(tools::worktree_tools::WorktreeRegistry::new()),
        );

        // Cron tools, and — when no owner handed in a store — the scheduler
        // that drains this session's own. `~/.agix/scenes/<scope>/scheduled_tasks.json`
        // is the exact path `CronCreate`'s own prompt promises the user for
        // `durable: true` jobs; session-only jobs never touch disk. A delegate
        // gets neither: scheduled jobs belong to the session its host keeps,
        // and a delegate's own scheduler would pop that session's durable jobs.
        //
        // Known limitation of the store-per-session case only: two concurrent
        // sessions in the same scope each load the durable file and each
        // schedule their own fire for a durable job. A session pool avoids it
        // by owning one store and one scheduler (`Builder::cron_store`).
        let (cron_store, fire_jobs_here) = match (agent_depth, self.spec.cron_store.clone()) {
            (0, Some(store)) => (Some(store), false),
            (0, None) => {
                let paths = settings.paths.config_paths();
                let _ = std::fs::create_dir_all(&paths.user_data_dir);
                let store = Arc::new(tools::cron::CronStore::load_or_default(Some(
                    paths.scheduled_tasks_file(),
                )));
                (Some(store), true)
            }
            _ => (None, false),
        };
        if let Some(store) = &cron_store {
            tools::register_cron_tools(tools, store.clone());
        }
        CronWiring {
            store: cron_store,
            fire_here: fire_jobs_here,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn finalize_tools(
        &self,
        registry: Arc<InMemoryToolRegistry>,
        scene: &Arc<SceneSpec>,
        layout: &base::paths::Layout,
        plugin_tools: Vec<Arc<dyn base::tool::Tool>>,
        plugin_tool_owners: &std::collections::HashMap<String, crate::plugin::ToolOwner>,
        attended: bool,
        questions_answered: bool,
    ) -> Result<SessionTools, EngineError> {
        let tools = &registry;
        self.register_run_ext(tools, layout);
        let host_tools = crate::host_tools::declared_tools(&self.spec.host_tools)?;
        let host_names = admit_extra_tools(tools, scene, host_tools, plugin_tools)?;

        let host_schema = self.spec.output_schema.clone();
        let tool_policy = Arc::new(crate::tool_policy::ToolPolicy::new(
            scene,
            self.spec.tool_selector.clone(),
            plugin_tool_owners
                .iter()
                .filter(|(_, owner)| owner.admitted)
                .map(|(name, _)| name.clone())
                .chain(host_names)
                .chain(host_schema.is_some().then(|| STRUCTURED_OUTPUT.to_string())),
        ));

        match &host_schema {
            Some(schema) => tools::bind_output_schema(tools, schema),
            None => {
                tools.remove(STRUCTURED_OUTPUT);
            }
        }
        // The tool policy, applied to the registry itself rather than
        // only to what is advertised: `ToolSearch` and dispatch read this same
        // registry, so a tool the scene excludes has to be absent from it.
        for t in tools.list() {
            let waits_on_a_reply = base::interface::scene::NEEDS_A_USER.contains(&t.name())
                && (!attended || (t.name() == ASK_USER_QUESTION && !questions_answered));
            let excluded = !tool_policy.permits(t.as_ref());
            if waits_on_a_reply || excluded {
                tools.remove(t.name());
                tracing::debug!(
                    tool = t.name(),
                    scene = %scene.id,
                    plugin = plugin_tool_owners.get(t.name()).map(|o| o.plugin.as_str()),
                    reason = if excluded {
                        "not permitted by the session's tool policy"
                    } else {
                        "waits on a reply no one will give"
                    },
                    "tool removed from the session"
                );
            }
        }

        // Last, so its description lists the tools this session ended up
        // with. Only the root session writes programs: a sub-agent is already
        // one delegation down.
        #[cfg(feature = "ptc")]
        if self.spec.agent_depth == 0 && tool_policy.scene_permits(ptc::TOOL_NAME) {
            tools.register(self.run_code_for(tools.as_ref(), layout));
        }

        if host_schema.is_some() && tools.get(STRUCTURED_OUTPUT).is_none() {
            return Err(EngineError::OutputSchema(format!(
                "scene {:?} denies {STRUCTURED_OUTPUT}, the tool the session's structured \
                 output is handed back with",
                scene.id
            )));
        }

        report_duplicates(tools);
        Ok(SessionTools {
            tools: registry,
            policy: tool_policy,
        })
    }
}

impl Builder {
    /// `RunExt`, when enabled packages bring commands: ahead of the tools
    /// admitted with a collision check, so none of them can take its name.
    fn register_run_ext(&self, tools: &InMemoryToolRegistry, layout: &base::paths::Layout) {
        #[cfg(feature = "ptc")]
        if !self.host.ext_commands.is_empty() {
            let run_ext = ptc::RunExtTool::new(self.host.ext_commands.clone())
                .with_library(ptc::Library::for_engine(layout.engine()));
            tools.register(Arc::new(match self.spec.run_slots.clone() {
                Some(slots) => run_ext.with_run_slots(slots),
                None => run_ext,
            }));
        }
        #[cfg(not(feature = "ptc"))]
        let _ = (tools, layout);
    }
}

/// Registers the scene's, the session's host, the plugins' and the host
/// connections' tools, in that order, and returns the names of the host
/// tools registered. A host connection's tool whose name is taken is left
/// out with a warning: it must not keep the session from being built.
pub(super) fn admit_extra_tools(
    tools: &InMemoryToolRegistry,
    scene: &SceneSpec,
    (host_tools, provided_tools): crate::host_tools::DeclaredTools,
    plugin_tools: Vec<Arc<dyn base::tool::Tool>>,
) -> Result<Vec<String>, EngineError> {
    // Scene-contributed tools (`SceneSpec::extra_tools`, default empty).
    //
    // Deliberately last. The collision check is the whole point of this
    // block, and it can only see names already in the registry — running
    // it before `Agent`/`Skill`/`WebSearch`/`Cron*`/`Worktree*` are
    // registered would let a scene claim one of those names, pass the
    // check against an empty slot, and then win the lookup anyway
    // (`get` returns the first match, and the engine's own instance is
    // appended after). That is precisely the shadowing this rejects.
    for t in scene.extra_tools.iter().cloned() {
        if taken(tools, t.name()) {
            return Err(EngineError::Internal(format!(
                "scene {:?} contributes a tool named {:?}, which is already \
                     registered; rename the scene's tool",
                scene.id,
                t.name()
            )));
        }
        tools.register(t);
    }

    let mut host_names = Vec::new();
    for t in host_tools {
        if taken(tools, t.name()) {
            return Err(EngineError::HostTools(format!(
                "the host declares a tool named {:?}, which is already registered",
                t.name()
            )));
        }
        host_names.push(t.name().to_string());
        tools.register(t);
    }

    // After the scene's tools. The same `Arc` arriving twice is not a
    // collision; a different tool under a taken name is.
    for t in plugin_tools {
        match tools.get(t.name()) {
            Some(existing) if Arc::ptr_eq(&existing, &t) => continue,
            Some(_) => {
                return Err(EngineError::Internal(format!(
                    "a plugin contributes a tool named {:?}, which is already registered by \
                         something else",
                    t.name()
                )));
            }
            None if taken(tools, t.name()) => {
                return Err(EngineError::Internal(format!(
                    "a plugin contributes a tool named {:?}, which the engine registers last",
                    t.name()
                )));
            }
            None => {
                tools.register(t);
            }
        }
    }

    for t in provided_tools {
        if taken(tools, t.name()) {
            tracing::warn!(
                tool = t.name(),
                "a host connection's tool is left out of the session: the name is taken"
            );
            continue;
        }
        host_names.push(t.name().to_string());
        tools.register(t);
    }
    Ok(host_names)
}

fn report_duplicates(tools: &InMemoryToolRegistry) {
    let names = tools.names();
    let mut seen = std::collections::HashSet::new();
    let twice: Vec<&String> = names.iter().filter(|n| !seen.insert(n.as_str())).collect();
    debug_assert!(twice.is_empty(), "tools registered twice: {twice:?}");
    if !twice.is_empty() {
        tracing::error!(
            tools = ?twice,
            "tools registered twice; dispatch reaches only the first of each"
        );
    }
}

/// Whether `name` is in the registry or is one `finalize_tools` registers
/// after these, without a collision check of its own.
fn taken(tools: &InMemoryToolRegistry, name: &str) -> bool {
    #[cfg(feature = "ptc")]
    if name == ptc::TOOL_NAME {
        return true;
    }
    tools.get(name).is_some()
}

pub(super) fn register_task_tools(tools: &InMemoryToolRegistry) {
    tools.register(std::sync::Arc::new(tools::task_stop::TaskStopTool));
    tools.register(std::sync::Arc::new(tools::task_output::TaskOutputTool));
    tools.register(std::sync::Arc::new(tools::task_wait::TaskWaitTool));
    tools.register(std::sync::Arc::new(tools::task_message::TaskMessageTool));
}
