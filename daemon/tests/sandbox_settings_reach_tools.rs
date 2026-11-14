//! What `settings.json`'s `sandbox` section says reaches the tools that act
//! on it: the hosts `WebFetch` may reach, and what a sandboxed `Bash`
//! command may not read.

#![cfg(feature = "web")]

use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

fn stop(reason: base::message::StopReason) -> StreamEvent {
    StreamEvent::MessageDelta {
        delta: MessageDeltaPayload {
            stop_reason: Some(reason),
            stop_sequence: None,
        },
        usage: Some(Usage::default()),
    }
}

fn tool_round(id: &str, name: &str, input: Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.into(),
                name: name.into(),
                input: Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        stop(base::message::StopReason::ToolUse),
    ]
}

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
        stop(base::message::StopReason::EndTurn),
    ]
}

async fn call(sock: &std::path::Path, method: &str, params: Value) -> Value {
    let request = json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
    let mut conn = UnixStream::connect(sock).await.unwrap();
    conn.write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let (r, _w) = conn.split();
    let mut reader = BufReader::new(r);
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(120), reader.read_line(&mut line))
            .await
            .expect("the call answered")
            .unwrap();
        assert!(n > 0, "{method}: connection closed before a response");
        let frame: Value = serde_json::from_str(&line).unwrap();
        if frame.get("id").is_some() {
            assert!(frame.get("result").is_some(), "{method} failed: {frame}");
            return frame["result"].clone();
        }
    }
}

/// An HTTP server on 127.0.0.1 answering every request with `body`.
async fn stub_server(body: &'static str) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(reply.as_bytes()).await;
            });
        }
    });
    port
}

/// The `tool_result` blocks the model was sent for `id`.
fn answers(model: &MockAnthropicClient, id: &str) -> Vec<Value> {
    (0..model.calls())
        .filter_map(|n| model.nth_request(n))
        .flat_map(|r| {
            serde_json::to_value(&r).unwrap()["messages"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["type"] == "tool_result" && b["tool_use_id"] == id)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sandbox_section_reaches_the_tools() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let global = dir.join("agix");
    let work = dir.join("work");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let secret = work.join("secret.txt");
    std::fs::write(&secret, "SANDBOX-SECRET-7c2").unwrap();
    std::fs::write(
        global.join("settings.json"),
        json!({
            "permission_rules": [
                {"tool": "WebFetch", "action": "allow"},
                {"tool": "Bash", "action": "allow"},
            ],
            "sandbox": {
                "network_mode": "allowlist",
                "allowed_domains": ["127.0.0.1"],
                "deny_read": [secret],
            },
        })
        .to_string(),
    )
    .unwrap();
    let port = stub_server("STUB-BODY-reached").await;

    let sock = dir.join("agix.sock");
    let paths = StaticDaemonPaths::with_project(global.clone(), work.clone()).with_global(global);
    let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
    config.settings.values.memory_enabled = false;
    let model = Arc::new(MockAnthropicClient::new());
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(work.clone()),
            model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
            transcripts: daemon::Transcripts::Nowhere,
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let listener = DaemonServer::bind_unix(&sock).unwrap();
    tokio::spawn(async move {
        let _ = server.serve_unix_listener(listener).await;
    });

    model.push_turn(tool_round(
        "toolu_listed",
        "WebFetch",
        json!({"url": format!("http://127.0.0.1:{port}/")}),
    ));
    model.push_turn(tool_round(
        "toolu_unlisted",
        "WebFetch",
        json!({"url": format!("http://localhost:{port}/")}),
    ));
    model.push_turn(tool_round(
        "toolu_cat",
        "Bash",
        json!({"command": "cat secret.txt; echo end"}),
    ));
    model.push_turn(text_round("done"));
    call(&sock, "session.run_turn", json!({"message": "fetch"})).await;

    let listed = answers(&model, "toolu_listed")
        .first()
        .cloned()
        .unwrap_or_default();
    assert!(
        listed.to_string().contains("STUB-BODY-reached"),
        "the listed host was not reached: {listed}"
    );
    let unlisted = answers(&model, "toolu_unlisted")
        .first()
        .cloned()
        .unwrap_or_default();
    assert!(
        !unlisted.to_string().contains("STUB-BODY-reached"),
        "a host the allowlist does not name was reached: {unlisted}"
    );
    let cat = answers(&model, "toolu_cat")
        .first()
        .cloned()
        .unwrap_or_default();
    assert!(
        cat.to_string().contains("end"),
        "the command did not run: {cat}"
    );
    if tools::bash::sandbox_status().starts_with("available") {
        assert!(
            !cat.to_string().contains("SANDBOX-SECRET-7c2"),
            "a path `deny_read` names was readable to a sandboxed command: {cat}"
        );
    }
}
