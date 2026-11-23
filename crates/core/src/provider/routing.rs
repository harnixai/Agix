//! Routing model calls over the providers in `model.order`.
//!
//! A task asks for a tier (or the session for a model id); every call starts
//! at the head of the order and goes to the first provider that can take it:
//! one with quota left, not cooling down after a connection failure, that
//! accepts the request without an auth or connection failure once its own
//! retries are spent. A request the provider refuses as such ends the call
//! there — another provider would refuse it too. The same call to a later
//! provider asks for that provider's model of the same tier.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use super::{ApiType, ProviderConfig, QuotaLedger, ResolvedTiers, TaskTiers, Tier};
use crate::interface::model::{
    Model, ModelError, ModelErrorKind, ModelEvent, ModelMessage, ModelStream, StreamParams,
    ToolDef, Usage, UsageTap,
};
use crate::prompt::PromptBlock;

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

/// A call that went to a different provider than the call before it.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ProviderSwitch {
    pub from: String,
    pub to: String,
    /// Why each provider before `to` did not take the call; when none was
    /// passed over, that `to` can take calls again.
    pub reason: String,
    /// The kind of call: `main`, `subagent`, `compact`, ….
    pub task: String,
}

/// Told about every [`ProviderSwitch`] of a session's calls.
pub trait RouteObserver: Send + Sync {
    fn switched(&self, switch: &ProviderSwitch);
}

#[derive(Default)]
struct Health {
    cooling: Mutex<HashMap<String, Instant>>,
}

impl Health {
    fn is_cooling(&self, id: &str) -> bool {
        let cooling = self.cooling.lock().unwrap_or_else(|e| e.into_inner());
        cooling.get(id).is_some_and(|until| Instant::now() < *until)
    }

    fn cool(&self, id: &str) {
        let mut cooling = self.cooling.lock().unwrap_or_else(|e| e.into_inner());
        cooling.insert(id.to_string(), Instant::now() + COOLDOWN);
    }

    fn recovered(&self, id: &str) {
        let mut cooling = self.cooling.lock().unwrap_or_else(|e| e.into_inner());
        cooling.remove(id);
    }
}

#[derive(Default)]
struct SessionRoute {
    last: Mutex<Option<String>>,
    observer: OnceLock<Arc<dyn RouteObserver>>,
}

impl SessionRoute {
    fn served(&self, to: &str, task: &str, reason: impl FnOnce() -> String) {
        let from = {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            match last.replace(to.to_string()) {
                Some(from) if from != to => from,
                _ => return,
            }
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

/// The providers a pair's settings define, the order calls go through them
/// in, and the tier each task asks for. Cheap to clone; clones share the
/// providers' health. One per session ([`Self::for_session`]) also keeps
/// which provider the session's last call went to.
#[derive(Clone)]
pub struct TaskRouter {
    providers: Arc<Vec<Arc<RoutedProvider>>>,
    order: Arc<Vec<String>>,
    tasks: TaskTiers,
    health: Arc<Health>,
    quota: Option<Arc<QuotaKeeper>>,
    session: Arc<SessionRoute>,
}

impl TaskRouter {
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
            session: Arc::default(),
        })
    }

    /// A router over one provider.
    pub fn single(provider: RoutedProvider, tasks: TaskTiers) -> Self {
        Self {
            order: Arc::new(vec![provider.config.id.clone()]),
            providers: Arc::new(vec![Arc::new(provider)]),
            tasks,
            health: Arc::default(),
            quota: None,
            session: Arc::default(),
        }
    }

    pub fn with_quota(mut self, ledger: Arc<dyn QuotaLedger>) -> Self {
        self.quota = Some(Arc::new(QuotaKeeper {
            ledger,
            unwritten: Arc::default(),
        }));
        self
    }

    /// This router for one session: `preferred` first in its order when
    /// given, and its own record of where its calls went.
    pub fn for_session(&self, preferred: Option<&str>) -> Result<Self, String> {
        let mut order = (*self.order).clone();
        if let Some(id) = preferred {
            if self.find(id).is_none() {
                return Err(format!("provider `{id}` is not configured"));
            }
            order.retain(|o| o != id);
            order.insert(0, id.to_string());
        }
        let session = SessionRoute {
            last: Mutex::new(order.first().cloned()),
            observer: OnceLock::new(),
        };
        Ok(Self {
            order: Arc::new(order),
            session: Arc::new(session),
            ..self.clone()
        })
    }

    /// Report this session's provider switches to `observer`. The first
    /// observer stays.
    pub fn observe(&self, observer: Arc<dyn RouteObserver>) {
        let _ = self.session.observer.set(observer);
    }

    pub fn order(&self) -> &[String] {
        &self.order
    }

