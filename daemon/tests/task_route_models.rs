//! The model each call asks for is its task's tier at the provider it goes
//! to, seen at the providers' endpoints.

use std::sync::Arc;
use std::time::Duration;

use model::mock::MockAnthropicClient;
use serde_json::{json, Value};

use super::list_models::{http, sse_text, Seen, Stub};

fn route(path: &str) -> String {
    if path.ends_with("/v1/messages") {
        return sse_text("ok");
    }
    http(500, r#"{"error":{"message":"unexpected path"}}"#)
}

fn provider(id: &str, base_url: String) -> base::provider::ProviderConfig {
    base::provider::ProviderConfig {
        id: id.into(),
        api: Some("anthropic".into()),
        base_url: Some(base_url),
        models: [format!("{id}-a"), format!("{id}-b")]
            .into_iter()
            .map(Into::into)
            .collect(),
        tiers: base::provider::Tiers {
            strong: Some(format!("{id}-a")),
            balanced: Some(format!("{id}-b")),
            fast: None,
        },
        ..Default::default()
    }
}

/// A daemon with providers `one` and `two` on `stub`, in `order`, and
/// `model.tasks` as given.
async fn daemon(stub: &Stub, order: &[&str], tasks: Value) -> crate::support::Served {
    let one = provider("one", format!("{}/one", stub.base));
    let two = provider("two", format!("{}/two", stub.base));
    let order: Vec<String> = order.iter().map(|id| id.to_string()).collect();
    crate::support::serve_permitting_all(
        Arc::new(MockAnthropicClient::new()),
        crate::support::Serving {
            keys: &[("one", "k-one"), ("two", "k-two")],
            ..Default::default()
        },
        |config| {
            let values = &mut config.settings.values;
            values.providers = vec![one, two];
            values.model.order = order;
            values.model.tasks = serde_json::from_value(tasks).unwrap();
        },
    )
    .await
}

/// The first request to reach `prefix` that carries `needle`.
async fn request_to(stub: &Stub, prefix: &str, needle: &str) -> Seen {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let found = stub
            .seen()
            .into_iter()
            .find(|s| s.path.starts_with(prefix) && s.body.to_string().contains(needle));
        if let Some(request) = found {
            return request;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "nothing carrying `{needle}` reached {prefix}: {:?}",
            stub.seen()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn session(served: &crate::support::Served, options: Value) -> String {
    crate::support::call(&served.sock, "session.create", json!({"options": options})).await
        ["session_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn the_conversation_asks_the_first_provider_for_the_main_tier() {
    let stub = Stub::start(route).await;
    let served = daemon(&stub, &["two", "one"], json!({"main": "balanced"})).await;
    let session = session(&served, json!({})).await;
    let turn = crate::support::call_frame(
        &served.sock,
        "session.run_turn",
        json!({"session_id": session, "message": "MAIN-ROUTED"}),
    )
    .await;
    assert!(turn.get("result").is_some(), "{turn}");
    assert_eq!(
        request_to(&stub, "/two/", "MAIN-ROUTED").await.body["model"],
        "two-b"
    );
}

async fn start_agent(served: &crate::support::Served, session: &str, prompt: &str) {
    let started = crate::support::call_frame(
        &served.sock,
        "session.startAgent",
        json!({"session_id": session, "prompt": prompt}),
    )
    .await;
    assert!(started.get("result").is_some(), "{started}");
}

#[tokio::test]
async fn a_sub_agent_asks_for_the_subagent_tier() {
    let stub = Stub::start(route).await;
    let served = daemon(&stub, &["one"], json!({})).await;
    let session = session(&served, json!({})).await;
    start_agent(&served, &session, "SUB-ON-ONE").await;
    assert_eq!(
        request_to(&stub, "/one/", "SUB-ON-ONE").await.body["model"],
        "one-b"
    );
}

#[tokio::test]
async fn a_provider_a_session_names_takes_its_sub_agents_too() {
    let stub = Stub::start(route).await;
    let served = daemon(&stub, &["two", "one"], json!({})).await;
    let session = session(&served, json!({"provider": "one"})).await;
    start_agent(&served, &session, "SUB-ON-NAMED").await;
    assert_eq!(
        request_to(&stub, "/one/", "SUB-ON-NAMED").await.body["model"],
        "one-b"
    );
}
