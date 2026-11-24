use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base::interface::model::{
    Model, ModelError, ModelErrorKind, ModelEvent, ModelMessage, ModelStream, StreamParams,
    ToolDef, Usage,
};
use base::prompt::PromptBlock;
use base::provider::{PeriodUsage, Quota, QuotaStatus, Tiers};
use tokio_util::sync::CancellationToken;

use super::*;

fn provider(id: &str, models: &[&str], tiers: [Option<&str>; 3]) -> ProviderConfig {
    ProviderConfig {
        id: id.into(),
        models: models.iter().map(|m| (*m).into()).collect(),
        tiers: Tiers {
            strong: tiers[0].map(str::to_string),
            balanced: tiers[1].map(str::to_string),
            fast: tiers[2].map(str::to_string),
        },
        ..Default::default()
    }
}

/// Usage in memory, counted as the file-backed ledger counts it.
#[derive(Default)]
struct Ledger(Mutex<std::collections::HashMap<String, u64>>);

impl QuotaLedger for Ledger {
    fn status(&self, provider: &str, quota: &Quota) -> QuotaStatus {
        let used = self.0.lock().unwrap().get(provider).copied().unwrap_or(0);
        let usage = |limit| PeriodUsage {
            start: "2026-11-19".into(),
            used,
            limit,
        };
        QuotaStatus {
            day: usage(quota.day),
            week: usage(quota.week),
            month: usage(quota.month),
            error: None,
        }
    }

    fn record(&self, provider: &str, tokens: u64) -> Result<(), String> {
        *self.0.lock().unwrap().entry(provider.into()).or_default() += tokens;
        Ok(())
    }
}

impl Ledger {
    fn used(&self, provider: &str) -> u64 {
        self.0.lock().unwrap().get(provider).copied().unwrap_or(0)
    }

    /// Wait until what `provider` used reaches `expected`, as the ledger is
    /// written off the call's thread.
    async fn settles_at(&self, provider: &str, expected: u64) {
        let reached = async {
            while self.used(provider) < expected {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), reached)
            .await
            .expect("the usage is recorded");
        assert_eq!(self.used(provider), expected);
    }
}

#[derive(Clone)]
enum Answer {
    Ok,
    Fail(ModelErrorKind),
    FailWith(ModelErrorKind, &'static str),
    /// Reports what the call's opening used, then the stream breaks off.
    BreaksOff,
    /// Reports what the call's opening used, sends a little, then nothing.
    Stalls,
    /// Fails a call asking for this model, answers the rest.
    FailOn(&'static str, ModelErrorKind),
}

/// What a provider reports when the call opens: the input side of
/// [`Scripted::usage`].
fn opening(usage: Usage) -> Usage {
    Usage {
        output_tokens: 0,
        ..usage
    }
}

struct Scripted {
    id: &'static str,
    answer: Answer,
    usage: Usage,
    asked: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Model for Scripted {
    fn api_type(&self) -> ApiType {
        ApiType::Anthropic
    }

    fn provenance(&self) -> Option<String> {
        Some(self.id.to_string())
    }

    async fn stream(
        &self,
        _prompt_blocks: Vec<PromptBlock>,
        _tools: Vec<ToolDef>,
        _messages: Vec<ModelMessage>,
        params: StreamParams,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        self.asked
            .lock()
            .unwrap()
            .push(format!("{}:{}", self.id, params.model));
        let text = || {
            Ok(ModelEvent::TextDelta {
                text: "partial".into(),
            })
        };
        let answer = match &self.answer {
            Answer::FailOn(model, kind) if params.model == *model => Answer::Fail(*kind),
            Answer::FailOn(..) => Answer::Ok,
            other => other.clone(),
        };
        match &answer {
            Answer::Ok => {
                params.usage_tap.report(opening(self.usage));
                Ok(Box::new(futures::stream::iter([Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: self.usage,
                })])))
            }
            Answer::Fail(kind) => Err(ModelError::new(*kind, "scripted")),
            Answer::FailWith(kind, message) => Err(ModelError::new(*kind, *message)),
            Answer::BreaksOff => {
                params.usage_tap.report(opening(self.usage));
                Ok(Box::new(futures::stream::iter([
                    text(),
                    Err(ModelError::stream_interrupted("connection reset")),
                ])))
            }
            Answer::Stalls => {
                params.usage_tap.report(opening(self.usage));
                Ok(Box::new(futures::StreamExt::chain(
                    futures::stream::iter([text()]),
                    futures::stream::pending(),
                )))
            }
            Answer::FailOn(..) => unreachable!("resolved above"),
        }
    }
}

fn routed(
    config: ProviderConfig,
    answer: Answer,
    asked: &Arc<Mutex<Vec<String>>>,
) -> RoutedProvider {
    let id: &'static str = Box::leak(config.id.clone().into_boxed_str());
    RoutedProvider {
        tiers: config.resolved_tiers().unwrap(),
        config,
        client: Arc::new(Scripted {
            id,
            answer,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_input_tokens: 2,
                cache_read_input_tokens: 1000,
            },
            asked: asked.clone(),
        }),
    }
}

