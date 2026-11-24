//! Phase 13: the `Agent` literal, one sub-struct at a time.

use super::*;

impl Builder {
    pub(super) fn assemble(mut self, w: Wired) -> (Agent, EventReceiver, InputSender) {
        w.events
            .host_tools
            .set(self.admitted_host_tools(&w.tool_policy));
        let chains = w.chains;
        let (recovery_policy, turn_policy, budget_policy) =
            self.policies(&w.resolved.settings, &w.resolved.scene);
        let settings = w.resolved.settings;
        let gate = self.gate(
            w.resolved.permission,
            &w.events,
            &w.hooks,
            &w.ambient,
            chains.turn_starts,
        );
        let task_router = self.observed_router(
            &w.events.tx,
            &w.resolved.telemetry_handle,
            &w.session.session_id,
        );
        let session_end = SessionEnd {
            ended: w.hooks.session_ended,
            stopping: CancellationToken::new(),
            tools: w.resolved.tools.clone(),
            state: w.session_state.clone(),
            attachment: w.attachment,
            reason: EndReasonSlot::default(),
        };
        let agent = Agent {
            scene: w.resolved.scene,
            exec: w.exec,
            engine_git: w.engine_git,
            config: w.config,
            session: w.session,
            session_state: w.session_state,
            current_turn_id: String::new(),
            session_end,
            identity: self.identity(w.agent_depth, w.resolved.attended),
            llm: ModelRoute::new(
                w.resolved.model,
                task_router,
                recovery_policy,
                chains.model_interceptors,
            ),
            tooling: Tooling::new(
                w.resolved.tools,
                Arc::new(w.skill_catalog),
                w.command_registry,
                w.mcp,
                Arc::new(w.hooks.delegator),
                chains.tool_middleware,
                chains.result_transformers,
                w.tool_policy,
                w.checkpoints,
            ),
            gate,
            prompt: self.prompt_context(
                chains.prompt_registry,
                chains.prompt_middleware,
                w.resolved.claude_md_content,
                w.resolved.rules,
            ),
            policies: Policies {
                turn_policy,
                budget_policy,
                health: Arc::new(base::interface::health::HealthChecks::from_vec(
                    chains.health_checks,
                )),
                environment: w.resolved.environment,
            },
            memory: self.memory(w.resolved.memory_store, chains.retrieval_hooks),
            compaction: Compaction::new(
                w.compactor,
                settings.values.compaction.clear_on_cache_expiry,
            ),
            session_config: w.resolved.session_config,
            turn: Default::default(),
            charged_to: None,
            open_turn: w.open_turn,
            last_had_tool_uses: true,
            repeat_guard: Default::default(),
            telemetry: Telemetry::new(w.resolved.telemetry_handle),
            channels: Channels {
                input_rx: w.input_rx,
                event_tx: w.events.tx,
                current_turn_cancel: Default::default(),
                steering: self.spec.steering.take().map(|s| s.0).unwrap_or_default(),
                pending_host_calls: w.events.pending_host_calls,
                pending_host_progress: w.events.pending_host_progress,
                host_calls: w.events.host_calls,
            },
        };
        (agent, w.events.rx, w.input_tx)
    }

    /// The router, its switches reported on this session's events and
    /// telemetry.
    fn observed_router(
        &mut self,
        events: &crate::agent::messages::EventSender,
        telemetry: &telemetry::TelemetryHandle,
        session_id: &str,
    ) -> Option<Arc<dyn base::provider::TaskRouting>> {
        let router = self.host.task_router.take()?;
        router.observe(Arc::new(crate::agent::instruments::SwitchReporter {
            events: events.clone(),
            telemetry: telemetry.clone(),
            session_id: session_id.to_string(),
        }));
        Some(router)
    }

