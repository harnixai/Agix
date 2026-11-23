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

use base::interface::model::{Effort, ThinkingMode};
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
        self.request_carrying(needle).model
    }

    fn request_carrying(&self, needle: &str) -> model::types::MessagesRequest {
        for i in 0.. {
            let Some(request) = self.model.nth_request(i) else {
                break;
            };
            let rendered = serde_json::to_string(&request).expect("a request serializes");
            if rendered.contains(needle) {
                return request;
            }
        }
        panic!("no request carried `{needle}`")
    }

    async fn create(&self, params: serde_json::Value) -> String {
        self.ok("session.create", params).await["session_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn turn(&self, session: &str, message: &str) {
        self.ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": message}),
        )
        .await;
    }
}

async fn start_daemon(turns: usize) -> Daemon {
    start_daemon_with(turns, |_| {}).await
}

async fn start_daemon_with(
    turns: usize,
    configure: impl FnOnce(&mut daemon::config::DaemonConfig),
) -> Daemon {
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
        configure,
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

/// Settings that ask for `low` effort and leave thinking to the endpoint, so
/// a session's own choice is told apart from them.
async fn start_reasoning_daemon(turns: usize) -> Daemon {
    start_daemon_with(turns, |config| {
        config.settings.values.model.effort = Some(Effort::Low);
        config.settings.values.model.thinking_mode = ThinkingMode::Auto;
    })
    .await
}

#[tokio::test]
async fn an_effort_set_on_one_session_is_what_its_next_call_carries_and_no_others() {
    let daemon = start_reasoning_daemon(4).await;
    let chosen = daemon.create(serde_json::json!({})).await;
    let other = daemon.create(serde_json::json!({})).await;

    let answer = daemon
        .ok(
            "session.setModel",
            serde_json::json!({"session_id": chosen, "effort": "high", "thinking": "off"}),
        )
        .await;
    assert_eq!(answer["effort"], "high", "{answer}");
    assert_eq!(answer["thinking"], "off", "{answer}");
    assert_eq!(answer["model"], serde_json::Value::Null, "{answer}");

    daemon.turn(&chosen, "MSG-CHOSEN").await;
    daemon.turn(&other, "MSG-OTHER").await;
    let request = daemon.request_carrying("MSG-CHOSEN");
    assert_eq!(request.effort, Some(Effort::High));
    assert!(
        matches!(
            request.thinking,
            Some(model::types::ThinkingConfig::Disabled)
        ),
        "{:?}",
        request.thinking
    );
    let request = daemon.request_carrying("MSG-OTHER");
    assert_eq!(
        request.effort,
        Some(Effort::Low),
        "the other session is left on the settings"
    );
    assert!(request.thinking.is_none(), "{:?}", request.thinking);

    let got = daemon
        .ok("session.get", serde_json::json!({"session_id": chosen}))
        .await;
    assert_eq!(
        got["reasoning"],
        serde_json::json!({
            "effort": "high", "effort_source": "session",
            "thinking": "off", "thinking_source": "session",
        })
    );
    let got = daemon
        .ok("session.get", serde_json::json!({"session_id": other}))
        .await;
    assert_eq!(
        got["reasoning"],
        serde_json::json!({
            "effort": "low", "effort_source": "settings",
            "thinking": "auto", "thinking_source": "settings",
        })
    );
    let inspected = daemon
        .ok(
            "session.inspectContext",
            serde_json::json!({"session_id": chosen}),
        )
        .await;
    assert_eq!(inspected["reasoning"]["effort"], "high", "{inspected}");
    assert_eq!(inspected["reasoning"]["effort_source"], "session");

    let answer = daemon
        .ok(
            "session.setModel",
            serde_json::json!({"session_id": chosen, "effort": null}),
        )
        .await;
    assert_eq!(answer["effort"], serde_json::Value::Null, "{answer}");
    assert_eq!(
        answer["thinking"], "off",
        "only the named field is taken back"
    );
    daemon.turn(&chosen, "MSG-TAKEN-BACK").await;
    assert_eq!(
        daemon.request_carrying("MSG-TAKEN-BACK").effort,
        Some(Effort::Low),
        "`null` puts the session back on the settings' effort"
    );
}

#[tokio::test]
async fn a_thinking_budget_under_the_minimum_is_invalid_params() {
    let daemon = start_reasoning_daemon(0).await;
    let refused = daemon
        .call(
            "session.create",
            serde_json::json!({"options": {"thinking": {"on_budget": 0}}}),
        )
        .await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    assert_eq!(
        refused["error"]["data"]["invalid_options"][0]["field"], "thinking",
        "{refused}"
    );

    let session = daemon.create(serde_json::json!({})).await;
    let refused = daemon
        .call(
            "session.setModel",
            serde_json::json!({"session_id": session, "thinking": {"on_budget": 1023}}),
        )
        .await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    daemon
        .ok(
            "session.setModel",
            serde_json::json!({"session_id": session, "thinking": {"on_budget": 1024}}),
        )
        .await;
}

#[tokio::test]
async fn a_thinking_budget_is_kept_under_the_calls_max_tokens() {
    let daemon = start_daemon_with(2, |config| {
        config.settings.values.model.max_tokens = 1500;
        config.settings.values.model.thinking_mode = ThinkingMode::Auto;
    })
    .await;
    let roomy = daemon
        .create(serde_json::json!({"options": {"thinking": {"on_budget": 8000}}}))
        .await;
    daemon.turn(&roomy, "MSG-BUDGET").await;
    let request = daemon.request_carrying("MSG-BUDGET");
    assert_eq!(request.max_tokens, 1500);
    match request.thinking {
        Some(model::types::ThinkingConfig::Enabled { budget_tokens }) => {
            assert!((1024..1500).contains(&budget_tokens), "{budget_tokens}")
        }
        other => panic!("expected a budget under max_tokens, got {other:?}"),
    }
}

#[tokio::test]
async fn the_effort_options_ask_for_is_what_the_session_calls_with() {
    let daemon = start_reasoning_daemon(1).await;
    let session = daemon
        .create(serde_json::json!({"options": {"effort": "max", "thinking": "on"}}))
        .await;
    daemon.turn(&session, "MSG-OPTIONS").await;
    let request = daemon.request_carrying("MSG-OPTIONS");
    assert_eq!(request.effort, Some(Effort::Max));
    assert!(
        matches!(
            request.thinking,
            Some(model::types::ThinkingConfig::Adaptive)
        ),
        "{:?}",
        request.thinking
    );
}

#[tokio::test]
async fn a_sessions_effort_is_kept_through_a_close_and_resume() {
    let daemon = start_reasoning_daemon(1).await;
    let session = daemon.create(serde_json::json!({})).await;
    daemon
        .ok(
            "session.setModel",
            serde_json::json!({"session_id": session, "effort": "xhigh"}),
        )
        .await;
    crate::support::close_and_resume(&daemon.sock, &session).await;
    daemon.turn(&session, "MSG-RESUMED").await;
    assert_eq!(
        daemon.request_carrying("MSG-RESUMED").effort,
        Some(Effort::Xhigh)
    );
}

/// A reply that hands `prompt` to a sub-agent and stops for its answer.
fn agent_call(prompt: &str) -> Vec<StreamEvent> {
    let input = serde_json::json!({"description": "look", "prompt": prompt});
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: "toolu_agent".into(),
                name: "Agent".into(),
                input: serde_json::json!({}),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::ToolUse),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        },
    ]
}

