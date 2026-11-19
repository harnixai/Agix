//! Which model a sub-agent asks for: its tier at the first provider, or the
//! tier, alias or model its spawn names — seen at the provider's endpoint.

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

/// The first request carrying `needle` the provider received.
async fn request_with(stub: &Stub, needle: &str) -> Seen {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let found = stub
            .seen()
            .into_iter()
            .find(|s| s.path.starts_with("/p/") && s.body.to_string().contains(needle));
        if let Some(request) = found {
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
async fn a_sub_agent_asks_for_its_tier_or_what_its_spawn_names() {
    let stub = Stub::start(route).await;
    let provider: base::provider::ProviderConfig = serde_json::from_value(json!({
        "id": "p", "api": "anthropic", "base_url": format!("{}/p", stub.base),
        "models": ["p-a", "p-b", "p-c"],
        "tiers": {"strong": "p-a", "balanced": "p-b", "fast": "p-c"},
    }))
    .unwrap();
    let served = crate::support::serve_permitting_all(
        Arc::new(MockAnthropicClient::new()),
        crate::support::Serving {
            keys: &[("p", "k")],
            ..Default::default()
        },
        |config| {
            config.settings.values.providers = vec![provider];
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
        json!({"session_id": session, "model": "fast"}),
    )
    .await;
    assert!(switched.get("result").is_some(), "{switched}");

    for (prompt, named, expected) in [
        ("SUB-UNNAMED", None, "p-b"),
        ("SUB-TIER", Some("strong"), "p-a"),
        ("SUB-ALIAS", Some("haiku"), "p-c"),
        ("SUB-INHERIT", Some("inherit"), "p-c"),
        ("SUB-ID", Some("p-a"), "p-a"),
    ] {
        let mut params = json!({"session_id": session, "prompt": prompt});
        if let Some(named) = named {
            params["model"] = json!(named);
        }
        let started = crate::support::call_frame(&served.sock, "session.startAgent", params).await;
        assert!(started.get("result").is_some(), "{started}");
        assert_eq!(
            request_with(&stub, prompt).await.body["model"],
            expected,
            "{prompt}"
        );
    }
}
