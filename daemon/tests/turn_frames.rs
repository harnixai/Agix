//! What a turn tells its watchers: frames, bracketing, usage, thinking, structured output, and their agreement with the history.

use std::time::Duration;

use crate::support::*;
use daemon::rpc::codes;
use model::stream::{BlockDelta, ContentBlockStart, StreamEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// The `turn_complete` frame carries what the whole turn spent.
///
/// This is the number a daemon client budgets on — the engine's `TurnOutcome`
/// never crosses the socket. The frame is built by hand-copying two fields out
/// of the event, which is exactly the shape of code that forwards the wrong
/// one — the last call's usage, or a zero for a cancelled turn.
///
/// Two calls with different costs, because equal ones would let a sum and a
/// last value pass the same assertion.
#[tokio::test]
async fn the_turn_complete_frame_reports_the_whole_turn() {
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round_costing("Read", "t1", serde_json::json!({"file_path": "x"}), 100, 20),
            text_round_costing("done", 7, 3),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let (frames, resp) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"read it"},"id":1}"#,
    )
    .await;
    assert!(resp["result"].is_object(), "turn failed: {resp}");

    let complete = frames
        .iter()
        .filter_map(|f| f.get("params").and_then(|p| p.get("event")))
        .find(|e| e.get("kind").and_then(|k| k.as_str()) == Some("turn_complete"))
        .unwrap_or_else(|| panic!("no turn_complete frame in {frames:#?}"));

    assert_eq!(
        complete["api_calls"], 2,
        "the case needs a turn of more than one call to mean anything: {complete}"
    );
    assert_eq!(
        complete["usage"]["input_tokens"], 107,
        "input tokens are the last call's, not the turn's: {complete}"
    );
    assert_eq!(
        complete["usage"]["output_tokens"], 23,
        "output tokens are the last call's, not the turn's: {complete}"
    );

    srv.stop().await;
}

/// `turn_complete` is a turn's last frame and only the `run_turn` response
/// follows it (protocol §5.5) — for the connection that started the turn and
/// for one that only watches. A watcher's frames and its own responses share
/// one queue, so a request it sends once the turn has answered is answered
/// after that turn's `turn_complete`.
#[tokio::test]
async fn every_watcher_has_turn_complete_before_the_turns_response() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("done")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":1,"params":{}}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();

    let (read, mut write) = UnixStream::connect(&srv.sock).await.unwrap().into_split();
    let mut watcher = BufReader::new(read).lines();
    let subscribe = serde_json::json!({
        "jsonrpc": "2.0", "method": "session.subscribe", "id": 1,
        "params": {"session_id": sid},
    });
    write
        .write_all(format!("{subscribe}\n").as_bytes())
        .await
        .unwrap();
    let (_, subscribed) = until_response(&mut watcher, 1).await;
    assert!(subscribed["result"].is_object(), "{subscribed}");

    let run = serde_json::json!({
        "jsonrpc": "2.0", "method": "session.run_turn", "id": 1,
        "params": {"session_id": sid, "message": "hi", "turn_id": "t-order"},
    });
    let (frames, response) = rpc_streaming(&srv.sock, &run.to_string()).await;
    assert!(response["result"].is_object(), "turn failed: {response}");
    let last = frames
        .last()
        .unwrap_or_else(|| panic!("no frames before {response}"));
    assert_eq!(
        last["params"]["event"]["kind"], "turn_complete",
        "{frames:#?}"
    );
    assert_eq!(last["params"]["turn_id"], "t-order", "{last}");

    let ping = serde_json::json!({"jsonrpc": "2.0", "method": "daemon.ping", "id": 2});
    write
        .write_all(format!("{ping}\n").as_bytes())
        .await
        .unwrap();
    let (seen, pong) = until_response(&mut watcher, 2).await;
    assert_eq!(pong["result"]["pong"], true, "{pong}");
    let completes: Vec<&serde_json::Value> = seen
        .iter()
        .filter(|f| f["params"]["event"]["kind"] == "turn_complete")
        .collect();
    assert_eq!(
        completes.len(),
        1,
        "the watcher's turn_complete came after the turn had answered: {seen:#?}"
    );
    assert_eq!(completes[0]["params"]["turn_id"], "t-order");

    srv.stop().await;
}

