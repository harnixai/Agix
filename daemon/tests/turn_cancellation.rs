//! `session.interrupt` stops a turn whose tool is still running: the turn
//! ends as cancelled, an open question is withdrawn, and the session takes
//! the next turn.

use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

fn text_round(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::TextDelta { text: text.into() },
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

fn tool_round(tool: &str, id: &str, input: serde_json::Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: tool.to_string(),
                input: serde_json::Value::Null,
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

struct Daemon {
    sock: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl Daemon {
    /// Questions wait forever: only the interrupt can end this turn.
    async fn start(model: Arc<MockAnthropicClient>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("cancel.sock");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut config = load_daemon_config(
            "claude-sonnet-4-6",
            2000,
            Some(&sock),
            "code",
            &StaticDaemonPaths::new(dir.path().to_path_buf()),
        );
        config.settings.values.memory_enabled = false;
        config
            .settings
            .values
            .execution
            .permission_prompt_timeout_secs = 0;
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(cwd),
                model_client: Some(model),
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");
        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        let bind = sock.clone();
        tokio::spawn(async move {
            let _ = server.serve_unix(&bind).await;
        });
        for _ in 0..80 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(sock.exists(), "socket never bound");
        Self { sock, _dir: dir }
    }

    async fn call(&self, request: serde_json::Value) -> serde_json::Value {
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let (r, _w) = conn.split();
        let mut reader = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            assert!(n > 0, "connection closed before a response: {request}");
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                return frame;
            }
        }
    }
}

#[tokio::test]
async fn an_interrupt_withdraws_an_open_question_and_ends_the_turn_as_cancelled() {
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(tool_round(
        "AskUserQuestion",
        "ask-1",
        serde_json::json!({"question": "which branch?", "options": [{"key": "a", "label": "main"}]}),
    ));
    model.push_turn(text_round("never reached"));
    model.push_turn(text_round("next turn"));
    let daemon = Daemon::start(model).await;

    let mut conn = UnixStream::connect(&daemon.sock).await.unwrap();
    let request = serde_json::json!({
        "jsonrpc": "2.0", "method": "session.run_turn", "id": 1,
        "params": {"message": "ask me", "options": {"prompt_types": ["permission", "question"]}},
    });
    conn.write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let (r, _w) = conn.split();
    let mut reader = BufReader::new(r);
    let mut frames = Vec::new();
    let mut session = None;
    let response = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            assert!(n > 0, "connection closed before the turn's response");
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                return frame;
            }
            let event = &frame["params"]["event"];
            if event["kind"] == "prompt" && event["prompt_type"] == "question" && session.is_none()
            {
                let sid = frame["params"]["session_id"].as_str().unwrap().to_string();
                let interrupted = daemon
                    .call(serde_json::json!({
                        "jsonrpc": "2.0", "method": "session.interrupt", "id": 2,
                        "params": {"session_id": sid},
                    }))
                    .await;
                assert_eq!(interrupted["result"]["interrupted"], true, "{interrupted}");
                session = Some(sid);
            }
            frames.push(event.clone());
        }
    })
    .await
    .expect("the interrupted turn never ended");
    assert!(response["result"].is_object(), "{response}");
    let session = session.expect("the question was put to the host");

    let of_kind = |kind: &str| -> Vec<&serde_json::Value> {
        frames.iter().filter(|e| e["kind"] == kind).collect()
    };
    let resolved = of_kind("prompt_resolved");
    assert_eq!(resolved.len(), 1, "{frames:?}");
    assert_eq!(resolved[0]["prompt_id"], "ask-1", "{frames:?}");
    assert_eq!(resolved[0]["resolution"], "withdrawn", "{frames:?}");
    assert_eq!(resolved[0]["reason"], "cancelled", "{frames:?}");
    let completed = of_kind("turn_complete");
    assert_eq!(completed.len(), 1, "{frames:?}");
    assert_eq!(completed[0]["stop_reason"], "cancelled", "{frames:?}");

    let next = tokio::time::timeout(
        Duration::from_secs(20),
        daemon.call(serde_json::json!({
            "jsonrpc": "2.0", "method": "session.run_turn", "id": 3,
            "params": {"session_id": session, "message": "again"},
        })),
    )
    .await
    .expect("the next turn never ended");
    assert!(
        next["result"].is_object(),
        "the session is still busy: {next}"
    );
}
