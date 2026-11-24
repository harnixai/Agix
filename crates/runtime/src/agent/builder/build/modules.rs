//! Phases 9 and 10: running the modules' `on_session`, then the command registry.

use super::*;

pub(super) struct ModuleInputs<'a> {
    pub(super) resolved: &'a Resolved,
    pub(super) config: &'a Arc<base::context::EngineConfig>,
    pub(super) hooks: &'a Arc<HookRunner>,
    pub(super) ambient: &'a hooks::payload::Ambient,
    pub(super) events: &'a EventWiring,
    pub(super) session: &'a Transcript,
    pub(super) session_state: &'a Arc<session_state::SessionState>,
    pub(super) depth: u32,
    pub(super) plugin_agent_types: &'a [base::agent_type::AgentTypeDefinition],
    pub(super) delegator: &'a crate::module::LateDelegator,
    pub(super) skill_catalog: &'a crate::module::LateSkillCatalog,
    pub(super) session_ended: &'a CancellationToken,
    pub(super) exec: &'a base::interface::exec::ExecProviders,
    pub(super) host_layers: HostLayers,
    pub(super) mcp: Option<Box<dyn base::interface::mcp_servers::McpServers>>,
    pub(super) checkpoints: Option<&'a Arc<crate::checkpoint::Checkpoints>>,
}

impl Builder {
    pub(super) fn run_modules(
        &self,
        inputs: ModuleInputs<'_>,
    ) -> Result<Box<dyn base::interface::mcp_servers::McpServers>, EngineError> {
        let ModuleInputs {
            resolved,
            config,
            hooks,
            ambient,
            events,
            session,
            session_state,
            depth,
            plugin_agent_types,
            delegator,
            skill_catalog,
            session_ended,
            exec,
            host_layers,
            mcp: mut mcp_slot,
            checkpoints,
        } = inputs;
        let Resolved {
            tools,
            template,
            settings,
            session_config,
            scene,
            model,
            modules,
            ..
        } = resolved;
        // Dispatch looks tools up in the registry, so a grant's adapters live
        // there as an mcp module's do.
        for adapter in mcp_slot.iter().flat_map(|m| m.tools()) {
            tools.register(adapter);
        }
        {
            let session_id = session.session_id_str().to_string();
            let host = self.host_contracts(resolved, session, exec, host_layers);
            let mut cx = crate::module::SessionContext {
                tools,
                template_tools: template,
                settings,
                session_config,
                config,
                scene,
                model,
                pending_permissions: &events.pending_permissions,
                pending_mcp_answers: self.spec.mcp_prompts.then_some(&events.pending_mcp),
                elicitation: self.ext.elicitation.as_ref(),
                hooks,
                hook_ambient: ambient,
                events: &events.tx,
                session_id: &session_id,
                session_state: &(session_state.clone() as Arc<dyn base::context::Session>),
                depth,
                host: &host,
                plugin_agent_types,
                mcp: &mut mcp_slot,
                delegator,
                skill_catalog,
                session_ended,
                checkpoints,
                host_calls: &events.host_calls,
                host_tools: &events.host_tools,
            };
            for module in modules {
                module
                    .on_session(&mut cx)
                    .map_err(|e| EngineError::Internal(e.to_string()))?;
            }
        }
        Ok(mcp_slot.unwrap_or_else(|| Box::new(base::interface::mcp_servers::NoMcpServers)))
    }

    /// `tools` is the session's final registry: a plugin command is a `/name`
    /// only where `RunExt` is there to run it.
    /// What this session resolved for the host, as every module and
    /// delegate is shown it.
    fn host_contracts(
        &self,
        resolved: &Resolved,
        session: &Transcript,
        exec: &base::interface::exec::ExecProviders,
        host_layers: HostLayers,
    ) -> HostContracts {
        HostContracts {
            exec: exec.clone(),
            token_counter: session.token_counter().clone(),
            rules: resolved.rules.clone(),
            environment: resolved.environment.clone(),
            permission: resolved.permission.clone(),
            history_store: self.host.history_store.clone(),
            telemetry: resolved.telemetry_handle.clone(),
            task_router: self.host.task_router.clone(),
            scene_registry: self.host.scene_registry.clone(),
            memory_store: resolved.memory_store.clone(),
            memory_retriever: self.host.memory_retriever.clone(),
            retrieval_hooks: self.host.retrieval_hooks.clone(),
            instruction_provider: resolved.instruction_provider.clone(),
            plugins: self.host.plugins.clone(),
            modules: resolved.modules.to_vec(),
            host_layers,
            attended: resolved.attended,
            skill_dirs: self.host.skill_dirs.clone(),
            ext_commands: self.host.ext_commands.clone(),
            host_js_libs: self.host.host_js_libs.clone(),
        }
    }

    pub(super) fn command_registry(
        &self,
        skill_catalog: &crate::module::LateSkillCatalog,
        mcp: &dyn base::interface::mcp_servers::McpServers,
        tools: &dyn base::tool::ToolRegistry,
    ) -> Arc<crate::commands::CommandRegistry> {
        let mut registry =
            crate::commands::CommandRegistry::from_skill_catalog(Arc::new(skill_catalog.clone()));
        if tools.get(crate::commands::EXT_TOOL_NAME).is_some() {
            registry.register_ext_commands(&self.host.ext_commands);
        }
        let mcp_prompts = mcp.prompt_commands();
        if !mcp_prompts.is_empty() {
            tracing::info!(
                count = mcp_prompts.len(),
                "registering MCP prompts as slash commands"
            );
            registry.register_mcp_prompts(&mcp_prompts);
        }
        std::sync::Arc::new(registry)
    }
}
