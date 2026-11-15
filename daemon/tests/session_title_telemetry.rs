//! The model call that names a session is accounted like every other one: the
//! session's telemetry file holds one `api_request` for it, tagged `title`.

use std::sync::Arc;
use std::time::Duration;

use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};

const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

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
}

async fn start_daemon() -> Daemon {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(one_turn());
    model.push_turn(one_turn());
    let served = crate::support::serve_permitting_all(
        model,
        crate::support::Serving {
            scene: "harnix",
            model: DEFAULT_MODEL,
            ..Default::default()
        },
        |_| {},
    )
    .await;
    Daemon {
        sock: served.sock,
        _dir: served.dir,
        _handle: served.handle,
    }
}

fn api_requests(file: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == "api_request")
        .collect()
}

#[tokio::test]
async fn the_naming_call_is_one_api_request_in_the_session_telemetry() {
    let daemon = start_daemon().await;
    let telemetry = daemon._dir.path().join("telemetry.jsonl");
    let session = daemon
        .ok(
            "session.create",
            serde_json::json!({"options": {"telemetry": {"output": telemetry}}}),
        )
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": "把发布流程整理成一份清单"}),
        )
        .await;

    let mut titled = Vec::new();
    for _ in 0..200 {
        titled = api_requests(&telemetry)
            .into_iter()
            .filter(|v| v["purpose"] == "title")
            .collect();
        if !titled.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        titled.len(),
        1,
        "the naming call should be reported once: {:?}",
        api_requests(&telemetry)
    );
}