    pub fn tasks(&self) -> &TaskTiers {
        &self.tasks
    }

    fn find(&self, id: &str) -> Option<&Arc<RoutedProvider>> {
        self.providers.iter().find(|p| p.config.id == id)
    }

    fn ordered(&self) -> impl Iterator<Item = &Arc<RoutedProvider>> {
        self.order.iter().filter_map(|id| self.find(id))
    }

    /// The provider calls go to when every one can take them.
    pub fn head(&self) -> &RoutedProvider {
        self.ordered().next().unwrap_or(&self.providers[0])
    }

    /// The client of provider `id`, when it is defined.
    pub fn provider(&self, id: &str) -> Option<Arc<dyn Model>> {
        self.find(id).map(|p| p.client.clone())
    }

    pub fn provider_config(&self, id: &str) -> Option<&ProviderConfig> {
        self.find(id).map(|p| &p.config)
    }

    /// Whether some provider in the order would take `model`.
    pub fn accepts(&self, model: &str) -> bool {
        Tier::from_name(model).is_some() || self.ordered().any(|p| p.config.accepts_model(model))
    }

    /// The model `name` stands for at the head: its model of that tier when
    /// `name` is a tier, else `name`.
    pub fn resolve_model(&self, name: &str) -> String {
        match Tier::from_name(name) {
            Some(tier) => self.head().tiers.get(tier).to_string(),
            None => name.to_string(),
        }
    }

    /// The model `task` goes to at the head; `None` for a task that uses no
    /// model.
    pub fn model_name_for(&self, task: &str) -> Option<String> {
        let tier = self.tasks.get(task)?;
        Some(self.head().tiers.get(tier).to_string())
    }

    /// What `models` says about `model` at the first provider in the order
    /// that lists it.
    pub fn model_spec(&self, model: &str) -> Option<&super::ModelSpec> {
        self.ordered().find_map(|p| p.config.model_spec(model))
    }

    /// The model `task`'s calls go through. A task with no tier of its own
    /// asks for `strong`.
    pub fn model_for(&self, task: &str) -> Arc<dyn Model> {
        Arc::new(RoutedModel {
            router: self.clone(),
            task: task.to_string(),
            tier: self.tasks.get(task).unwrap_or(Tier::Strong),
            served: Mutex::new(None),
        })
    }

    /// A router with every provider's client replaced by `wrap(id, client)`,
    /// sharing this one's health and session.
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