#[tokio::test]
async fn a_sub_agent_runs_on_the_model_its_session_was_switched_to() {
    let daemon = start_daemon(0).await;
    daemon.model.push_turn(agent_call("SUB-PROMPT"));
    daemon.model.push_turn(one_turn());
    daemon.model.push_turn(one_turn());
    let session = daemon.create(serde_json::json!({})).await;
    daemon
        .ok(
            "session.setModel",
            serde_json::json!({"session_id": session, "model": CHOSEN_MODEL}),
        )
        .await;

    daemon.turn(&session, "MSG-DELEGATE").await;
    assert_eq!(daemon.model_asked_for("MSG-DELEGATE"), CHOSEN_MODEL);
    assert_eq!(
        daemon.model_asked_for("SUB-PROMPT"),
        CHOSEN_MODEL,
        "the sub-agent asks for the model its session now runs on"
    );
}

#[cfg(feature = "web")]
#[tokio::test]
async fn a_web_search_runs_on_the_model_its_session_was_switched_to() {
    let daemon = start_daemon(0).await;
    daemon.model.push_turn(crate::support::tool_round(
        "WebSearch",
        "w1",
        serde_json::json!({"query": "SEARCH-QUERY"}),
    ));
    daemon.model.push_turn(one_turn());
    daemon.model.push_turn(one_turn());
    let session = daemon.create(serde_json::json!({})).await;
    daemon
        .ok(
            "session.setModel",
            serde_json::json!({"session_id": session, "model": CHOSEN_MODEL}),
        )
        .await;

    daemon.turn(&session, "MSG-SEARCH").await;
    assert_eq!(daemon.model_asked_for("MSG-SEARCH"), CHOSEN_MODEL);
    assert_eq!(
        daemon.model_asked_for("Perform a web search for the query: SEARCH-QUERY"),
        CHOSEN_MODEL,
        "the search asks the model its session now runs on"
    );
}
