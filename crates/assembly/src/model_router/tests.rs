use super::*;
use base::interface::model::{ModelErrorKind, ModelEvent, StreamParams, ThinkingMode};
use base::provider::{ApiType, ProviderConfig};

/// `Result::unwrap_err()` requires the `Ok` type to be `Debug`, which
/// `TaskRouter` is not.
fn err_of<T>(r: Result<T, String>) -> String {
    match r {
        Ok(_) => panic!("expected Err"),
        Err(e) => e,
    }
}

fn builtin() -> Arc<dyn AnthropicClient> {
    Arc::new(NoCredentialsClient::new(
        "the built-in provider is not used",
    ))
}

fn settings(providers: serde_json::Value, order: &[&str]) -> Settings {
    let mut settings = Settings::defaults_for("m");
    settings.providers = serde_json::from_value(providers).unwrap();
    settings.model.order = order.iter().map(|s| s.to_string()).collect();
    settings
}

fn keyed(api: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "id": api, "api": api, "api_key_env": "PATH", "tiers": {"strong": "m"},
    });
    for (k, v) in extra.as_object().unwrap() {
        entry[k] = v.clone();
    }
    entry
}

fn build(settings: &Settings) -> Result<TaskRouter, String> {
    build_task_router_with(settings, &builtin(), &model::factory::builtin_registry())
}

#[test]
fn every_defined_provider_is_built_and_the_order_is_kept() {
    let settings = settings(
        serde_json::json!([
            keyed("anthropic", serde_json::json!({})),
            keyed(
                "openai_chat",
                serde_json::json!({"base_url": "https://api.example.com/v1"})
            ),
        ]),
        &["openai_chat", "anthropic"],
    );
    let router = build(&settings).unwrap();
    assert_eq!(router.order(), ["openai_chat", "anthropic"]);
    assert_eq!(
        router.provider("openai_chat").unwrap().api_type(),
        ApiType::OpenAIChat
    );
}

#[test]
fn a_provider_that_cannot_be_built_says_which_and_why() {
    for (entry, expected) in [
        (keyed("openai_chat", serde_json::json!({})), "base_url"),
        (
            keyed("anthropic", serde_json::json!({"api": "bedrock"})),
            "bedrock",
        ),
        (
            keyed("anthropic", serde_json::json!({"base_url": "not a url"})),
            "invalid base_url",
        ),
        (
            keyed("anthropic", serde_json::json!({"models": ["x"]})),
            "tiers.strong",
        ),
    ] {
        let err = err_of(build(&settings(serde_json::json!([entry]), &[])));
        assert!(err.contains(expected), "{expected}: {err}");
    }
    let err = err_of(build(&settings(
        serde_json::json!([keyed("anthropic", serde_json::json!({}))]),
        &["anthropic", "ghost"],
    )));
    assert!(err.contains("ghost"), "{err}");
}

fn params() -> StreamParams {
    StreamParams {
        model: "strong".into(),
        max_tokens: 100,
        thinking_mode: ThinkingMode::Off,
        effort: None,
        cache_edits: Vec::new(),
        origin: None,
        input_map: None,
        provider_search: None,
    }
}

struct Answers;

#[async_trait::async_trait]
impl Model for Answers {
    fn api_type(&self) -> ApiType {
        ApiType::OpenAIChat
    }

    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<base::interface::model::ModelMessage>,
        params: StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        Ok(Box::new(futures::stream::iter([Ok(
            ModelEvent::TextDelta { text: params.model },
        )])))
    }
}

struct AnswersFactory;

impl base::interface::model_factory::ModelFactory for AnswersFactory {
    fn api_type(&self) -> &str {
        "fictional"
    }

    fn build(
        &self,
        _provider_id: &str,
        _cfg: &ProviderConfig,
        _credentials: &dyn CredentialSource,
        _network: Option<&Arc<dyn base::interface::exec::Network>>,
    ) -> Result<Arc<dyn Model>, String> {
        Ok(Arc::new(Answers))
    }
}

/// A protocol the engine does not implement, reachable by configuration
/// alone; and a provider with no key does not stop the router, it hands its
/// calls on.
#[tokio::test]
async fn a_provider_without_a_key_hands_its_calls_to_a_registered_protocol() {
    use futures::StreamExt;
    let mut factories = model::factory::builtin_registry();
    factories.register(Arc::new(AnswersFactory));
    let settings = settings(
        serde_json::json!([
            {"id": "keyless", "api": "anthropic", "api_key_env": "AGIX_TEST_KEY_THAT_IS_NOT_SET",
             "tiers": {"strong": "k"}},
            {"id": "mine", "api": "fictional", "api_key_env": "PATH", "tiers": {"strong": "fic-strong"}},
        ]),
        &["keyless", "mine"],
    );
    let router = build_task_router_with(&settings, &builtin(), &factories).unwrap();
    let mut stream = router
        .model_for("main")
        .stream(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            params(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("the second provider takes the call");
    match stream.next().await {
        Some(Ok(ModelEvent::TextDelta { text })) => assert_eq!(text, "fic-strong"),
        other => panic!("{other:?}"),
    }

    let alone = settings_with_only_keyless();
    let router = build_task_router_with(&alone, &builtin(), &factories).unwrap();
    let Err(error) = router
        .model_for("main")
        .stream(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            params(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
    else {
        panic!("nothing can take the call");
    };
    assert_eq!(error.kind, ModelErrorKind::NoCredentials);
    assert!(error.message.contains("keyless"), "{error}");
}

fn settings_with_only_keyless() -> Settings {
    settings(
        serde_json::json!([
            {"id": "keyless", "api": "anthropic", "api_key_env": "AGIX_TEST_KEY_THAT_IS_NOT_SET",
             "tiers": {"strong": "k"}},
        ]),
        &[],
    )
}

/// What the daemon builds every binding's router with when the host hands a
/// source in: entries that name a variable use it, the rest ask the host.
#[test]
fn the_router_asks_the_host_for_keys_the_settings_leave_out() {
    use base::interface::credentials::{ConfigThen, Secret, StaticCredentials};
    let host: Arc<dyn CredentialSource> =
        Arc::new(ConfigThen(Arc::new(StaticCredentials::new([(
            "vaulted".to_string(),
            Secret::new("sk-from-the-vault"),
        )]))));
    let factories = model::factory::builtin_registry().with_credentials(host);
    let vaulted = ProviderConfig {
        id: "vaulted".into(),
        ..Default::default()
    };
    assert_eq!(factories.missing_credential("vaulted", &vaulted), None);
    assert!(factories
        .missing_credential("other", &ProviderConfig::default())
        .is_some());
}
