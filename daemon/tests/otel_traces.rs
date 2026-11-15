#![cfg(feature = "otel")]
//! A turn that calls a tool reaches an OTLP collector as spans: the turn, the
//! model calls and the tool call, with the GenAI attributes and none of the
//! conversation's text.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

fn round(
    block: ContentBlockStart,
    delta: BlockDelta,
    stop: base::message::StopReason,
) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: block,
        },
        StreamEvent::ContentBlockDelta { index: 0, delta },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(stop),
                stop_sequence: None,
            },
            usage: Some(Usage {
                input_tokens: 17,
                output_tokens: 5,
                ..Default::default()
            }),
        },
    ]
}

type Bodies = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

async fn collector() -> (String, Bodies) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let bodies: Bodies = Arc::default();
    let seen = bodies.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let head_end = loop {
                    if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break at + 4;
                    }
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let length: usize = head
                    .to_lowercase()
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").map(str::to_string))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + length {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                seen.lock().unwrap().push((path, buf[head_end..].to_vec()));
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    (endpoint, bodies)
}

async fn call(
    sock: &std::path::Path,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    crate::support::call(sock, method, params).await
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_with_a_tool_call_reaches_the_collector_as_spans() {
    let (endpoint, bodies) = collector().await;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("otel.sock");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(round(
        ContentBlockStart::ToolUse {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input: serde_json::Value::Null,
        },
        BlockDelta::InputJsonDelta {
            partial_json: serde_json::json!({"command": "echo traced"}).to_string(),
        },
        base::message::StopReason::ToolUse,
    ));
    model.push_turn(round(
        ContentBlockStart::Text {
            text: String::new(),
        },
        BlockDelta::TextDelta {
            text: "all done".into(),
        },
        base::message::StopReason::EndTurn,
    ));
    model.push_turn(round(
        ContentBlockStart::Text {
            text: String::new(),
        },
        BlockDelta::TextDelta {
            text: "title".into(),
        },
        base::message::StopReason::EndTurn,
    ));

    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.settings.values.memory_enabled = false;
    config.settings.values.permission_mode = base::permission::PermissionMode::BypassPermissions;
    config.settings.values.telemetry_url = Some(endpoint);
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

    let session = call(&sock, "session.create", serde_json::json!({})).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    call(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": session, "message": "SECRET-PROMPT-WORDS"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::task::spawn_blocking(assembly::telemetry_otel::shutdown_process_exporter)
        .await
        .unwrap();

    let traces: Vec<Vec<u8>> = bodies
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path == "/v1/traces")
        .map(|(_, body)| body.clone())
        .collect();
    assert!(!traces.is_empty(), "no trace reached the collector");
    let all: Vec<u8> = traces.concat();
    for expected in [
        "execute_tool Bash",
        "chat ",
        "invoke_agent ",
        "gen_ai.usage.input_tokens",
        "gen_ai.conversation.id",
    ] {
        assert!(
            contains(&all, expected),
            "{expected} is not in what was exported"
        );
    }
    assert!(!contains(&all, "SECRET-PROMPT-WORDS"));
}
