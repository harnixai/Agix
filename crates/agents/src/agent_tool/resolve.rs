//! What a delegate is resolved from its parent: its tools, prompt, model,
//! settings, scene, MCP grant and permission.

use super::*;

impl Delegator {
    /// The model instance a sub-agent spawn should use. Every spawn is the
    /// `subagent` task, whatever its `subagent_type`: `model.tasks` is keyed
    /// by the kind of call, not the kind of agent.
    pub(crate) fn model_for_subagent(&self) -> Arc<dyn Model> {
        match &self.env.host.task_router {
            Some(router) => router.model_for("subagent"),
            None => self.env.model.clone(),
        }
    }

    pub(crate) fn agent_type_def(&self, name: &str) -> Option<AgentTypeDefinition> {
        self.catalog.get(name)
    }

    /// The MCP servers a delegate of type `def` is granted: the parent's named
    /// in the type's `mcp_servers`. Which of their tools it is offered is its
    /// tool policy's to decide, as for every other tool.
    pub(crate) fn mcp_granted_to(
        &self,
        def: Option<&AgentTypeDefinition>,
    ) -> Option<Box<dyn base::interface::mcp_servers::McpServers>> {
        let def = def.filter(|d| !d.mcp_servers.is_empty())?;
        let Some(parent) = &self.env.mcp else {
            tracing::warn!(
                agent_type = %def.name,
                "agent type names mcp_servers, but this session has no MCP servers to grant"
            );
            return None;
        };
        Some(parent.granted(Some(&def.mcp_servers)))
    }

    /// How the delegate `spec` describes asks about its calls: of the
    /// parent's policy, with `rules` — its deny rules and the grants it starts
    /// with — besides; its questions go to the parent's host as `task`'s, or
    /// are refused for the reason `no_one_to_ask` gives.
    pub(crate) fn relay_of(
        &self,
        spec: &Spec,
        task: Option<Arc<base::context::RunningTask>>,
        no_one_to_ask: Option<&'static str>,
        rules: Vec<base::permission::PermissionRule>,
    ) -> Relay {
        let agent_type = spec.agent_type.as_deref();
        self.relay(&spec.session_id, agent_type, task, no_one_to_ask, rules)
    }

    /// [`Self::relay_of`] the delegate `delegate_id`, of `agent_type`.
    pub(crate) fn relay(
        &self,
        delegate_id: &str,
        agent_type: Option<&str>,
        task: Option<Arc<base::context::RunningTask>>,
        no_one_to_ask: Option<&'static str>,
        rules: Vec<base::permission::PermissionRule>,
    ) -> Relay {
        let env = &self.env;
        let turn_grants = Arc::new(TurnGrants::default());
        let tightens_only = agent_type
            .and_then(|name| self.agent_type_def(name))
            .is_some_and(|def| def.permission_mode.is_some() && !def.source.may_loosen());
        let relay = Arc::new(DelegateRelay {
            parent: env.host.permission.policy.clone(),
            parent_denials: env.host.permission.path_denials.clone(),
            parent_session: env.parent.state.clone(),
            no_one_to_ask,
            rules,
            turn_grants: turn_grants.clone(),
            tightens_only,
        });
        Relay {
            permission: PermissionSet {
                policy: relay.clone(),
                path_denials: Some(relay),
                grants: env.host.permission.grants.clone().map(|parent| {
                    Arc::new(ParentGrants {
                        parent,
                        turn: turn_grants,
                    }) as Arc<dyn base::interface::permission::PermissionGrants>
                }),
            },
            elicitation: Arc::new(ParentElicitation {
                delegate_id: delegate_id.to_string(),
                route: match &env.parent.asks_upstream {
                    Some(upstream) => QuestionRoute::Upstream(upstream.clone()),
                    None => QuestionRoute::Channel {
                        events: env.parent.events.clone(),
                        pending: env.parent.pending_permissions.clone(),
                        mcp: env.parent.pending_mcp_answers.clone(),
                    },
                },
                task: task.clone(),
            }),
            host_calls: Arc::new(runtime::host_tools::DelegateHostCalls::new(
                env.host_calls.clone(),
                task.map(|task| task.task_id.clone()),
                delegate_id,
            )),
        }
    }