    /// The first provider in the order that serves `model`: lists it, or
    /// names it as a tier; else the first that accepts any model; else the
    /// head.
    fn home_of(&self, model: &str) -> &RoutedProvider {
        self.ordered()
            .find(|p| p.config.lists_model(model) || p.tiers.contains(model))
            .or_else(|| self.ordered().find(|p| p.config.models.is_empty()))
            .map(Arc::as_ref)
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

    fn quota_left(&self, provider: &RoutedProvider) -> Result<(), String> {
        match &self.quota {
            Some(ledger) if !provider.config.quota.is_unlimited() => {
                match ledger
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

struct RoutedModel {
    router: TaskRouter,
    task: String,
    tier: Tier,
    /// The provider the last call went to. Reasoning is stamped with it, and
    /// reasoning stamped with another is kept from each provider, by the
    /// router rather than by each client: a host's own client may do
    /// neither.
    served: Mutex<Option<String>>,
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
        self.router.head().client.api_type()
    }

    fn provenance(&self) -> Option<String> {
        let served = self.served.lock().unwrap_or_else(|e| e.into_inner());
        Some(
            served
                .clone()
                .unwrap_or_else(|| self.router.head().config.id.clone()),
        )
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let router = &self.router;
        let alone = router.order.len() < 2;
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
                    router.health.recovered(&id);
                    *self.served.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
                    router
                        .session
                        .served(&id, &self.task, || switch_reason(&passed_over, &id));
                    return Ok(self.counting(stream, &provider, tap));
                }
                Err(error) if !takes_provider_out(error.kind) => return Err(error),
                Err(error) => {
                    if !alone && is_connection_failure(error.kind) {
                        router.health.cool(&id);
                    }
                    tracing::warn!(provider = %id, task = %self.task, %error, "provider could not take the call");
                    passed_over.push((id, crate::text::scrub_url(&error.to_string())));
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
        let router = &self.router;
        let mut passed_over = Vec::new();
        let mut ready = Vec::new();
        let mut cooling = Vec::new();
        for provider in router.ordered() {
            if let Err(reason) = router.quota_left(provider) {
                passed_over.push((provider.config.id.clone(), reason));
            } else if !alone && router.health.is_cooling(provider.id()) {
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
        params.model = self.router.model_at(provider, &params.model, self.tier);
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

    fn counting(
        &self,
        stream: ModelStream,
        provider: &RoutedProvider,
        tap: UsageTap,
    ) -> ModelStream {
        let quota = self
            .router
            .quota
            .clone()
            .filter(|_| !provider.config.quota.is_unlimited())
            .map(|keeper| (keeper, provider.config.id.clone()));
        Counting::wrap(stream, tap, quota)
    }
}

/// A model nothing routes, counted as a routed call is: what its calls spend
/// is charged where their tap says. Without it, a session built on a bare
/// client would count nothing.
pub struct Metered(pub Arc<dyn Model>);

#[async_trait]
impl Model for Metered {
    fn api_type(&self) -> ApiType {
        self.0.api_type()
    }

    fn provenance(&self) -> Option<String> {
        self.0.provenance()
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        mut params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let tap = params.usage_tap.for_attempt();
        params.usage_tap = tap.clone();
        let stream = self
            .0
            .stream(prompt_blocks, tools, messages, params, cancel)
            .await?;
        Ok(Counting::wrap(stream, tap, None))
    }
}

/// A call's stream that counts its usage once — against its provider's
/// quota and into the node its tap is charged to — at
/// [`ModelEvent::EndTurn`], or, for a call that broke off, failed or was
/// dropped before it, when it goes, with what the client reported so far.
struct Counting {
    inner: ModelStream,
    quota: Option<(Arc<QuotaKeeper>, String)>,
    tap: UsageTap,
    counted: bool,
}

impl Counting {
    fn wrap(
        inner: ModelStream,
        tap: UsageTap,
        quota: Option<(Arc<QuotaKeeper>, String)>,
    ) -> ModelStream {
        if quota.is_none() && tap.charge().is_none() {
            return inner;
        }
        Box::new(Self {
            inner,
            quota,
            tap,
            counted: false,
        })
    }

    fn count_once(&mut self, usage: &Usage) {
        if std::mem::replace(&mut self.counted, true) {
            return;
        }
        if let Some((keeper, provider)) = &self.quota {
            let spent = crate::interface::budget_policy::Spend::from(*usage);
            keeper.record(provider, spent.counted_tokens());
        }
        if let Some(node) = self.tap.charge() {
            node.add(*usage);
        }
    }
}

impl futures::Stream for Counting {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let polled = self.inner.poll_next_unpin(cx);
        if let std::task::Poll::Ready(Some(Ok(ModelEvent::EndTurn { usage, .. }))) = &polled {
            let usage = *usage;
            self.count_once(&usage);
        }
        polled
    }
}

impl Drop for Counting {
    fn drop(&mut self) {
        if let Some(usage) = self.tap.reported() {
            self.count_once(&usage);
        }
    }
}

/// How many times a write of usage is tried before it is given up on. Each
/// try already waits for the ledger's lock.
const QUOTA_WRITE_ATTEMPTS: u32 = 3;

/// A router's quota ledger, written off the thread that polls the call: a
/// write can wait seconds on another instance's lock. Usage recorded but not
/// yet written counts in [`Self::status`] meanwhile, so the next call sees it.
struct QuotaKeeper {
    ledger: Arc<dyn QuotaLedger>,
    unwritten: Arc<Mutex<HashMap<String, u64>>>,
}

impl QuotaKeeper {
    fn status(&self, provider: &str, quota: &super::Quota) -> super::QuotaStatus {
        let mut status = self.ledger.status(provider, quota);
        let unwritten = lock(&self.unwritten).get(provider).copied().unwrap_or(0);
        for period in [&mut status.day, &mut status.week, &mut status.month] {
            period.used = period.used.saturating_add(unwritten);
        }
        status
    }

    fn record(&self, provider: &str, tokens: u64) {
        if tokens == 0 {
            return;
        }
        *lock(&self.unwritten)
            .entry(provider.to_string())
            .or_default() += tokens;
        let (ledger, unwritten, provider) = (
            self.ledger.clone(),
            self.unwritten.clone(),
            provider.to_string(),
        );
        let write = move || {
            let mut failed = None;
            for _ in 0..QUOTA_WRITE_ATTEMPTS {
                match ledger.record(&provider, tokens) {
                    Ok(()) => {
                        failed = None;
                        break;
                    }
                    Err(error) => failed = Some(error),
                }
            }
            let mut unwritten = lock(&unwritten);
            if let Some(left) = unwritten.get_mut(&provider) {
                *left = left.saturating_sub(tokens);
                if *left == 0 {
                    unwritten.remove(&provider);
                }
            }
            if let Some(error) = failed {
                tracing::warn!(provider, tokens, %error, "quota usage not recorded");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(write)),
            Err(_) => drop(std::thread::spawn(write)),
        }
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