fn params(model: &str) -> StreamParams {
    StreamParams {
        model: model.into(),
        max_tokens: 100,
        thinking_mode: base::interface::model::ThinkingMode::Off,
        effort: None,
        cache_edits: Vec::new(),
        origin: None,
        input_map: None,
        provider_search: None,
        usage_tap: Default::default(),
    }
}

async fn open(model: &Arc<dyn Model>, name: &str) -> Result<ModelStream, ModelError> {
    model
        .stream(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            params(name),
            CancellationToken::new(),
        )
        .await
}

/// The provider `stream` says served it, once it is read to the end.
async fn served_by(mut stream: ModelStream) -> Result<Option<String>, ModelError> {
    use futures::StreamExt;
    let mut served = None;
    while let Some(item) = stream.next().await {
        if let ModelEvent::Served { provider, .. } = item? {
            served.get_or_insert(provider);
        }
    }
    Ok(served)
}

/// A call through `model` asking for `name`, and the provider that served it.
async fn call(model: &Arc<dyn Model>, name: &str) -> Result<Option<String>, ModelError> {
    served_by(open(model, name).await?).await
}

#[derive(Default)]
struct Switches(Mutex<Vec<ProviderSwitch>>);

impl RouteObserver for Switches {
    fn switched(&self, switch: &ProviderSwitch) {
        self.0.lock().unwrap().push(switch.clone());
    }
}

fn ds() -> ProviderConfig {
    provider(
        "ds",
        &["ds-pro", "ds-flash"],
        [Some("ds-pro"), Some("ds-flash"), None],
    )
}

fn claude() -> ProviderConfig {
    provider("claude", &[], [Some("opus"), Some("sonnet"), Some("haiku")])
}

#[tokio::test]
async fn a_failing_provider_hands_the_call_to_the_next_with_the_same_tier() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let router = ProviderSet::new(
        vec![
            routed(ds(), Answer::Fail(ModelErrorKind::ServerError), &asked),
            routed(claude(), Answer::Ok, &asked),
        ],
        vec!["ds".into(), "claude".into()],
        TaskTiers::default(),
    )
    .unwrap()
    .session(None)
    .unwrap();
    let switches = Arc::new(Switches::default());
    router.observe(switches.clone());

    let main = router.model_for("main");
    let served = call(&main, "ds-flash").await.unwrap();
    assert_eq!(*asked.lock().unwrap(), ["ds:ds-flash", "claude:sonnet"]);
    assert_eq!(served.as_deref(), Some("claude"));
    assert_eq!(
        main.provenance(),
        None,
        "a router has no provider of its own"
    );
    let seen = switches.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].from.as_str(), seen[0].to.as_str()),
        ("ds", "claude")
    );
    assert_eq!(seen[0].task, "main");
    assert!(
        seen[0].reason.contains("server_error"),
        "{}",
        seen[0].reason
    );

    asked.lock().unwrap().clear();
    call(&main, "ds-pro").await.unwrap();
    assert_eq!(
        *asked.lock().unwrap(),
        ["claude:opus"],
        "a provider that failed to connect is passed over while it cools down"
    );
    assert_eq!(
        switches.0.lock().unwrap().len(),
        1,
        "no switch: claude again"
    );
}

