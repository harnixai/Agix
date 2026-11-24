//! Routing model calls over the providers in `model.order`.
//!
//! A task asks for a tier (or the session for a model id); every call starts
//! at the head of the order and goes to the first provider that can take it:
//! one with quota left, not cooling down after a connection failure, that
//! accepts the request without an auth or connection failure once its own
//! retries are spent. A request the provider refuses as such ends the call
//! there — another provider would refuse it too. The same call to a later
//! provider asks for that provider's model of the same tier.
//!
//! A [`ProviderSet`] is what a pair's settings define, shared by every
//! session on the pair along with its providers' health and quota; a
//! [`SessionRouter`] is one session's view of it — its own order and its
//! record of where its calls went — and the
//! [`TaskRouting`](base::provider::TaskRouting) runtime routes through. Each
//! call is metered here, once, and names the provider that served it in its
//! own stream.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base::interface::model::{
    Model, ModelError, ModelErrorKind, ModelEvent, ModelMessage, ModelStream, OnSpent,
    StreamParams, ToolDef, UsageTap,
};
use base::prompt::PromptBlock;
use base::provider::{
    ApiType, Destination, ProviderConfig, ProviderSwitch, QuotaLedger, ResolvedTiers,
    RouteObserver, TaskRouting, TaskTiers, Tier,
};
use tokio_util::sync::CancellationToken;

mod quota;

use quota::QuotaKeeper;

/// How long a provider that failed to connect is passed over while another
/// can take the call.
const COOLDOWN: Duration = Duration::from_secs(60);

/// A provider's configuration with its client built.
pub struct RoutedProvider {
    pub config: ProviderConfig,
    pub tiers: ResolvedTiers,
    pub client: Arc<dyn Model>,
}

impl RoutedProvider {
    /// A provider with nothing configured beyond its id and tiers.
    pub fn new(id: &str, tiers: ResolvedTiers, client: Arc<dyn Model>) -> Self {
        Self {
            config: ProviderConfig {
                id: id.to_string(),
                ..Default::default()
            },
            tiers,
            client,
        }
    }

    pub fn id(&self) -> &str {
        &self.config.id
    }
}

#[derive(Default)]
struct Health {
    cooling: Mutex<HashMap<String, Instant>>,
}

impl Health {
    fn is_cooling(&self, id: &str) -> bool {
        lock(&self.cooling)
            .get(id)
            .is_some_and(|until| Instant::now() < *until)
    }

    fn cool(&self, id: &str) {
        lock(&self.cooling).insert(id.to_string(), Instant::now() + COOLDOWN);
    }

    fn recovered(&self, id: &str) {
        lock(&self.cooling).remove(id);
    }
}

/// The providers a pair's settings define, the order calls go through them
/// in, and the tier each task asks for. Cheap to clone; clones share the
/// providers' health and quota.
#[derive(Clone)]
pub struct ProviderSet {
    providers: Arc<Vec<Arc<RoutedProvider>>>,
    order: Arc<Vec<String>>,
    tasks: TaskTiers,
    health: Arc<Health>,
    quota: Option<Arc<QuotaKeeper>>,
}

impl ProviderSet {
    /// `order` names providers among `providers`; empty is the first of them
    /// alone.
    pub fn new(
        providers: Vec<RoutedProvider>,
        order: Vec<String>,
        tasks: TaskTiers,
    ) -> Result<Self, String> {
        let Some(first) = providers.first() else {
            return Err("no provider to route to".to_string());
        };
        let mut ordered: Vec<String> = Vec::new();
        if order.is_empty() {
            ordered.push(first.config.id.clone());
        }
        for id in order {
            if !providers.iter().any(|p| p.config.id == id) {
                return Err(format!(
                    "`model.order` names provider `{id}`, which is not defined"
                ));
            }
            if !ordered.contains(&id) {
                ordered.push(id);
            }
        }
        Ok(Self {
            providers: Arc::new(providers.into_iter().map(Arc::new).collect()),
            order: Arc::new(ordered),
            tasks,
            health: Arc::default(),
            quota: None,
        })
    }

    /// A set of one provider.
    pub fn single(provider: RoutedProvider, tasks: TaskTiers) -> Self {
        Self {
            order: Arc::new(vec![provider.config.id.clone()]),
            providers: Arc::new(vec![Arc::new(provider)]),
            tasks,
            health: Arc::default(),
            quota: None,
        }
    }

