//! A session can be pointed at a provider of its own, and a model is checked
//! against the provider it will be sent to.
//!
//! The providers here are configured but never reached: these tests are about
//! which choices the daemon accepts and keeps, not about the HTTP that follows.

use std::sync::Arc;

use model::mock::MockAnthropicClient;

const INVALID_PARAMS: i64 = -32602;

struct Daemon {
    sock: std::path::PathBuf,
    _dir: tempfile::TempDir,
    _handle: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        crate::support::call_frame(&self.sock, method, params).await
    }

    async fn ok(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let response = self.call(method, params).await;
        assert!(
            response.get("result").is_some(),
            "{method} failed: {response}"
        );
        response["result"].clone()
    }

    async fn session(&self, options: serde_json::Value) -> String {
        self.ok("session.create", serde_json::json!({"options": options}))
            .await["session_id"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

fn provider(id: &str, models: &[&str]) -> base::provider::ProviderConfig {
    base::provider::ProviderConfig {
        id: id.into(),
        api: Some("anthropic".into()),
        base_url: Some("http://127.0.0.1:9".into()),
        models: models.iter().map(|m| (*m).into()).collect(),
        tiers: base::provider::Tiers {
            strong: models.first().map(|m| m.to_string()),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn start_daemon() -> Daemon {
    let served = crate::support::serve_permitting_all(
        Arc::new(MockAnthropicClient::new()),
        Default::default(),
        |config| {
            config.settings.values.providers = vec![
                provider("one", &["one-a"]),
                provider("two", &["two-a", "two-b"]),
            ];
        },
    )
    .await;
    crate::support::trust_projects_under(served.dir.path(), served.dir.path());
    Daemon {
        sock: served.sock,
        _dir: served.dir,
        _handle: served.handle,
    }
}

#[tokio::test]
async fn a_provider_nobody_configured_is_refused_at_creation() {
    let daemon = start_daemon().await;
    let response = daemon
        .call(
            "session.create",
            serde_json::json!({"options": {"provider": "nope"}}),
        )
        .await;
    assert_eq!(response["error"]["code"], INVALID_PARAMS, "{response}");
}

#[tokio::test]
async fn a_model_is_checked_against_the_provider_it_would_go_to() {
    let daemon = start_daemon().await;
    let session = daemon.session(serde_json::json!({"provider": "two"})).await;

    let refused = daemon
        .call(
            "session.setModel",
            serde_json::json!({"session_id": session, "model": "one-a"}),
        )
        .await;
    assert_eq!(
        refused["error"]["code"], INVALID_PARAMS,
        "one-a is provider one's model, and this session talks to two: {refused}"
    );

    let accepted = daemon
        .ok(
            "session.setModel",
            serde_json::json!({"session_id": session, "model": "two-b"}),
        )
        .await;
    assert_eq!(accepted["model"], "two-b");
    assert_eq!(accepted["provider"], "two");
}

#[tokio::test]
async fn a_session_moves_to_another_provider_and_stays_there() {
    let daemon = start_daemon().await;
    let session = daemon.session(serde_json::json!({})).await;

    let switched = daemon
        .ok(
            "session.setModel",
            serde_json::json!({"session_id": session, "provider": "two"}),
        )
        .await;
    assert_eq!(switched["provider"], "two");
    assert!(
        switched["model"].is_null(),
        "no model was chosen for the new provider, so `model.tasks.main` applies: {switched}"
    );

    daemon.ok("config.reload", serde_json::json!({})).await;

    let refused = daemon
        .call(
            "session.setModel",
            serde_json::json!({"session_id": session, "model": "one-a"}),
        )
        .await;
    assert_eq!(
        refused["error"]["code"], INVALID_PARAMS,
        "after a reload the session still talks to provider two: {refused}"
    );
}

#[tokio::test]
async fn a_provider_added_while_a_session_is_open_can_be_chosen_by_it() {
    let daemon = start_daemon().await;
    let session = daemon.session(serde_json::json!({})).await;

    daemon
        .ok(
            "config.setProvider",
            serde_json::json!({
                "provider_id": "three",
                "config": {
                    "api": "anthropic",
                    "base_url": "http://127.0.0.1:9",
                    "tiers": {"strong": "three-a"},
                    "models": ["three-a"]
                }
            }),
        )
        .await;

    let switched = daemon
        .ok(
            "session.setModel",
            serde_json::json!({"session_id": session, "provider": "three", "model": "three-a"}),
        )
        .await;
    assert_eq!(switched["provider"], "three");
    assert_eq!(switched["model"], "three-a");
}
