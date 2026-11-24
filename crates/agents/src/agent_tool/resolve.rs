//! What a delegate is resolved from its parent: its tools, prompt, model,
//! settings, scene, MCP grant and permission.

use super::*;

type Route = Arc<dyn base::provider::TaskRouting>;

impl Delegator {
    fn router<'a>(&'a self, own: Option<&'a Route>) -> Option<&'a Route> {
        own.or(self.env.host.task_router.as_ref())
    }

    /// The model instance a sub-agent spawn should use. Every spawn is the
    /// `subagent` task, whatever its `subagent_type`: `model.tasks` is keyed
    /// by the kind of call, not the kind of agent.
    pub(crate) fn model_for_subagent(&self, own: Option<&Route>) -> Arc<dyn Model> {
        match self.router(own) {
            Some(router) => router.model_for(SUBAGENT),
            None => self.env.model.clone(),
        }
    }

    /// `None` shares the parent's route.
    pub(crate) fn own_route(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
        def: Option<&AgentTypeDefinition>,
    ) -> Result<Option<Route>, String> {
        let Some(parent) = &self.env.host.task_router else {
            return match provider {
                Some(id) => Err(format!(
                    "provider `{id}` is not configured: this session routes no providers"
                )),
                None => Ok(None),
            };
        };
        if let Some(id) = provider {
            return parent.starting_at(id).map(Some);
        }
        if let Some(def) = def {
            if let Some(id) = &def.provider {
                match parent.starting_at(id) {
                    Ok(route) => return Ok(Some(route)),
                    Err(error) => tracing::warn!(
                        agent_type = %def.name,
                        provider = %id,
                        %error,
                        "agent type names a provider this session does not define; ignoring it"
                    ),
                }
            }
        }
        let Some(model) = model.or_else(|| def.and_then(|d| d.model.as_deref())) else {
            return Ok(None);
        };
        if model == "inherit" || tier_named(model).is_some() {
            return Ok(None);
        }
        match parent.home_of(model) {
            Some(home) if parent.destination(SUBAGENT, model).provider != home => {
                parent.starting_at(&home).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// `detail` with where `spec`'s calls are expected to go: the head of its
    /// route and the model asked there. The first call may correct it.
    pub(crate) fn placed(&self, spec: &Spec, mut detail: Value) -> Value {
        let def = spec
            .agent_type
            .as_deref()
            .and_then(|t| self.agent_type_def(t));
        let named = spec.model.clone().or_else(|| def.and_then(|d| d.model));
        let model = self.delegate_model(spec.route.as_ref(), named.as_deref());
        match self.router(spec.route.as_ref()) {
            Some(router) => {
                let to = router.destination(SUBAGENT, &model);
                place(&mut detail, to.model, Some(to.provider));
            }
            None => place(&mut detail, model, None),
        }
        detail
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

    /// A sub-agent's configuration: the parent's, with its own model
    /// ([`Self::delegate_model`]) and the mode a host switched the parent to
    /// since it was built.
    pub(crate) fn sub_settings(
        &self,
        own: Option<&Route>,
        model_name: Option<&str>,
    ) -> SessionConfig {
        let model_name = self.delegate_model(own, model_name);
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

    /// The model `model_name` names ([`Self::named_delegate_model`]), else
    /// [`Self::unnamed_delegate_model`]; on a route of its own, that
    /// model's counterpart at the route's head.
    pub(crate) fn delegate_model(&self, own: Option<&Route>, model_name: Option<&str>) -> String {
        let name = match model_name {
            Some(name) => self.named_delegate_model(own, name),
            None => self.unnamed_delegate_model(own),
        };
        match own {
            Some(route) => route.destination(SUBAGENT, &name).model,
            None => name,
        }
    }

    /// The model of a delegate whose spawn and agent type name none: the
    /// `subagent` task's tier at the first provider — unless that provider
    /// serves it with the conversation's own tier's model, in which case the
    /// delegate follows the parent's model.
    fn unnamed_delegate_model(&self, own: Option<&Route>) -> String {
        let Some(router) = self.router(own) else {
            return self.parent_model();
        };
        match (
            router.model_name_for(SUBAGENT),
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
    fn named_delegate_model(&self, own: Option<&Route>, name: &str) -> String {
        if name == "inherit" {
            return self.parent_model();
        }
        match (tier_named(name), self.router(own)) {
            (Some(tier), Some(router)) => router.resolve_model(tier.as_str()),
            (Some(_), None) => self.parent_model(),
            (None, _) => name.to_string(),
        }
    }

    /// The scene a sub-agent should run in.
    ///
    /// The agent type's own `scene:` when it is no wider than the parent's,
    /// otherwise the parent's. `Err` when the type names a scene the host has
    /// but keeps from this session.
    pub(crate) fn scene_for_subagent(
        &self,
        def: Option<&AgentTypeDefinition>,
    ) -> Result<Arc<SceneSpec>, String> {
        let parent = &self.env.scene;
        if let Some(name) = def.and_then(|d| d.scene.as_deref()) {
            if self
                .env
                .host
                .scene_registry
                .as_ref()
                .is_some_and(|r| r.withholds(name))
            {
                return Err(format!(
                    "agent type `{}` runs in scene `{name}`, which this session does not have",
                    def.map(|d| d.name.as_str()).unwrap_or("")
                ));
            }
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
                        _ => return Ok(requested),
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
        Ok(parent.clone())
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

const SUBAGENT: &str = base::provider::TaskTiers::SUBAGENT;

fn tier_named(name: &str) -> Option<base::provider::Tier> {
    base::provider::Tier::from_claude_alias(name).or_else(|| base::provider::Tier::from_name(name))
}