#[tokio::test]
async fn thinking_reaches_the_client_as_its_own_frame() {
    let mut round = vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Thinking {
                thinking: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::ThinkingDelta {
                thinking: "let me see".into(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::SignatureDelta {
                signature: "sig".into(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
    ];
    for event in text_round("answer") {
        round.push(match event {
            StreamEvent::ContentBlockStart { content_block, .. } => {
                StreamEvent::ContentBlockStart {
                    index: 1,
                    content_block,
                }
            }
            StreamEvent::ContentBlockDelta { delta, .. } => {
                StreamEvent::ContentBlockDelta { index: 1, delta }
            }
            StreamEvent::ContentBlockStop { .. } => StreamEvent::ContentBlockStop { index: 1 },
            other => other,
        });
    }
    let (srv, _seen) =
        start_scripted_server(vec![round], ask_settings_no_memory(), Duration::ZERO).await;

    let (frames, resp) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"think"},"id":1}"#,
    )
    .await;
    assert!(resp["result"].is_object(), "turn failed: {resp}");
    let thinking = events_of_kind(&frames, "thinking_delta");
    assert_eq!(thinking.len(), 1, "{frames:?}");
    assert_eq!(thinking[0]["text"], "let me see");
    let text: String = events_of_kind(&frames, "text_delta")
        .iter()
        .map(|f| f["text"].as_str().unwrap())
        .collect();
    assert_eq!(text, "answer");

    srv.stop().await;
}

/// A tab watching a session must learn that a turn it did not start has
/// failed; only the caller gets the error response.
#[tokio::test]
async fn a_failed_turn_tells_every_watcher_it_is_over() {
    let failing = vec![StreamEvent::Error {
        error: model::stream::ApiErrorPayload {
            kind: "invalid_request_error".into(),
            message: "boom".into(),
        },
    }];
    let (srv, _seen) = start_scripted_server(
        vec![text_round("first"), failing],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let sid = run_turn(&srv.sock, None, "hello").await;

    let mut watcher = UnixStream::connect(&srv.sock).await.unwrap();
    watcher
        .write_all(
            format!(
                r#"{{"jsonrpc":"2.0","method":"session.subscribe","id":7,"params":{{"session_id":"{sid}"}}}}"#
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    watcher.write_all(b"\n").await.unwrap();
    let (r, _w) = watcher.split();
    let mut watched = BufReader::new(r);
    let mut line = String::new();
    watched.read_line(&mut line).await.unwrap();
    let subscribed: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(subscribed["result"].is_object(), "{subscribed}");

    let (frames, resp) = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"again"}},"id":1}}"#
        ),
    )
    .await;
    assert!(resp["error"].is_object(), "the turn should fail: {resp}");
    assert!(
        events_of_kind(&frames, "error").is_empty(),
        "the caller is told by its response, not twice: {frames:?}"
    );

    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert!(
                watched.read_line(&mut line).await.unwrap() > 0,
                "watcher closed"
            );
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["params"]["event"]["kind"] == "error" {
                return v;
            }
        }
    })
    .await
    .expect("the watcher never learned the turn had ended");
    assert_eq!(ended["params"]["session_id"], sid.as_str());
    let event = &ended["params"]["event"];
    assert_eq!(
        event["message"], resp["error"]["message"],
        "the watcher reads what the caller reads: {event} / {resp}"
    );
    assert!(
        event["message"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{}: ", event["code"].as_str().unwrap())),
        "{event}"
    );
    assert_eq!(event["persisted"], true, "{event}");

    srv.stop().await;
}

#[tokio::test]
async fn a_turn_reports_how_full_the_context_is_after_its_last_call() {
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round_costing("Glob", "tool-1", serde_json::json!({"pattern": "*"}), 50, 5),
            text_round_costing("done", 120, 7),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let (sid, done) = turn_complete_of(&srv.sock, None, "look").await;
    assert_eq!(done["usage"]["input_tokens"], 170, "{done}");
    assert_eq!(done["last_call_usage"]["input_tokens"], 120, "{done}");
    assert_eq!(done["context"]["tokens"], 120, "{done}");
    assert!(done["context"]["window"].as_u64().unwrap() > 0, "{done}");
    assert!(done["context"]["compact_threshold"].is_u64(), "{done}");

    let detail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","id":2,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    assert_eq!(detail["result"]["context"], done["context"], "{detail}");

    srv.stop().await;
}

