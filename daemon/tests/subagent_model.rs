//! Which model a sub-agent asks for when settings route sub-agents to a
//! provider of their own, seen at that provider's endpoint.

use std::sync::Arc;
use std::time::Duration;

use model::mock::MockAnthropicClient;
use serde_json::json;

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

/// The first request the `sub` provider received after `seen_before` others.
async fn next_sub_request(stub: &Stub, seen_before: usize) -> Seen {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let sub: Vec<Seen> = stub
            .seen()
            .into_iter()
            .filter(|s| s.path.starts_with("/sub/"))
            .collect();
        if let Some(request) = sub.into_iter().nth(seen_before) {
            return request;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the sub-agent never reached its provider: {:?}",
            stub.seen()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_sub_agent_asks_for_the_model_task_models_routes_sub_agents_to() {
    let stub = Stub::start(route).await;
    let main = provider(format!("{}/main", stub.base), &["main-a", "main-b"]);
    let sub = provider(format!("{}/sub", stub.base), &["sub-a", "sub-b"]);
    let served = crate::support::serve_permitting_all(
        Arc::new(MockAnthropicClient::new()),
        Default::default(),
        |config| {
            let values = &mut config.settings.values;
            values.providers.insert("main".into(), main);
            values.providers.insert("sub".into(), sub);
            values.default_provider = Some("main".into());
            values.task_models =
                serde_json::from_value(json!({"subagent": {"provider": "sub", "model": "sub-b"}}))
                    .unwrap();
        },
    )
    .await;
    let session = crate::support::call(&served.sock, "session.create", json!({})).await
        ["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let switched = crate::support::call_frame(
        &served.sock,
        "session.setModel",
        json!({"session_id": session, "model": "main-b"}),
    )
    .await;
    assert!(switched.get("result").is_some(), "{switched}");

    let started = crate::support::call_frame(
        &served.sock,
        "session.startAgent",
        json!({"session_id": session, "prompt": "SUB-ROUTED"}),
    )
    .await;
    assert!(started.get("result").is_some(), "{started}");
    let request = next_sub_request(&stub, 0).await;
    assert_eq!(
        request.body["model"], "sub-b",
        "task_models.subagent names the model, over the session's own choice"
    );

    let started = crate::support::call_frame(
        &served.sock,
        "session.startAgent",
        json!({"session_id": session, "prompt": "SUB-NAMED", "model": "sub-a"}),
    )
    .await;
    assert!(started.get("result").is_some(), "{started}");
    let request = next_sub_request(&stub, 1).await;
    assert_eq!(
        request.body["model"], "sub-a",
        "a model named for the spawn itself comes first"
    );
}
