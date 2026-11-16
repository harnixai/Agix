//! The model name each route carries is one its provider was configured with,
//! seen at the providers' endpoints.

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

fn provider(base_url: String, models: &[&str]) -> base::provider::ProviderConfig {
    base::provider::ProviderConfig {
        api_type: Some("anthropic".into()),
        base_url: Some(base_url),
        api_key: Some("k".into()),
        default_model: models.first().map(|m| m.to_string()),
        models: models.iter().map(|m| (*m).into()).collect(),
        ..Default::default()
    }
}

/// A daemon whose settings' model is `settings_model`, with providers `one`
/// (the default) and `two` on `stub`, and `task_models` as given.
async fn daemon(
    stub: &Stub,
    settings_model: &'static str,
    task_models: Value,
) -> crate::support::Served {
    let one = provider(format!("{}/one", stub.base), &["one-a", "one-b"]);
    let two = provider(format!("{}/two", stub.base), &["two-a", "two-b"]);
    crate::support::serve_permitting_all(
        Arc::new(MockAnthropicClient::new()),
        crate::support::Serving {
            model: settings_model,
            ..Default::default()
        },
        |config| {
            let values = &mut config.settings.values;
            values.providers.insert("one".into(), one);
            values.providers.insert("two".into(), two);
            values.default_provider = Some("one".into());
            values.task_models = serde_json::from_value(task_models).unwrap();
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
async fn the_conversation_asks_for_the_model_task_models_main_names() {
    let stub = Stub::start(route).await;
    let served = daemon(
        &stub,
        "one-a",
        json!({"main": {"provider": "two", "model": "two-b"}}),
    )
    .await;
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