    pub fn with_quota(mut self, ledger: Arc<dyn QuotaLedger>) -> Self {
        self.quota = Some(Arc::new(QuotaKeeper::new(ledger)));
        self
    }

    /// One session's router over these providers: `preferred` first in its
    /// order when given, and its own record of where its calls went.
    pub fn session(&self, preferred: Option<&str>) -> Result<SessionRouter, String> {
        let mut order = (*self.order).clone();
        if let Some(id) = preferred {
            if self.find(id).is_none() {
                return Err(format!("provider `{id}` is not configured"));
            }
            order.retain(|o| o != id);
            order.insert(0, id.to_string());
        }
        Ok(SessionRouter(Arc::new(Route {
            last: Mutex::new(order.first().cloned()),
            order,
            set: self.clone(),
            observer: OnceLock::new(),
        })))
    }

    pub fn order(&self) -> &[String] {
        &self.order
    }

    /// The provider calls go to when every one can take them.
    pub fn head(&self) -> &RoutedProvider {
        head(self, &self.order)
    }

    /// The client of provider `id`, when it is defined.
    pub fn provider(&self, id: &str) -> Option<Arc<dyn Model>> {
        self.find(id).map(|p| p.client.clone())
    }

    /// These providers with every client replaced by `wrap(id, client)`,
    /// sharing their health and quota.
    pub fn map_models(&self, wrap: impl Fn(&str, Arc<dyn Model>) -> Arc<dyn Model>) -> Self {
        let providers = self
            .providers
            .iter()
            .map(|p| {
                Arc::new(RoutedProvider {
                    config: p.config.clone(),
                    tiers: p.tiers.clone(),
                    client: wrap(&p.config.id, p.client.clone()),
                })
            })
            .collect();
        Self {
            providers: Arc::new(providers),
            ..self.clone()
        }
    }

    fn find(&self, id: &str) -> Option<&Arc<RoutedProvider>> {
        self.providers.iter().find(|p| p.config.id == id)
    }

    fn quota_left(&self, provider: &RoutedProvider) -> Result<(), String> {
        match &self.quota {
            Some(keeper) if !provider.config.quota.is_unlimited() => {
                match keeper
                    .status(provider.id(), &provider.config.quota)
                    .exhausted()
                {
                    Some(reason) => Err(reason),
                    None => Ok(()),
                }
            }
            _ => Ok(()),
        }
    }

    /// What a call `provider` served spent, recorded against its quota.
    fn quota_meter(&self, provider: &RoutedProvider) -> Option<OnSpent> {
        let keeper = self
            .quota
            .clone()
            .filter(|_| !provider.config.quota.is_unlimited())?;
        let id = provider.config.id.clone();
        Some(Box::new(move |usage| {
            let spent = base::interface::budget_policy::Spend::from(usage);
            keeper.record(&id, spent.counted_tokens());
        }))
    }
}

fn head<'a>(set: &'a ProviderSet, order: &[String]) -> &'a RoutedProvider {
    order
        .iter()
        .find_map(|id| set.find(id))
        .unwrap_or(&set.providers[0])
}

/// One session's routing over a [`ProviderSet`]. Cheap to clone; clones are
/// the same session.
#[derive(Clone)]
pub struct SessionRouter(Arc<Route>);

struct Route {
    set: ProviderSet,
    order: Vec<String>,
    /// Where the session's last call went, to tell a switch from the call
    /// before. Nothing about a call itself is read from it.
    last: Mutex<Option<String>>,
    observer: OnceLock<Arc<dyn RouteObserver>>,
}

impl SessionRouter {
    pub fn order(&self) -> &[String] {
        &self.0.order
    }

    /// The provider this session's calls go to when every one can take them.
    pub fn head(&self) -> &RoutedProvider {
        self.0.head()
    }
}

impl TaskRouting for SessionRouter {
    fn model_for(&self, task: &str) -> Arc<dyn Model> {
        Arc::new(RoutedModel {
            route: self.0.clone(),
            task: task.to_string(),
            tier: self.0.set.tasks.get(task).unwrap_or(Tier::Strong),
        })
    }

