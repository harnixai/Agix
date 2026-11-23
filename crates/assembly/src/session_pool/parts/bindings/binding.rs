//! 一个 `(project, scene)` 对解析出的一切：合并后的设置、对话模型与按任务的路由、
//! skill/agent 类型/命令目录，以及它们来自哪些设置文件。

use model::client::AnthropicClient;
use settings::LoadedSettings;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::warn;

/// What a per-session settings entry is keyed by: the project it belongs to
/// (`None` = the global no-project tier) and the scene it runs under.
///
/// Both halves matter. The scene contributes a real settings layer
/// (`~/.agix/scenes/<scene>/settings.json`, plus the skills, agents and
/// plugins beside it), so one project under two scenes is two different
/// resolutions, not one.
pub(in crate::session_pool) type ProjectSceneKey = (Option<PathBuf>, String);

/// Which pair a binding was built for, and that pair's epoch in the
/// [`super::BindingCache`] when the build began. The cache bumps a pair's
/// epoch on every invalidation, so a binding — and every session built on
/// it — is current exactly while the two still agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session_pool) struct BindingStamp {
    pub(in crate::session_pool) key: ProjectSceneKey,
    pub(in crate::session_pool) epoch: u64,
}

/// Whether what `key` resolves to was merged from `tier`. The global tier is
/// under every pair; a scene or project tier only under its own.
pub(in crate::session_pool) fn tier_covers(
    tier: &crate::watch_hub::Tier,
    key: &ProjectSceneKey,
) -> bool {
    use crate::watch_hub::Tier;
    match tier {
        Tier::Global => true,
        Tier::Scene(scene) => &key.1 == scene,
        Tier::Project(root) => key.0.as_deref() == Some(root.as_path()),
        Tier::Host(_) => false,
    }
}

/// Everything a session reads that the pair `(project, scene)` decides: the
/// merged settings, and the skills, agent types and commands that live in the
/// directories those settings name.
///
/// One per pair, shared by every session on it. They are here together
/// because they are the same fact stated four times: a binding is built from
/// one `Settings`, and rebuilding it is how any of them changes.
pub(in crate::session_pool) struct ProjectBinding {
    pub(in crate::session_pool) settings: Arc<LoadedSettings>,
    /// Where a session on this pair sends its model calls. `providers` and
    /// `model` are settings like any other, so they are decided by the pair: a
    /// project that configures its own provider is served by it.
    pub(in crate::session_pool) task_router: Arc<model::routing::ProviderSet>,
    /// What building `task_router` had to say — reported by `config.reload`
    /// and `config.setProvider` for the pair they targeted.
    pub(in crate::session_pool) routing: RoutingReport,
    pub(in crate::session_pool) modules: crate::modules::ProjectModules,
    pub(in crate::session_pool) commands: Arc<runtime::commands::CommandRegistry>,
    /// Subscriptions to the settings files this binding was merged from.
    /// Held, not read: dropping them is how a binding that gets invalidated
    /// releases its share of the watch (`WatchHub` refcounts, so the tiers
    /// several projects have in common are watched once).
    pub(super) _watch: Vec<crate::watch_hub::WatchToken>,
    /// The settings files, skill and agent-type directories of this
    /// binding's tiers that did not exist when it was built, so are not
    /// watched — see [`ProjectBinding::appeared`].
    pub(super) unwatched_settings: Vec<(crate::watch_hub::Tier, PathBuf)>,
    /// Every session built on this binding keeps a copy, and rebuilds before
    /// its next turn once the cache no longer considers it current.
    pub(in crate::session_pool) stamp: BindingStamp,
}

impl ProjectBinding {
    pub(in crate::session_pool) fn build(
        stamp: BindingStamp,
        settings: Arc<LoadedSettings>,
        plugins: &crate::plugins::Contributions,
        watch: Vec<crate::watch_hub::WatchToken>,
        client: &Arc<dyn AnthropicClient>,
        last_good_router: Option<Arc<model::routing::ProviderSet>>,
        credentials: &Arc<dyn base::interface::credentials::CredentialSource>,
    ) -> Self {
        let (task_router, routing) =
            resolve_router(&settings, last_good_router, client, credentials);
        let modules = crate::modules::ProjectModules::build(&settings, plugins);
        let commands = modules.commands();
        Self {
            settings,
            task_router,
            routing,
            modules,
            commands,
            _watch: watch,
            unwatched_settings: Vec::new(),
            stamp,
        }
    }

    /// The tier of a settings file, skill or agent-type directory that has
    /// been created since this binding was built. Nothing watched it — a
    /// missing path is not subscribed, see `BindingCache::watch_tiers_of` — so
    /// this is the only way its first contents (a rule `permit_always` wrote
    /// to a new `settings.local.json`, a project's first skill or agent type)
    /// reach the sessions without a `config.reload`.
    pub(in crate::session_pool) fn appeared(&self) -> Option<&crate::watch_hub::Tier> {
        self.unwatched_settings
            .iter()
            .find(|(_, path)| path.exists())
            .map(|(tier, _)| tier)
    }

    /// Have every call of a session on this pair answered by `model`, under
    /// the built-in provider's id.
    #[cfg(test)]
    pub(in crate::session_pool) fn answer_with(
        &mut self,
        model: Arc<dyn base::interface::model::Model>,
    ) {
        self.task_router = Arc::new(model::routing::ProviderSet::single(
            model::routing::RoutedProvider::new(
                base::provider::ProviderConfig::BUILTIN_ID,
                base::provider::ResolvedTiers::all(&self.settings.main_model),
                model,
            ),
            self.settings.values.model.tasks.clone(),
        ));
    }