    /// A sub-agent's configuration: the parent's, with its own model — the
    /// one `model_name` names ([`Self::named_delegate_model`]), else
    /// [`Self::unnamed_delegate_model`] — and the mode a host switched the
    /// parent to since it was built.
    pub(crate) fn sub_settings(&self, model_name: Option<&str>) -> SessionConfig {
        let model_name = match model_name {
            Some(name) => self.named_delegate_model(name),
            None => self.unnamed_delegate_model(),
        };
        let config = (*self.env.session_config)
            .clone()
            .with_model_name(model_name);
        match self.env.parent.state.host_permission_mode() {
            Some(mode) => config.with_permission_mode(mode),
            None => config,
        }
    }

    /// The model the parent session's next call asks for — a host's choice
    /// made since the session was built is not in its settings.
    fn parent_model(&self) -> String {
        self.env
            .parent
            .state
            .model_choice()
            .unwrap_or_else(|| self.env.config.model.clone())
    }

    /// The model of a delegate whose spawn and agent type name none: the
    /// `subagent` task's tier at the first provider — unless that provider
    /// serves it with the conversation's own tier's model, in which case the
    /// delegate follows the parent's model.
    fn unnamed_delegate_model(&self) -> String {
        let Some(router) = &self.env.host.task_router else {
            return self.parent_model();
        };
        match (
            router.model_name_for("subagent"),
            router.model_name_for("main"),
        ) {
            (Some(subagent), Some(main)) if subagent != main => subagent,
            _ => self.parent_model(),
        }
    }

    /// The model `name` asks for: `inherit` is the parent's; a tier —
    /// `strong`, `balanced`, `fast`, or Claude Code's `opus`, `sonnet`,
    /// `haiku` — the first provider's model of that tier; anything else a
    /// model id.
    fn named_delegate_model(&self, name: &str) -> String {
        if name == "inherit" {
            return self.parent_model();
        }
        let tier = base::provider::Tier::from_claude_alias(name)
            .or_else(|| base::provider::Tier::from_name(name));
        match (tier, &self.env.host.task_router) {
            (Some(tier), Some(router)) => router.resolve_model(tier.as_str()),
            (Some(_), None) => self.parent_model(),
            (None, _) => name.to_string(),
        }
    }

    /// The scene a sub-agent should run in.
    ///
    /// The agent type's own `scene:` when it is no wider than the parent's,
    /// otherwise the parent's.
    pub(crate) fn scene_for_subagent(&self, def: Option<&AgentTypeDefinition>) -> Arc<SceneSpec> {
        let parent = &self.env.scene;
        if let Some(name) = def.and_then(|d| d.scene.as_deref()) {
            match self.resolve_scene_id(name) {
                Some(requested) => {
                    // An agent type may *narrow* the scene, never widen it.
                    //
                    // Agent types are loaded from `.agix/agents/*.md`, which
                    // is repository content — on a cloned repo it is
                    // attacker-supplied. A scene's allow list is the
                    // enforcement point for "this session may not run shell
                    // commands" (the `harnix` and `mini` scenes rely on it), so
                    // letting a file in the repo pick the scene meant a narrow
                    // session could
                    // spawn `Agent(subagent_type: "x")` where `x.md` said
                    // `scene: code` and get `Bash` back. The boundary the
                    // parent scene draws has to hold across a spawn.
                    //
                    // "Narrower" is judged by tool surface: the requested
                    // scene is accepted only when everything it exposes is
                    // also exposed by the parent.
                    match parent {
                        p if !requested.tools.selector().within(&p.tools.selector()) => {
                            tracing::warn!(
                                scene = %name,
                                parent_scene = %p.name,
                                agent_type = %def.map(|d| d.name.as_str()).unwrap_or(""),
                                "agent type requested a scene that grants tools the parent scene \
                                 does not; ignoring it and inheriting the parent's scene"
                            );
                        }
                        _ => return requested,
                    }
                }
                None => {
                    tracing::warn!(
                        scene = %name,
                        agent_type = %def.map(|d| d.name.as_str()).unwrap_or(""),
                        "unrecognized scene in agent type definition, inheriting parent's scene instead"
                    );
                }
            }
        }
        parent.clone()
    }

    /// A scene id resolved against the host's registry when one was wired,
    /// otherwise against the built-in set (`scene::builtin`). Plugin scenes (`plugin:<name>`)
    /// only exist in the registry, so without this an agent type naming one
    /// would silently fall through to "unrecognized scene" and inherit the
    /// parent's instead.
    pub(crate) fn resolve_scene_id(&self, id: &str) -> Option<Arc<SceneSpec>> {
        if let Some(registry) = &self.env.host.scene_registry {
            return registry.resolve(id);
        }
        scene::builtin(id, &self.env.session_config.settings().paths.layout).map(Arc::new)
    }
}
