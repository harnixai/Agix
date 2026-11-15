//! `session.setModel` changes what the next turn asks for, and keeps it.
//!
//! Without it the only way to change a running session's model was to change
//! the configuration and start a new session, which costs the conversation.
//!
//! What the client asked for has to outlive the `Agent` it was asked of. A
//! session is rebuilt from settings whenever a reload invalidates its
//! binding, and the model would go back to the configured one with
//! nothing said to whoever chose otherwise — the second test is that case.

use std::sync::Arc;

use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};

const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const CHOSEN_MODEL: &str = "claude-opus-4-6";

/// One assistant turn that says "ok" and stops.
fn one_turn() -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::TextDelta { text: "ok".into() },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::EndTurn),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        },
    ]
}

struct Daemon {
    sock: std::path::PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
    _handle: tokio::task::JoinHandle<()>,
}

impl Daemon {
    /// One call on its own connection, draining any stream frames.
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

    /// The model named in the first request that carried `needle`.
    ///
    /// By content rather than by index, and the first match rather than the
    /// last: a turn is not the only thing that reaches the provider — session
    /// naming runs afterwards, on a task model of its own, over the same
    /// conversation — so both counting requests and taking the last match
    /// would be answering about the wrong call.
    fn model_asked_for(&self, needle: &str) -> String {
        for i in 0.. {
            let Some(request) = self.model.nth_request(i) else {
                break;
            };
            let rendered = serde_json::to_string(&request).expect("a request serializes");
            if rendered.contains(needle) {
                return request.model;
            }
        }
        panic!("no request carried `{needle}`")
    }
}

async fn start_daemon(turns: usize) -> Daemon {
    let model = Arc::new(MockAnthropicClient::new());
    for _ in 0..turns {
        model.push_turn(one_turn());
    }
    let served = crate::support::serve_permitting_all(
        model.clone(),
        crate::support::Serving {
            model: DEFAULT_MODEL,
            history: true,
            ..Default::default()
        },
        |_| {},
    )
    .await;
    Daemon {
        sock: served.sock,
        model,
        _dir: served.dir,
        _handle: served.handle,
    }
}

#[tokio::test]
async fn the_model_a_session_was_told_to_use_is_the_one_it_asks_for() {
    let daemon = start_daemon(2).await;
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "MSG-BEFORE"}),
        )
        .await;
    assert_eq!(daemon.model_asked_for("MSG-BEFORE"), DEFAULT_MODEL);

    let switched = daemon
        .ok(
            "session.setModel",
            serde_json::json!({"session_id": session, "model": CHOSEN_MODEL}),
        )
        .await;
    assert_eq!(switched["model"], CHOSEN_MODEL);
    assert_eq!(
        switched["applies"], "next_turn",
        "the switch is for the next call, and the response says so"
    );

    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "MSG-AFTER"}),
        )
        .await;
    assert_eq!(
        daemon.model_asked_for("MSG-AFTER"),
        CHOSEN_MODEL,
        "the turn after the switch went to the model the client chose"
    );
}

/// A reload rebuilds an already-running session's `Agent` from settings
/// before its next turn. Without the pool replaying the choice onto the new
/// `Agent`, that turn silently goes back to the configured model.
#[tokio::test]
async fn a_reload_does_not_quietly_put_the_model_back() {
    let daemon = start_daemon(2).await;
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "MSG-BEFORE"}),
        )
        .await;
    daemon
        .ok(
            "session.setModel",
            serde_json::json!({"session_id": session, "model": CHOSEN_MODEL}),
        )
        .await;

    daemon.ok("config.reload", serde_json::json!({})).await;

    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "MSG-AFTER-RELOAD"}),
        )
        .await;
    assert_eq!(
        daemon.model_asked_for("MSG-AFTER-RELOAD"),
        CHOSEN_MODEL,
        "a session rebuilt by a reload still talks to the model the client chose"
    );
}

#[tokio::test]
async fn setting_a_model_on_a_session_that_is_not_running_is_not_found() {
    let daemon = start_daemon(0).await;
    let response = daemon
        .call(
            "session.setModel",
            serde_json::json!({"session_id": "no-such-session", "model": CHOSEN_MODEL}),
        )
        .await;
    assert_eq!(
        response["error"]["code"], -32000,
        "SESSION_NOT_FOUND: {response}"
    );
}

#[tokio::test]
async fn a_model_is_required() {
    let daemon = start_daemon(0).await;
    let session = daemon.ok("session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let response = daemon
        .call(
            "session.setModel",
            serde_json::json!({"session_id": session, "model": "  "}),
        )
        .await;
    assert_eq!(
        response["error"]["code"], -32602,
        "INVALID_PARAMS: {response}"
    );
}