    pub(super) fn policies(
        &self,
        settings: &LoadedSettings,
        scene: &SceneSpec,
    ) -> (
        Arc<dyn base::interface::recovery_policy::RecoveryPolicy>,
        Arc<dyn base::interface::turn_policy::TurnPolicy>,
        Arc<dyn base::interface::budget_policy::BudgetPolicy>,
    ) {
        let recovery_policy: Arc<dyn base::interface::recovery_policy::RecoveryPolicy> =
            self.host.recovery_policy.clone().unwrap_or_else(|| {
                Arc::new(base::interface::recovery_policy::DefaultRecovery::default())
            });
        let turn_policy: Arc<dyn base::interface::turn_policy::TurnPolicy> =
            self.ext.turn_policy.clone().unwrap_or_else(|| {
                Arc::new(base::interface::turn_policy::LimitsPolicy::new(
                    settings.values.turn.max_model_calls,
                    scene.execution.max_api_calls_per_turn,
                    5,
                ))
            });
        let budget_policy: Arc<dyn base::interface::budget_policy::BudgetPolicy> = self
            .ext
            .budget_policy
            .clone()
            .unwrap_or_else(|| Arc::new(settings.values.engine_budget()));
        (recovery_policy, turn_policy, budget_policy)
    }

    pub(super) fn identity(&mut self, agent_depth: u32, attended: bool) -> Identity {
        Identity {
            agent_depth,
            agent_type: self.spec.agent_type.take(),
            team_seat: self.spec.team_seat.take(),
            drive_goals: !self.spec.goal_rounds_from_owner,
            skip_warmup: self.spec.skip_warmup,
            attended,
            start_source: None,
        }
    }

    pub(super) fn gate(
        &mut self,
        permission: PermissionSet,
        events: &EventWiring,
        hook_wiring: &HookWiring,
        ambient: &hooks::payload::Ambient,
        turn_starts: Vec<crate::plugin::TurnStart>,
    ) -> Gate {
        let elicitation = self.ext.elicitation.take().unwrap_or_else(|| {
            let mut channel = crate::elicitation::ChannelElicitation::new(
                events.tx.clone(),
                events.pending_permissions.clone(),
            );
            if self.spec.question_prompts {
                channel = channel.with_questions(events.pending_questions.clone());
            }
            if self.spec.mcp_prompts {
                channel = channel.with_mcp(events.pending_mcp.clone());
            }
            Arc::new(channel)
        });
        let elicitation = crate::elicitation_hooks::hooked(
            elicitation,
            &hook_wiring.hooks,
            ambient,
            &hook_wiring.session_ended,
        );
        Gate::new(
            permission,
            elicitation,
            events.pending_permissions.clone(),
            events.pending_questions.clone(),
            events.pending_mcp.clone(),
            hook_wiring.hooks.clone(),
            turn_starts,
        )
    }

    pub(super) fn prompt_context(
        &mut self,
        prompt_registry: Arc<dyn base::interface::prompt_registry::PromptRegistry>,
        prompt_middleware: Vec<Arc<dyn base::interface::prompt_middleware::PromptMiddleware>>,
        claude_md_content: Option<String>,
        rules: Arc<dyn base::interface::instruction_provider::RuleProvider>,
    ) -> PromptContext {
        PromptContext::new(
            prompt_registry,
            self.ext
                .prompt_assembler
                .take()
                .unwrap_or_else(|| Arc::new(crate::prompt_assembly::DefaultAssembler)),
            prompt_middleware,
            self.spec.frozen.take(),
            claude_md_content,
            rules,
        )
    }

    pub(super) fn memory(
        &mut self,
        memory_store: Arc<MemoryStore>,
        retrieval_hooks: Vec<Arc<dyn base::interface::memory_contracts::RetrievalHook>>,
    ) -> Memory {
        Memory::new(
            memory_store,
            self.host
                .memory_retriever
                .take()
                .unwrap_or_else(|| Arc::new(memory::LlmRetriever)),
            retrieval_hooks,
            self.spec.memory_extraction,
        )
    }
}
