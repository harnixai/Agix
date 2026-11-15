//! `--max-tokens` is every project's `model.max_tokens` where its settings
//! files do not set one, not only the daemon's own project's.

use std::sync::Arc;

use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};

const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const FALLBACK: u32 = 1234;

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
        model.clone(),
        crate::support::Serving {
            model: DEFAULT_MODEL,
            max_tokens: FALLBACK,
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

async fn max_tokens_asked_in(daemon: &Daemon, project: &std::path::Path, needle: &str) -> u32 {
    let session = daemon
        .ok(
            "session.create",
            serde_json::json!({"project_root": project}),
        )
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": needle}),
        )
        .await;
    for i in 0.. {
        let Some(request) = daemon.model.nth_request(i) else {
            break;
        };
        if serde_json::to_string(&request).unwrap().contains(needle) {
            return request.max_tokens;
        }
    }
    panic!("no request carried `{needle}`")
}

#[tokio::test]
async fn another_project_falls_back_to_the_flag_and_its_own_setting_wins() {
    let daemon = start_daemon().await;

    let plain = tempfile::tempdir().unwrap();
    assert_eq!(
        max_tokens_asked_in(&daemon, plain.path(), "MSG-PLAIN").await,
        FALLBACK,
        "a project whose files set nothing gets the flag's value"
    );

    let own = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(own.path().join(".agix")).unwrap();
    std::fs::write(
        own.path().join(".agix/settings.json"),
        r#"{"model": {"max_tokens": 4321}}"#,
    )
    .unwrap();
    assert_eq!(
        max_tokens_asked_in(&daemon, own.path(), "MSG-OWN").await,
        4321,
        "a project that sets it keeps its own"
    );
}