    fn model_name_for(&self, task: &str) -> Option<String> {
        let tier = self.0.set.tasks.get(task)?;
        Some(self.head().tiers.get(tier).to_string())
    }

    fn resolve_model(&self, name: &str) -> String {
        match Tier::from_name(name) {
            Some(tier) => self.head().tiers.get(tier).to_string(),
            None => name.to_string(),
        }
    }

    fn observe(&self, observer: Arc<dyn RouteObserver>) {
        let _ = self.0.observer.set(observer);
    }

    fn starting_at(&self, provider: &str) -> Result<Arc<dyn TaskRouting>, String> {
        Ok(Arc::new(self.0.set.session(Some(provider))?))
    }

    fn home_of(&self, model: &str) -> Option<String> {
        self.0.definite_home(model).map(|p| p.id().to_string())
    }

    fn destination(&self, task: &str, model: &str) -> Destination {
        let head = self.head();
        let tier = self.0.set.tasks.get(task).unwrap_or(Tier::Strong);
        Destination {
            provider: head.id().to_string(),
            model: self.0.model_at(head, model, tier),
        }
    }
}

impl Route {
    fn head(&self) -> &RoutedProvider {
        head(&self.set, &self.order)
    }

    fn ordered(&self) -> impl Iterator<Item = &Arc<RoutedProvider>> {
        self.order.iter().filter_map(|id| self.set.find(id))
    }

    fn definite_home(&self, model: &str) -> Option<&RoutedProvider> {
        self.ordered()
            .find(|p| p.config.lists_model(model) || p.tiers.contains(model))
            .map(Arc::as_ref)
    }

    /// The first provider in the order that serves `model`: its definite
    /// home; else the first that accepts any model; else the head.
    fn home_of(&self, model: &str) -> &RoutedProvider {
        self.definite_home(model)
            .or_else(|| {
                self.ordered()
                    .find(|p| p.config.models.is_empty())
                    .map(Arc::as_ref)
            })
            .unwrap_or_else(|| self.head())
    }

    /// The model a call asking for `requested` is sent to `provider` with.
    fn model_at(&self, provider: &RoutedProvider, requested: &str, task_tier: Tier) -> String {
        if requested.is_empty() {
            return provider.tiers.get(task_tier).to_string();
        }
        if let Some(tier) = Tier::from_name(requested) {
            return provider.tiers.get(tier).to_string();
        }
        let home = self.home_of(requested);
        if std::ptr::eq(home, provider) {
            return requested.to_string();
        }
        let tier = home
            .tiers
            .tier_of(requested, task_tier)
            .unwrap_or(task_tier);
        provider.tiers.get(tier).to_string()
    }

    fn served(&self, to: &str, task: &str, reason: impl FnOnce() -> String) {
        let from = match lock(&self.last).replace(to.to_string()) {
            Some(from) if from != to => from,
            _ => return,
        };
        let switch = ProviderSwitch {
            from,
            to: to.to_string(),
            reason: reason(),
            task: task.to_string(),
        };
        tracing::info!(from = %switch.from, to = %switch.to, task, reason = %switch.reason, "model calls moved to another provider");
        if let Some(observer) = self.observer.get() {
            observer.switched(&switch);
        }
    }
}

/// Whether a provider that failed a call this way is no use for it, so the
/// next one should be asked.
fn takes_provider_out(kind: ModelErrorKind) -> bool {
    use ModelErrorKind as K;
    matches!(
        kind,
        K::RateLimited
            | K::Overloaded
            | K::Unavailable
            | K::Transport
            | K::Timeout
            | K::ServerError
            | K::Auth
            | K::NoCredentials
            | K::Billing
            | K::Denied
    )
}

/// Whether the failure is the connection's, which waiting may cure.
fn is_connection_failure(kind: ModelErrorKind) -> bool {
    use ModelErrorKind as K;
    matches!(
        kind,
        K::RateLimited
            | K::Overloaded
            | K::Unavailable
            | K::Transport
            | K::Timeout
            | K::ServerError
    )
}

/// One task's calls through a session's route. Reasoning is kept from each
/// provider when another produced it, and each call names its provider in
/// its stream, by the router rather than by each client: a host's own client
/// may do neither.
struct RoutedModel {
    route: Arc<Route>,
    task: String,
    tier: Tier,
}

