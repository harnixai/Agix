//! A sub-agent cannot put a session into plan mode: plan mode is the
//! conversation's, entered by the model the user is talking to.

#![cfg(feature = "agents")]

use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_calling_enter_plan_mode_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("agix");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let sock = dir.path().join("agix.sock");
    let paths = StaticDaemonPaths::with_project(global.clone(), work.clone()).with_global(global);
    let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
    config.settings.values.memory_enabled = false;
    let model = Arc::new(MockAnthropicClient::new());
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(work),
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
        "toolu_agent",
        "Agent",
        json!({"description": "plan it", "prompt": "plan it",
               "subagent_type": "general-purpose"}),
    ));
    model.push_turn(tool_round("toolu_plan", "EnterPlanMode", json!({})));
    model.push_turn(text_round("planned"));
    model.push_turn(text_round("done"));
    call(&sock, "session.run_turn", json!({"message": "delegate"})).await;

    let answered: Vec<Value> = (0..model.calls())
        .filter_map(|n| model.nth_request(n))
        .flat_map(|r| {
            serde_json::to_value(&r).unwrap()["messages"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["type"] == "tool_result" && b["tool_use_id"] == "toolu_plan")
        .collect();
    assert!(
        !answered.is_empty(),
        "the sub-agent's call was never answered"
    );
    let answer = answered[0].to_string();
    assert!(
        answered[0]["is_error"] == true && answer.contains("sub-agent"),
        "a sub-agent entered plan mode: {answer}"
    );
}
