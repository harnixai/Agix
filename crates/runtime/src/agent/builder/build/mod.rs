//! `Builder::build`: the phases that assemble the registry, session state, modules and the `Agent` itself, in order.

use super::*;

mod assemble;
mod background;
mod modules;
mod plugins;
mod resolve;
mod toolset;
mod types;
mod wiring;

use background::{spawn_cron, spawn_task_hooks};
use modules::ModuleInputs;
use types::*;
use wiring::bind_tools;

impl Builder {
    pub fn build(mut self) -> Result<(Agent, EventReceiver, InputSender), EngineError> {
        let host_layers = self.host_layers();
        let questions_answered = self.questions_answered();
        let mut r = self.resolve()?;
        let session = self.open_session(&r.settings)?;
        let checkpoints = self.session_checkpoints(&r.settings, &session);
        let compactor = self
            .host
            .compactor
            .take()
            .unwrap_or_else(|| Arc::new(DefaultCompactor) as Arc<dyn Compactor>);
        let agent_depth = self.spec.agent_depth;
        let mut plugins = self.register_plugins(&session, &r.settings, &r.scene)?;
        let (config, exec) = self.config_and_exec(&r.settings, &r.scene)?;
        let mut hooks = self.wire_hooks(&r.settings, &r.model, &session, &exec);
        let skill_catalog = crate::module::LateSkillCatalog::default();

        let state = self.wire_session_state(
            &r.settings,
            &config,
            &r.scene,
            &session,
            &hooks.hooks,
            r.instruction_file.as_ref(),
            exec,
        )?;
        let cron =
            self.register_session_tools(&r.tools, &r.settings, &r.model, &r.telemetry_handle);

        let events = self.wire_events();
        let mcp_slot = self.ext.mcp_servers.take();
        let mcp = self.run_modules(ModuleInputs {
            resolved: &r,
            config: &config,
            pending_permissions: &events.pending_permissions,
            hooks: &hooks.hooks,
            ambient: &state.ambient,
            events: &events.tx,
            session: &session,
            session_state: &state.state,
            depth: agent_depth,
            plugin_agent_types: &plugins.agent_types,
            delegator: &hooks.delegator,
            skill_catalog: &skill_catalog,
            session_ended: &hooks.session_ended,
            exec: &state.exec,
            host_layers,
            mcp: mcp_slot,
            checkpoints: checkpoints.as_ref(),
            host_calls: &events.host_calls,
        })?;
        let command_registry = self.command_registry(&skill_catalog, mcp.as_ref());

        let plugin_tools = plugins.take_tools();
        let surface = self.finalize_tools(
            r.tools,
            &r.scene,
            plugin_tools,
            &plugins.tool_owners,
            questions_answered,
        )?;
        r.tools = surface.tools;

        spawn_task_hooks(
            &hooks.hooks,
            &state.state,
            &state.ambient,
            &hooks.session_ended,
            hooks.task_wake.take(),
        );
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        spawn_cron(cron.store, cron.fire_here, &input_tx);
        let attachment = bind_tools(
            state.env,
            &state.exec,
            state.generation,
            &r.tools,
            &state.state,
            state.injected,
        );

        let chains = plugins.into_chains(&mut self);
        Ok(self.assemble(Wired {
            resolved: r,
            config,
            session,
            session_state: state.state,
            exec: state.exec,
            engine_git: state.engine_git,
            attachment,
            hooks,
            events,
            input_tx,
            input_rx,
            skill_catalog,
            command_registry,
            mcp,
            tool_policy: surface.policy,
            compactor,
            agent_depth,
            chains,
            checkpoints,
        }))
    }

    fn session_checkpoints(
        &mut self,
        settings: &LoadedSettings,
        session: &Transcript,
    ) -> Option<Arc<crate::checkpoint::Checkpoints>> {
        match self.spec.checkpoints.take() {
            Some(decided) => decided,
            None if self.spec.agent_depth == 0 => {
                let store = self.host.history_store.clone()?;
                let project_root = settings
                    .paths
                    .project_root()
                    .map(permissions::path_rule::canonicalize_best_effort);
                crate::checkpoint::Checkpoints::for_root(
                    store,
                    session.session_id_str(),
                    project_root,
                )
            }
            None => None,
        }
    }
}