#[tokio::test]
async fn why_a_provider_was_passed_over_does_not_carry_its_urls_password() {
    const FAILURE: &str = "https://relay:s3cr3t@gw.corp/v1/messages: connection refused";
    let asked = Arc::new(Mutex::new(Vec::new()));
    let failing = || {
        routed(
            ds(),
            Answer::FailWith(ModelErrorKind::Transport, FAILURE),
            &asked,
        )
    };
    let router = ProviderSet::new(
        vec![failing(), routed(claude(), Answer::Ok, &asked)],
        vec!["ds".into(), "claude".into()],
        TaskTiers::default(),
    )
    .unwrap()
    .session(None)
    .unwrap();
    let switches = Arc::new(Switches::default());
    router.observe(switches.clone());
    call(&router.model_for("main"), "ds-pro").await.unwrap();
    let seen = switches.0.lock().unwrap().clone();
    assert!(seen[0].reason.contains("gw.corp"), "{}", seen[0].reason);
    assert!(!seen[0].reason.contains("s3cr3t"), "{}", seen[0].reason);

    let mut down = claude();
    down.id = "other".into();
    let router = ProviderSet::new(
        vec![
            failing(),
            routed(
                down,
                Answer::FailWith(ModelErrorKind::Transport, FAILURE),
                &asked,
            ),
        ],
        vec!["ds".into(), "other".into()],
        TaskTiers::default(),
    )
    .unwrap()
    .session(None)
    .unwrap();
    let error = call(&router.model_for("main"), "ds-pro").await.unwrap_err();
    assert_eq!(error.kind, ModelErrorKind::ProvidersUnavailable);
    assert!(!error.to_string().contains("s3cr3t"), "{error}");
}

#[tokio::test]
async fn a_refused_request_is_not_sent_elsewhere() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let router = ProviderSet::new(
        vec![
            routed(ds(), Answer::Fail(ModelErrorKind::InvalidRequest), &asked),
            routed(claude(), Answer::Ok, &asked),
        ],
        vec!["ds".into(), "claude".into()],
        TaskTiers::default(),
    )
    .unwrap()
    .session(None)
    .unwrap();
    let err = call(&router.model_for("main"), "ds-pro").await.unwrap_err();
    assert_eq!(err.kind, ModelErrorKind::InvalidRequest);
    assert_eq!(*asked.lock().unwrap(), ["ds:ds-pro"]);
}

#[tokio::test]
async fn a_sole_provider_fails_with_its_own_error_and_all_failing_with_each_reason() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let alone = ProviderSet::new(
        vec![routed(
            ds(),
            Answer::Fail(ModelErrorKind::Overloaded),
            &asked,
        )],
        Vec::new(),
        TaskTiers::default(),
    )
    .unwrap()
    .session(None)
    .unwrap();
    let err = call(&alone.model_for("main"), "strong").await.unwrap_err();
    assert_eq!(err.kind, ModelErrorKind::Overloaded);

    let both = ProviderSet::new(
        vec![
            routed(ds(), Answer::Fail(ModelErrorKind::Auth), &asked),
            routed(claude(), Answer::Fail(ModelErrorKind::Overloaded), &asked),
        ],
        vec!["ds".into(), "claude".into()],
        TaskTiers::default(),
    )
    .unwrap()
    .session(None)
    .unwrap();
    let err = call(&both.model_for("compact"), "").await.unwrap_err();
    assert_eq!(err.kind, ModelErrorKind::ProvidersUnavailable);
    assert!(err.message.contains("`ds`: auth") && err.message.contains("`claude`: overloaded"));
    assert!(asked
        .lock()
        .unwrap()
        .ends_with(&["ds:ds-flash".into(), "claude:haiku".into()]));
}

#[tokio::test]
async fn a_provider_out_of_quota_is_skipped_after_the_call_that_used_it_up() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let mut limited = ds();
    limited.quota.day = Some(20);
    let router = ProviderSet::new(
        vec![
            routed(limited, Answer::Ok, &asked),
            routed(claude(), Answer::Ok, &asked),
        ],
        vec!["ds".into(), "claude".into()],
        TaskTiers::default(),
    )
    .unwrap()
    .with_quota(Arc::new(Ledger::default()))
    .session(None)
    .unwrap();
    let main = router.model_for("main");
    call(&main, "strong").await.unwrap();
    call(&main, "strong").await.unwrap();
    call(&main, "strong").await.unwrap();
    assert_eq!(
        *asked.lock().unwrap(),
        ["ds:ds-pro", "ds:ds-pro", "claude:opus"],
        "17 tokens a call, cache reads not counted: over 20 after the second"
    );
}