#[tokio::test]
async fn structured_output_is_checked_against_the_host_schema_and_returned_on_turn_complete() {
    let (srv, seen) = start_scripted_server(
        vec![
            tool_round("StructuredOutput", "so-1", serde_json::json!({"wrong": 1})),
            tool_round(
                "StructuredOutput",
                "so-2",
                serde_json::json!({"answer": "42"}),
            ),
            text_round("done"),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"],
        "additionalProperties": false
    });
    let (frames, response) = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"message":"answer in the format","options":{{"output_schema":{schema}}}}},"id":1}}"#
        ),
    )
    .await;
    assert!(response["result"].is_object(), "{response}");

    let results = events_of_kind(&frames, "tool_result");
    assert_eq!(results.len(), 2, "{frames:?}");
    assert_eq!(results[0]["is_error"], true, "{results:?}");
    assert!(
        results[0]["content"]
            .as_str()
            .unwrap()
            .contains("does not match the required schema"),
        "{results:?}"
    );
    assert_eq!(results[1]["is_error"], false, "{results:?}");

    let done = events_of_kind(&frames, "turn_complete");
    assert_eq!(
        done[0]["structured_output"],
        serde_json::json!({"answer": "42"}),
        "{done:?}"
    );

    let first = serde_json::to_value(&seen.lock().unwrap()[0]).unwrap();
    let tool = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "StructuredOutput")
        .expect("announced up front, not left for ToolSearch")
        .clone();
    assert_eq!(tool["input_schema"], schema, "{tool}");

    srv.stop().await;
}

#[tokio::test]
async fn session_compact_compacts_a_short_conversation_on_request() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round(&"a long answer ".repeat(40))],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let sid = run_turn(&srv.sock, None, "first").await;
    for i in 0..5 {
        run_turn(&srv.sock, Some(&sid), &format!("more {i}")).await;
    }

    let (frames, response) = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.compact","params":{{"session_id":"{sid}"}},"id":7}}"#
        ),
    )
    .await;
    assert!(response["result"].is_object(), "{response}");
    let compact = events_of_kind(&frames, "compact");
    assert_eq!(compact.len(), 1, "{frames:?}");
    let before = compact[0]["tokens_before"].as_u64().unwrap();
    let after = compact[0]["tokens_after"].as_u64().unwrap();
    assert!(after < before, "{compact:?}");
    assert!(
        compact[0]["messages_after"].as_u64() < compact[0]["messages_before"].as_u64(),
        "{compact:?}"
    );
    let done = events_of_kind(&frames, "turn_complete");
    assert_eq!(done[0]["stop_reason"], "command_executed", "{done:?}");

    let (frames, response) = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.clear","params":{{"session_id":"{sid}"}},"id":8}}"#
        ),
    )
    .await;
    assert!(response["result"].is_object(), "{response}");
    assert_eq!(
        events_of_kind(&frames, "turn_complete")[0]["stop_reason"],
        "command_executed"
    );

    let missing = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.compact","params":{},"id":9}"#,
    )
    .await;
    assert_eq!(missing["error"]["code"], codes::INVALID_PARAMS, "{missing}");

    srv.stop().await;
}

/// A client can draw a turn without guessing where it begins and where one
/// model call ends: `turn_start` comes first, every call is bracketed by
/// `step_start` / `step_end`, and the tool frames of a call fall inside it.
#[tokio::test]
async fn a_turn_and_each_of_its_model_calls_are_bracketed_by_frames() {
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round_costing("Glob", "tool-1", serde_json::json!({"pattern": "*"}), 7, 3),
            text_round_costing("done", 11, 5),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let (frames, resp) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"look"},"id":1}"#,
    )
    .await;
    assert!(resp["result"].is_object(), "turn failed: {resp}");

    let kinds: Vec<String> = frames
        .iter()
        .map(|f| {
            f["params"]["event"]["kind"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .filter(|k| {
            matches!(
                k.as_str(),
                "turn_start"
                    | "step_start"
                    | "step_end"
                    | "tool_use"
                    | "tool_result"
                    | "text_delta"
                    | "turn_complete"
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "turn_start",
            "step_start",
            "tool_use",
            "tool_result",
            "step_end",
            "step_start",
            "text_delta",
            "step_end",
            "turn_complete"
        ],
        "{frames:#?}"
    );

    let ends = events_of_kind(&frames, "step_end");
    assert_eq!(ends[0]["step"], 0);
    assert_eq!(ends[0]["stop_reason"], "tool_use");
    assert_eq!(ends[0]["tool_calls"], 1);
    assert_eq!(ends[0]["usage"]["input_tokens"], 7);
    assert_eq!(ends[1]["step"], 1);
    assert_eq!(ends[1]["usage"]["output_tokens"], 5);
    let turn_ids: std::collections::HashSet<&str> = frames
        .iter()
        .filter_map(|f| f["params"]["turn_id"].as_str())
        .collect();
    assert_eq!(turn_ids.len(), 1, "every frame belongs to the one turn");

    srv.stop().await;
}

/// A tool row can be drawn while the model is still writing the arguments:
/// `tool_use_start` names the call first, the argument fragments follow, and
/// they concatenate to exactly the input `tool_use` then carries.
#[tokio::test]
async fn a_tool_call_is_announced_before_its_arguments_are_complete() {
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round("Glob", "tool-7", serde_json::json!({"pattern": "*.rs"})),
            text_round("done"),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let (frames, resp) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"find"},"id":1}"#,
    )
    .await;
    assert!(resp["result"].is_object(), "turn failed: {resp}");

    let kinds: Vec<&str> = frames
        .iter()
        .filter_map(|f| f["params"]["event"]["kind"].as_str())
        .filter(|k| k.starts_with("tool_"))
        .collect();
    assert_eq!(
        kinds,
        [
            "tool_use_start",
            "tool_args_delta",
            "tool_use",
            "tool_result"
        ],
        "{frames:#?}"
    );
    let start = events_of_kind(&frames, "tool_use_start")[0];
    assert_eq!(start["id"], "tool-7");
    assert_eq!(start["name"], "Glob");
    let args: String = events_of_kind(&frames, "tool_args_delta")
        .iter()
        .map(|f| f["partial_json"].as_str().unwrap())
        .collect();
    let used = events_of_kind(&frames, "tool_use")[0];
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&args).unwrap(),
        used["input"]
    );

    srv.stop().await;
}