    /// The provider a session's calls go to first: the one it names, else
    /// the first of `model.order`.
    pub(in crate::session_pool) fn routed_provider(&self, named: Option<&str>) -> String {
        match named {
            Some(named) => named.to_string(),
            None => self.task_router.head().id().to_string(),
        }
    }

    /// Whether a session on this pair may put `provider` first and ask for
    /// `model`: the provider must be defined, and the model a tier or one the
    /// session's first provider takes — an empty `models` takes any.
    pub(in crate::session_pool) fn check_model_choice(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), String> {
        let router = self
            .task_router
            .session(provider)
            .map_err(|e| format!("{e} for this session's project"))?;
        let head = &router.head().config;
        match model {
            Some(model)
                if base::provider::Tier::from_name(model).is_none()
                    && !head.accepts_model(model) =>
            {
                Err(format!(
                    "model `{model}` is not one of provider `{}`'s models",
                    head.id
                ))
            }
            _ => Ok(()),
        }
    }

    pub(in crate::session_pool) fn reload_skills(&self) {
        self.modules.reload_skills(&self.settings);
    }

    /// Re-merge what the plugin set now contributes, in place: every session
    /// on the pair shares these catalogs, and `commands` is a view onto the
    /// skill one.
    pub(in crate::session_pool) fn refresh_plugins(&self, plugins: &crate::plugins::Contributions) {
        self.modules.refresh_plugins(&self.settings, plugins);
    }

    pub(in crate::session_pool) fn refresh_agent_types(
        &self,
        plugin_agent_types: &[base::agent_type::AgentTypeDefinition],
    ) {
        self.modules
            .refresh_agent_types(&self.settings, plugin_agent_types);
    }
}

/// How re-resolving a pair's routing went, for `config.setProvider` and
/// `config.reload`: `error` when the settings do not route — a tier outside a
/// provider's models, an order naming a provider nobody defines — and
/// `router_error` when they do but a client could not be built from them.
#[derive(Debug, Default, Clone)]
pub(in crate::session_pool) struct RoutingReport {
    pub(in crate::session_pool) warnings: Vec<String>,
    pub(in crate::session_pool) error: Option<String>,
    pub(in crate::session_pool) router_rebuilt: bool,
    pub(in crate::session_pool) router_error: Option<String>,
    /// Set when the settings define providers of their own that do not
    /// route, with nothing routed before to keep: no session is built on
    /// the pair, rather than one whose calls go to the built-in provider —
    /// somewhere the settings never said to send them.
    pub(in crate::session_pool) refusal: Option<String>,
}

/// What a pair that refuses sessions is routed to, should anything reach
/// its router anyway: every call fails with why.
struct Unrouted(String);

#[async_trait::async_trait]
impl base::interface::model::Model for Unrouted {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }

    async fn stream(
        &self,
        _: Vec<base::prompt::PromptBlock>,
        _: Vec<base::interface::model::ToolDef>,
        _: Vec<base::interface::model::ModelMessage>,
        _: base::interface::model::StreamParams,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        Err(base::interface::model::ModelError::invalid_request(
            format!("the providers do not route: {}", self.0),
        ))
    }
}

/// Build the router a pair's settings describe.
///
/// `last_good` is what this pair was already being served by. Settings that
/// no longer route, or that route but cannot produce a live client, keep it
/// rather than tearing working routing down over a bad edit — the failure is
/// reported, not acted on. With nothing to keep, a pair whose settings define
/// providers refuses sessions ([`RoutingReport::refusal`]); one that defines
/// none is served by the provider the engine supplies.
pub(in crate::session_pool) fn resolve_router(
    loaded: &LoadedSettings,
    last_good: Option<Arc<model::routing::ProviderSet>>,
    client: &Arc<dyn AnthropicClient>,
    credentials: &Arc<dyn base::interface::credentials::CredentialSource>,
) -> (Arc<model::routing::ProviderSet>, RoutingReport) {
    let mut report = RoutingReport::default();
    if let Err(e) = loaded.values.routing() {
        report.error = Some(e);
    } else {
        let quota_dir = loaded.paths.global_data_dir.join(base::paths::entry::QUOTA);
        match crate::model_router::build_task_router(
            &loaded.values,
            client,
            credentials,
            &loaded.paths.layout,
            quota_dir,
        ) {
            Ok(router) => {
                report.router_rebuilt = true;
                return (Arc::new(router), report);
            }
            Err(e) => report.router_error = Some(e),
        }
    }
    let problem = report.error.as_deref().or(report.router_error.as_deref());
    if let Some(router) = last_good {
        warn!(error = ?problem, "the providers do not route; keeping the previous router");
        return (router, report);
    }
    if let Some(defined) = loaded.values.providers.iter().find(|p| !p.builtin) {
        let problem = problem.unwrap_or_default().to_string();
        warn!(error = %problem, "the providers do not route; refusing sessions on this pair");
        let router = model::routing::ProviderSet::single(
            model::routing::RoutedProvider::new(
                &defined.id,
                base::provider::ResolvedTiers::all(&loaded.default_model),
                Arc::new(Unrouted(problem.clone())),
            ),
            loaded.values.model.tasks.clone(),
        );
        report.refusal = Some(format!("the providers do not route: {problem}"));
        return (Arc::new(router), report);
    }
    warn!(error = ?problem, "the providers do not route; serving the built-in provider");
    let builtin = base::provider::ProviderConfig::builtin(&loaded.default_model);
    let router = model::routing::ProviderSet::single(
        model::routing::RoutedProvider {
            tiers: base::provider::ResolvedTiers::all(&loaded.default_model),
            config: builtin,
            client: Arc::new(model::adapter::AnthropicModel::new(client.clone())),
        },
        loaded.values.model.tasks.clone(),
    );
    (Arc::new(router), report)
}