fn limited_ds_alone(answer: Answer, ledger: Arc<dyn QuotaLedger>) -> SessionRouter {
    let mut limited = ds();
    limited.quota.day = Some(1000);
    ProviderSet::new(
        vec![routed(limited, answer, &Arc::default())],
        vec![],
        TaskTiers::default(),
    )
    .unwrap()
    .with_quota(ledger)
    .session(None)
    .unwrap()
}

#[tokio::test]
async fn a_call_that_breaks_off_counts_what_its_opening_used() {
    let ledger = Arc::new(Ledger::default());
    let router = limited_ds_alone(Answer::BreaksOff, ledger.clone());
    let error = call(&router.model_for("main"), "strong").await.unwrap_err();
    assert_eq!(error.kind, ModelErrorKind::StreamInterrupted);
    ledger.settles_at("ds", 12).await;
}

#[tokio::test]
async fn a_call_dropped_midway_counts_what_its_opening_used() {
    use futures::StreamExt;
    let ledger = Arc::new(Ledger::default());
    let router = limited_ds_alone(Answer::Stalls, ledger.clone());
    let mut stream = router
        .model_for("main")
        .stream(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            params("strong"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(stream.next().await.is_some());
    drop(stream);
    ledger.settles_at("ds", 12).await;
}

/// Holds every write until it is let go, failing the first `failures` of
/// them as a lock that could not be had would.
struct HeldLedger {
    counted: Ledger,
    gate: Mutex<std::sync::mpsc::Receiver<()>>,
    failures: Mutex<u32>,
}

impl QuotaLedger for HeldLedger {
    fn status(&self, provider: &str, quota: &Quota) -> QuotaStatus {
        self.counted.status(provider, quota)
    }

    fn record(&self, provider: &str, tokens: u64) -> Result<(), String> {
        self.gate.lock().unwrap().recv().unwrap();
        let mut failures = self.failures.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            return Err("the directory could not be locked".into());
        }
        self.counted.record(provider, tokens)
    }
}

#[tokio::test]
async fn recording_usage_holds_up_no_call_and_a_write_that_fails_is_tried_again() {
    let (release, gate) = std::sync::mpsc::channel();
    let ledger = Arc::new(HeldLedger {
        counted: Ledger::default(),
        gate: Mutex::new(gate),
        failures: Mutex::new(2),
    });
    let asked = Arc::new(Mutex::new(Vec::new()));
    let mut limited = ds();
    limited.quota.day = Some(20);
    let router = ProviderSet::new(
        vec![
            routed(limited, Answer::Ok, &asked),
            routed(claude(), Answer::Ok, &asked),
        ],
        vec!["ds".into(), "claude".into()],
        TaskTiers::default(),
    )
    .unwrap()
    .with_quota(ledger.clone())
    .session(None)
    .unwrap();
    let main = router.model_for("main");
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for _ in 0..3 {
            call(&main, "strong").await.unwrap();
        }
    })
    .await
    .expect("the calls finish while the ledger is held");
    assert_eq!(
        *asked.lock().unwrap(),
        ["ds:ds-pro", "ds:ds-pro", "claude:opus"],
        "what is not written yet counts against the quota"
    );
    for _ in 0..4 {
        release.send(()).unwrap();
    }
    ledger.counted.settles_at("ds", 34).await;
}

#[tokio::test]
async fn a_session_may_put_another_provider_first() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let router = ProviderSet::new(
        vec![
            routed(ds(), Answer::Ok, &asked),
            routed(claude(), Answer::Ok, &asked),
        ],
        vec!["ds".into()],
        TaskTiers::default(),
    )
    .unwrap();
    let session = router.session(Some("claude")).unwrap();
    assert_eq!(session.order(), ["claude", "ds"]);
    assert_eq!(
        session.model_name_for("subagent").as_deref(),
        Some("sonnet")
    );
    assert_eq!(session.resolve_model("fast"), "haiku");
    assert!(router.session(Some("ghost")).is_err());
    assert_eq!(session.model_name_for("web_fetch"), None);
}