/// What a client needs from a turn, read off the live frames.
fn reduce_frames(frames: &[serde_json::Value]) -> serde_json::Value {
    let text: String = events_of_kind(frames, "text_delta")
        .iter()
        .map(|f| f["text"].as_str().unwrap())
        .collect();
    let calls: Vec<&serde_json::Value> = events_of_kind(frames, "tool_use")
        .iter()
        .map(|f| &f["id"])
        .collect();
    let results: Vec<&serde_json::Value> = events_of_kind(frames, "tool_result")
        .iter()
        .map(|f| &f["id"])
        .collect();
    serde_json::json!({"text": text, "calls": calls, "results": results})
}

/// The same, read off `session.history` for one turn.
fn reduce_history(messages: &[serde_json::Value], turn_id: &str) -> serde_json::Value {
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for m in messages.iter().filter(|m| m["turn_id"] == turn_id) {
        for block in m["content"].as_array().unwrap() {
            match block["type"].as_str().unwrap() {
                "text" if m["role"] == "assistant" => {
                    text.push_str(block["text"].as_str().unwrap())
                }
                "tool_use" => calls.push(block["id"].clone()),
                "tool_result" => results.push(block["tool_use_id"].clone()),
                _ => {}
            }
        }
    }
    serde_json::json!({"text": text, "calls": calls, "results": results})
}

/// A tab that watched a turn live and a tab that opened it afterwards from
/// `session.history` must end up showing the same thing — and the positions
/// the live frames report are positions in that history.
#[tokio::test]
async fn the_live_frames_and_the_history_of_a_turn_agree() {
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round_costing("Glob", "tool-1", serde_json::json!({"pattern": "*"}), 7, 3),
            text_round_costing("all done", 11, 5),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let (frames, resp) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"look"},"id":1}"#,
    )
    .await;
    assert!(resp["result"].is_object(), "turn failed: {resp}");
    let sid = resp["result"]["session_id"].as_str().unwrap().to_string();
    let turn_id = frames[0]["params"]["turn_id"].as_str().unwrap().to_string();

    let h = history(&srv.sock, &sid).await;
    let messages = h["messages"].as_array().unwrap();
    assert_eq!(reduce_frames(&frames), reduce_history(messages, &turn_id));

    let origins: Vec<&str> = messages
        .iter()
        .map(|m| m["origin"].as_str().unwrap())
        .collect();
    assert_eq!(
        origins,
        ["injected", "user", "assistant", "tool_result", "assistant"]
    );
    assert_eq!(messages[2]["usage"]["input_tokens"], 7);
    assert_eq!(messages[4]["usage"]["output_tokens"], 5);
    for (i, m) in messages.iter().enumerate() {
        assert_eq!(m["seq"], i);
        assert!(m["id"].is_string() && m["ts"].is_string(), "{m}");
    }

    let ends = events_of_kind(&frames, "step_end");
    assert_eq!(
        ends[0]["seq"], 4,
        "context, user, reply, results are on disk"
    );
    let done = events_of_kind(&frames, "turn_complete")[0];
    assert_eq!(done["seq"], h["total"]);

    let sub = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.subscribe","params":{{"session_id":"{sid}"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(sub["result"]["last_seq"], h["total"], "{sub}");

    srv.stop().await;
}
