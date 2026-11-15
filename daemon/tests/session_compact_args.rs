//! `session.compact` with `instructions` and `at`, over the real socket: the
//! instructions reach the summary request, and `at` leaves what follows the
//! cut in the history untouched.

use std::time::Duration;

use crate::support::*;
use rpc_client::{CompactAt, DaemonRpcClient};
use serde_json::json;

async fn three_turns() -> (ScriptedServer, SeenRequests, String) {
    let (srv, seen) = start_scripted_server(
        vec![
            text_round("answer one"),
            text_round("answer two"),
            text_round("answer three"),
            text_round("the summary"),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let mut sid = None;
    for (turn, text) in [
        ("t1", "first question"),
        ("t2", "second question"),
        ("t3", "third question"),
    ] {
        let reply = call(
            &srv.sock,
            "session.run_turn",
            json!({ "session_id": sid, "message": text, "turn_id": turn }),
        )
        .await;
        sid = Some(reply["session_id"].as_str().unwrap().to_string());
    }
    let sid = sid.unwrap();
    (srv, seen, sid)
}

fn history_text(history: &serde_json::Value) -> String {
    history["messages"].to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn instructions_reach_the_summary_request() {
    let (srv, seen, sid) = three_turns().await;

    let mut client = DaemonRpcClient::connect(&srv.sock).await.unwrap();
    let compacted = client
        .session_compact(&sid, Some("keep every file path verbatim"), None)
        .await
        .unwrap();
    assert!(compacted.error.is_none(), "{compacted:?}");

    {
        let requests = seen.lock().unwrap();
        let summary_request = serde_json::to_string(requests.last().unwrap()).unwrap();
        assert!(
            summary_request.contains("keep every file path verbatim"),
            "{summary_request}"
        );
    }
    assert!(history_text(&history(&srv.sock, &sid).await).contains("the summary"));
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn at_a_turn_keeps_what_follows_as_it_was() {
    let (srv, seen, sid) = three_turns().await;

    let mut client = DaemonRpcClient::connect(&srv.sock).await.unwrap();
    let compacted = client
        .session_compact(&sid, None, Some(CompactAt::Turn("t1")))
        .await
        .unwrap();
    assert!(compacted.error.is_none(), "{compacted:?}");

    let summarized = serde_json::to_string(seen.lock().unwrap().last().unwrap()).unwrap();
    assert!(summarized.contains("first question"), "{summarized}");
    assert!(!summarized.contains("second question"), "{summarized}");

    let after = history_text(&history(&srv.sock, &sid).await);
    assert!(after.contains("the summary"), "{after}");
    assert!(!after.contains("first question"), "{after}");
    for kept in [
        "second question",
        "answer two",
        "third question",
        "answer three",
    ] {
        assert!(after.contains(kept), "{kept} missing from {after}");
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_point_changes_nothing_and_says_so() {
    let (srv, _seen, sid) = three_turns().await;
    let before = history_text(&history(&srv.sock, &sid).await);

    let (frames, response) = rpc_streaming(
        &srv.sock,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "session.compact",
                 "params": { "session_id": sid, "at": { "to_turn": "nope" } } })
        .to_string(),
    )
    .await;
    assert!(response["result"].is_object(), "{response}");
    assert!(
        frames
            .iter()
            .any(|f| f.to_string().contains("Not compacted")),
        "{frames:?}"
    );
    assert_eq!(history_text(&history(&srv.sock, &sid).await), before);
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn at_needs_exactly_one_point() {
    let (srv, _seen, sid) = three_turns().await;
    let frame = call_frame(
        &srv.sock,
        "session.compact",
        json!({ "session_id": sid, "at": { "to_turn": "t1", "to_tool_use": "x" } }),
    )
    .await;
    assert!(frame.get("error").is_some(), "{frame}");
    srv.stop().await;
}