#[tokio::test]
async fn two_calls_open_at_once_through_one_model_each_name_their_own_provider() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let session = ProviderSet::new(
        vec![
            routed(
                ds(),
                Answer::FailOn("ds-flash", ModelErrorKind::Overloaded),
                &asked,
            ),
            routed(claude(), Answer::Ok, &asked),
        ],
        vec!["ds".into(), "claude".into()],
        TaskTiers::default(),
    )
    .unwrap()
    .session(None)
    .unwrap();
    let shared = session.model_for("main");
    let on_ds = open(&shared, "ds-pro").await.unwrap();
    let on_claude = open(&shared, "ds-flash").await.unwrap();
    assert_eq!(
        *asked.lock().unwrap(),
        ["ds:ds-pro", "ds:ds-flash", "claude:sonnet"]
    );
    let (first, second) = tokio::join!(served_by(on_ds), served_by(on_claude));
    assert_eq!(first.unwrap().as_deref(), Some("ds"));
    assert_eq!(second.unwrap().as_deref(), Some("claude"));
}

#[tokio::test]
async fn a_route_starting_at_a_provider_sends_there_and_reports_its_own_switches() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let set = ProviderSet::new(
        vec![
            routed(ds(), Answer::Ok, &asked),
            routed(claude(), Answer::Fail(ModelErrorKind::ServerError), &asked),
        ],
        vec!["ds".into(), "claude".into()],
        TaskTiers::default(),
    )
    .unwrap();
    let parent = set.session(None).unwrap();
    let parent_switches = Arc::new(Switches::default());
    parent.observe(parent_switches.clone());
    let child = parent.starting_at("claude").unwrap();
    let child_switches = Arc::new(Switches::default());
    child.observe(child_switches.clone());

    assert_eq!(
        child.destination("subagent", ""),
        Destination {
            provider: "claude".into(),
            model: "sonnet".into()
        }
    );
    let served = call(&child.model_for("subagent"), "").await.unwrap();
    assert_eq!(served.as_deref(), Some("ds"));
    assert_eq!(*asked.lock().unwrap(), ["claude:sonnet", "ds:ds-flash"]);
    assert_eq!(child_switches.0.lock().unwrap().len(), 1);
    assert!(parent_switches.0.lock().unwrap().is_empty());
    assert!(parent.starting_at("nobody").is_err());
}

#[tokio::test]
async fn a_route_starting_at_a_provider_falls_back_along_its_parents_route() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let own = provider(
        "own",
        &[],
        [Some("own-strong"), Some("own-balanced"), Some("own-fast")],
    );
    let set = ProviderSet::new(
        vec![
            routed(ds(), Answer::Ok, &asked),
            routed(claude(), Answer::Fail(ModelErrorKind::ServerError), &asked),
            routed(own, Answer::Ok, &asked),
        ],
        vec!["ds".into(), "claude".into()],
        TaskTiers::default(),
    )
    .unwrap();
    let parent = set.session(Some("own")).unwrap();
    let child = parent.starting_at("claude").unwrap();
    let served = call(&child.model_for("subagent"), "").await.unwrap();
    assert_eq!(served.as_deref(), Some("own"));
    assert_eq!(
        *asked.lock().unwrap(),
        ["claude:sonnet", "own:own-balanced"]
    );
    assert!(child.in_order("own") && child.in_order("ds"));
    assert!(!set.session(None).unwrap().in_order("own"));
}

#[test]
fn a_model_has_a_home_only_where_it_is_listed_or_a_tier() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let set = ProviderSet::new(
        vec![
            routed(ds(), Answer::Ok, &asked),
            routed(claude(), Answer::Ok, &asked),
        ],
        vec!["ds".into(), "claude".into()],
        TaskTiers::default(),
    )
    .unwrap();
    let router = set.session(None).unwrap();
    assert_eq!(router.home_of("ds-flash").as_deref(), Some("ds"));
    assert_eq!(router.home_of("sonnet").as_deref(), Some("claude"));
    assert_eq!(router.home_of("anything-else"), None);
    assert_eq!(
        router.destination("subagent", "sonnet"),
        Destination {
            provider: "ds".into(),
            model: "ds-flash".into()
        }
    );
}