#[derive(Clone)]
struct Request {
    prompt_blocks: Vec<PromptBlock>,
    tools: Vec<ToolDef>,
    messages: Vec<ModelMessage>,
}

#[async_trait]
impl Model for RoutedModel {
    fn api_type(&self) -> ApiType {
        self.route.head().client.api_type()
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let route = &self.route;
        let health = &route.set.health;
        let alone = route.order.len() < 2;
        let (candidates, mut passed_over) = self.candidates(alone);
        let mut request = Some(Request {
            prompt_blocks,
            tools,
            messages,
        });
        let mut last_error = None;
        let attempts = candidates.len();
        for (index, provider) in candidates.into_iter().enumerate() {
            let Some(whole) = request.take() else { break };
            let this = match index + 1 < attempts {
                true => {
                    let copy = whole.clone();
                    request = Some(whole);
                    copy
                }
                false => whole,
            };
            let id = provider.config.id.clone();
            let tap = params.usage_tap.for_attempt();
            match self.send(&provider, this, &params, &tap, &cancel).await {
                Ok(stream) => {
                    health.recovered(&id);
                    route.served(&id, &self.task, || switch_reason(&passed_over, &id));
                    let named = futures::stream::iter([Ok(ModelEvent::Served { provider: id })]);
                    let stream: ModelStream = Box::new(futures::StreamExt::chain(named, stream));
                    return Ok(tap.metered(stream, route.set.quota_meter(&provider)));
                }
                Err(error) if !takes_provider_out(error.kind) => return Err(error),
                Err(error) => {
                    if !alone && is_connection_failure(error.kind) {
                        health.cool(&id);
                    }
                    tracing::warn!(provider = %id, task = %self.task, %error, "provider could not take the call");
                    passed_over.push((id, base::text::scrub_url(&error.to_string())));
                    last_error = Some(error);
                }
            }
        }
        match (alone, last_error) {
            (true, Some(error)) => Err(error),
            _ => Err(ModelError::new(
                ModelErrorKind::ProvidersUnavailable,
                format!("no provider could take the call: {}", listed(&passed_over)),
            )),
        }
    }
}

impl RoutedModel {
    /// The providers to try, in order — those cooling down after the rest,
    /// unless there is only one — and the ones out of quota, with why.
    fn candidates(&self, alone: bool) -> (Vec<Arc<RoutedProvider>>, Vec<(String, String)>) {
        let set = &self.route.set;
        let mut passed_over = Vec::new();
        let mut ready = Vec::new();
        let mut cooling = Vec::new();
        for provider in self.route.ordered() {
            if let Err(reason) = set.quota_left(provider) {
                passed_over.push((provider.config.id.clone(), reason));
            } else if !alone && set.health.is_cooling(provider.id()) {
                cooling.push(provider.clone());
            } else {
                ready.push(provider.clone());
            }
        }
        ready.extend(cooling);
        (ready, passed_over)
    }

    /// `request` to `provider`, asking for its model of the call's tier.
    async fn send(
        &self,
        provider: &RoutedProvider,
        request: Request,
        params: &StreamParams,
        tap: &UsageTap,
        cancel: &CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let mut params = params.clone();
        params.usage_tap = tap.clone();
        params.model = self.route.model_at(provider, &params.model, self.tier);
        if let Some(cap) = provider
            .config
            .model_spec(&params.model)
            .and_then(|spec| spec.max_output)
        {
            params.max_tokens = params.max_tokens.min(cap);
        }
        let mut messages = request.messages;
        for message in &mut messages {
            message
                .content
                .retain(|block| !block.is_foreign_reasoning(Some(provider.id())));
        }
        provider
            .client
            .stream(
                request.prompt_blocks,
                request.tools,
                messages,
                params,
                cancel.clone(),
            )
            .await
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn switch_reason(passed_over: &[(String, String)], to: &str) -> String {
    match passed_over.is_empty() {
        true => format!("`{to}` can take calls again"),
        false => listed(passed_over),
    }
}

fn listed(passed_over: &[(String, String)]) -> String {
    passed_over
        .iter()
        .map(|(id, reason)| format!("`{id}`: {reason}"))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests;
