//! Session history, fork and delete over the real socket: what a session leaves on disk and what comes back from it.

use std::time::Duration;

use crate::support::*;
use daemon::rpc::codes;

/// A list a host can group by. Scene and project come from the session's own
/// `Meta` line, so an entry answers "which project, which scene" without the
/// caller asking `session.get` once per row — which was the only way to find
/// out, and impossible for a session that is only on disk.
#[tokio::test]
async fn a_listed_session_says_which_scene_and_project_it_belongs_to() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("answer")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "hello").await;

    let listed = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","id":1}"#,
    )
    .await;
    let entry = listed["result"]["sessions"]
        .as_array()
        .expect("a sessions array")
        .iter()
        .find(|s| s["session_id"] == sid.as_str())
        .unwrap_or_else(|| panic!("the session that just ran is not listed: {listed}"))
        .clone();

    assert_eq!(entry["scene"], "code");
    let project_root = entry["project_root"]
        .as_str()
        .unwrap_or_else(|| panic!("the entry names no project: {entry}"));
    assert!(
        project_root.ends_with("work"),
        "the entry names the project the session was created in, got {project_root}"
    );

    // The same two fields, for the same session, out of the same place.
    let detail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","id":2,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    assert_eq!(detail["result"]["scene"], entry["scene"]);
    assert_eq!(detail["result"]["project_root"], entry["project_root"]);

    srv.stop().await;
}

/// How many times a conversation has been compacted is a by-product of a
/// write that already happened, and it reached nobody: the marker went into
/// the transcript and the count into a field with no writer. Counted as the
/// log is written, so a session that has left memory answers too.
#[tokio::test]
async fn a_listed_session_reports_how_often_it_was_compacted() {
    let (srv, _seen) = start_scripted_server_in(
        "code",
        vec![text_round("answer")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let sid = run_turn(&srv.sock, None, "hello").await;

    // The engine writes this marker when it compacts; here it stands in for
    // one, since making a real turn cross the threshold needs a scene built
    // for it (see `session_event_compact.rs`).
    srv.store
        .append(
            base::session::SessionId::parse(&sid).unwrap(),
            history::entry::LogEntry::Compact {
                before_tokens: 100,
                after_tokens: 40,
                summary_block_id: None,
                replacement_history: None,
                summary: None,
                snip_removed_uuids: None,
            },
        )
        .await
        .expect("the marker lands");

    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","id":1,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;

    let listed = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","id":2}"#,
    )
    .await;
    let entry = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == sid.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("session missing from the list: {listed}"));

    assert_eq!(
        entry["status"], "inactive",
        "the point is the session that is only on disk: {entry}"
    );
    assert_eq!(
        entry["compact_count"], 1,
        "the compaction was counted where a listing can read it: {entry}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_history_returns_the_real_transcript() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("first answer")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let sid = run_turn(&srv.sock, None, "hello there").await;
    let result = history(&srv.sock, &sid).await;

    assert_eq!(result["session_id"], sid);
    assert_eq!(
        result["active"], true,
        "the session is still live: {result}"
    );

    let messages = result["messages"].as_array().expect("messages array");
    assert!(
        messages.len() >= 2,
        "expected at least the user turn and the assistant reply: {result}"
    );
    let rendered = serde_json::to_string(messages).unwrap();
    assert!(
        rendered.contains("hello there"),
        "user turn missing: {rendered}"
    );
    assert!(
        rendered.contains("first answer"),
        "assistant turn missing: {rendered}"
    );
    assert_eq!(result["total"], messages.len());
    assert_eq!(result["has_more"], false);

    srv.stop().await;
}

#[tokio::test]
async fn session_history_respects_its_bound_and_pages() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("reply")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "turn one").await;
    run_turn(&srv.sock, Some(&sid), "turn two").await;
    run_turn(&srv.sock, Some(&sid), "turn three").await;

    // `limit: 0` is the cheap "just tell me how big this is" probe the
    // pagination contract documents.
    let probe = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","limit":0}},"id":2}}"#
        ),
    )
    .await;
    let total = probe["result"]["total"].as_u64().unwrap() as usize;
    assert!(
        total >= 6,
        "three turns should project at least six messages: {probe}"
    );
    assert!(probe["result"]["messages"].as_array().unwrap().is_empty());
    assert_eq!(probe["result"]["has_more"], true);

    // A bounded page really is bounded, and says there's more.
    let page = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","offset":0,"limit":2}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(page["result"]["messages"].as_array().unwrap().len(), 2);
    assert_eq!(page["result"]["total"], total as u64);
    assert_eq!(page["result"]["has_more"], true);

    // The tail, addressed the way the docs say to address it.
    let tail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","offset":{},"limit":2}},"id":4}}"#,
            total - 2
        ),
    )
    .await;
    assert_eq!(tail["result"]["messages"].as_array().unwrap().len(), 2);
    assert_eq!(tail["result"]["has_more"], false);
    assert_ne!(
        tail["result"]["messages"], page["result"]["messages"],
        "the tail page must not be the head page"
    );

    // An over-large limit clamps rather than erroring, and reports what was
    // actually applied.
    let clamped = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","limit":100000}},"id":5}}"#
        ),
    )
    .await;
    assert_eq!(clamped["result"]["limit"], 500);

    srv.stop().await;
}

#[tokio::test]
async fn session_history_errors_cleanly_on_unknown_malformed_and_missing_ids() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    // Well-formed id, never used.
    let unknown = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.history","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(
        unknown["error"]["code"],
        codes::SESSION_NOT_FOUND,
        "{unknown}"
    );

    // Not a BASE58 id at all — a caller mistake, not a missing session.
    let malformed = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.history","params":{"session_id":"not-an-id!!"},"id":2}"#,
    )
    .await;
    assert_eq!(
        malformed["error"]["code"],
        codes::INVALID_PARAMS,
        "{malformed}"
    );

    let missing = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.history","params":{},"id":3}"#,
    )
    .await;
    assert_eq!(missing["error"]["code"], codes::INVALID_PARAMS, "{missing}");

    srv.stop().await;
}

#[tokio::test]
async fn session_history_is_unavailable_without_a_history_store() {
    let (server, sock, _dir, handle) = start_server_without_history().await;
    let v = rpc(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.history","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(v["error"]["code"], codes::INTERNAL_ERROR, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("history_store_wired"),
        "the error should point at the doctor field that explains it: {v}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn session_fork_produces_an_independent_session() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("original answer")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let original = run_turn(&srv.sock, None, "original question").await;

    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}"}},"id":2}}"#
        ),
    )
    .await;
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();
    assert_ne!(fork_id, original, "a fork must get its own id");
    assert_eq!(fork["result"]["parent_session_id"], original);
    assert_eq!(
        fork["result"]["active"], false,
        "a fork is disk-only until resumed"
    );

    let before_original = history(&srv.sock, &original).await;
    let before_fork = history(&srv.sock, &fork_id).await;
    // The fork's lines are new lines — their own ids and times — carrying
    // the same conversation.
    let copied = |h: &serde_json::Value| -> Vec<serde_json::Value> {
        h["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                serde_json::json!({
                    "role": m["role"], "content": m["content"], "origin": m["origin"],
                    "turn_id": m["turn_id"], "usage": m["usage"], "seq": m["seq"]
                })
            })
            .collect()
    };
    assert_eq!(
        copied(&before_original),
        copied(&before_fork),
        "the fork must start as an exact copy"
    );

    // Now write to the fork — a real turn, through the normal resume path.
    run_turn(&srv.sock, Some(&fork_id), "only on the fork").await;

    let after_original = history(&srv.sock, &original).await;
    let after_fork = history(&srv.sock, &fork_id).await;
    assert_eq!(
        after_original["messages"], before_original["messages"],
        "writing to the fork must not touch the original"
    );
    assert!(
        after_fork["total"].as_u64().unwrap() > before_fork["total"].as_u64().unwrap(),
        "the fork should have grown: {after_fork}"
    );
    assert!(serde_json::to_string(&after_fork["messages"])
        .unwrap()
        .contains("only on the fork"));
    assert!(
        !serde_json::to_string(&after_original["messages"])
            .unwrap()
            .contains("only on the fork"),
        "the fork's new turn leaked into the original"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_fork_at_a_message_truncates_and_carries_context_forward() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("answer one")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let original = run_turn(&srv.sock, None, "MARKERONE").await;
    run_turn(&srv.sock, Some(&original), "MARKERTWO").await;

    // Keep only the first exchange.
    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}","at_message":2}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(fork["result"]["forked_at_message"], 2, "{fork}");
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();

    let forked = history(&srv.sock, &fork_id).await;
    let rendered = serde_json::to_string(&forked["messages"]).unwrap();
    assert_eq!(forked["total"], 2, "{forked}");
    assert!(rendered.contains("MARKERONE"));
    assert!(
        !rendered.contains("MARKERTWO"),
        "the branch point was ignored: {rendered}"
    );

    // The truncated history is what the *model* sees on the fork's next
    // turn — that's the point of forking, not just copying a file.
    seen.lock().unwrap().clear();
    run_turn(&srv.sock, Some(&fork_id), "continue").await;
    let sent = {
        let requests = seen.lock().unwrap();
        let last = requests
            .last()
            .expect("the fork's turn must have called the model");
        format!("{:?}", last.messages)
    };
    assert!(
        sent.contains("MARKERONE"),
        "the fork lost its inherited context"
    );
    assert!(
        !sent.contains("MARKERTWO"),
        "the fork inherited turns from past its branch point"
    );

    srv.stop().await;
}

/// A fork of a session's start keeps how the session was set up — its
/// options and the model chosen for it — with none of its turns.
#[tokio::test]
async fn a_fork_at_the_start_keeps_the_sessions_options_and_chosen_model() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("source answer"), text_round("fork answer")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"code","options":{"prompt":{"append":"FORKSPECMARKER"}}},"id":1}"#,
    )
    .await;
    let source = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();
    let chosen = rpc(
        &srv.sock,
        &serde_json::json!({
            "jsonrpc": "2.0", "method": "session.setModel",
            "params": {"session_id": source, "model": "claude-chosen-for-the-source"}, "id": 2,
        })
        .to_string(),
    )
    .await;
    assert!(chosen["result"].is_object(), "{chosen}");
    run_turn(&srv.sock, Some(&source), "MARKERONE").await;

    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{source}","at_message":0}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(fork["result"]["forked_at_message"], 0, "{fork}");
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();
    let detail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","params":{{"session_id":"{fork_id}"}},"id":4}}"#
        ),
    )
    .await;
    assert_eq!(detail["result"]["scene"], "code", "{detail}");
    assert_eq!(history(&srv.sock, &fork_id).await["total"], 0);

    seen.lock().unwrap().clear();
    run_turn(&srv.sock, Some(&fork_id), "first turn of the fork").await;
    let (model, system, messages) = {
        let requests = seen.lock().unwrap();
        // The memory extraction after the turn sends a request of its own,
        // with no system prompt, and may land before or after this read.
        let turn = requests
            .iter()
            .rev()
            .find(|req| !req.system.is_empty())
            .expect("the fork's turn called the model");
        (
            turn.model.clone(),
            format!("{:?}", turn.system),
            format!("{:?}", turn.messages),
        )
    };
    assert_eq!(
        model, "claude-chosen-for-the-source",
        "the fork lost the model chosen for its source"
    );
    assert!(
        system.contains("FORKSPECMARKER"),
        "the fork lost its source's options: {system}"
    );
    assert!(!messages.contains("MARKERONE"), "the fork kept a turn");

    srv.stop().await;
}

#[tokio::test]
async fn session_fork_rejects_unknown_sessions_and_bad_branch_points() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let unknown = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.fork","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(
        unknown["error"]["code"],
        codes::SESSION_NOT_FOUND,
        "{unknown}"
    );

    let sid = run_turn(&srv.sock, None, "hi").await;
    let bad = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{sid}","at_message":"lots"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(bad["error"]["code"], codes::INVALID_PARAMS, "{bad}");

    // Past the end clamps to "the whole thing" rather than erroring — the
    // caller asked for at least this much and there isn't any more.
    let clamped = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{sid}","at_message":9999}},"id":3}}"#
        ),
    )
    .await;
    assert!(clamped["result"].is_object(), "{clamped}");
    assert_eq!(
        clamped["result"]["forked_at_message"], clamped["result"]["source_message_count"],
        "{clamped}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn forked_session_records_its_parent_on_disk() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let original = run_turn(&srv.sock, None, "root").await;

    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}"}},"id":2}}"#
        ),
    )
    .await;
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();

    // Lineage lives in the fork's `Meta` line, which is what makes
    // `HistoryStore::child_sessions` able to find it.
    let children = srv.store.child_sessions(&original).await.unwrap();
    assert_eq!(
        children.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        vec![fork_id],
        "the fork should be discoverable as a child of its source"
    );

    srv.stop().await;
}

#[tokio::test]
async fn closing_a_session_does_not_delete_what_was_forked_from_it() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let original = run_turn(&srv.sock, None, "root").await;

    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}"}},"id":2}}"#
        ),
    )
    .await;
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();

    // A fork records its source in `parent_session_id` so the lineage stays
    // queryable, but it is nobody's sidechain: closing the source must leave
    // it alone, which is the whole point of forking.
    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{original}"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(
        closed["result"]["sidechains_deleted"], 0,
        "a fork is not a sidechain: {closed}"
    );

    let sid = base::session::SessionId::parse(&fork_id).unwrap();
    assert!(
        srv.store.load(sid).await.is_ok(),
        "the fork's transcript must survive its source being closed"
    );

    srv.stop().await;
}

#[tokio::test]
async fn listing_by_parent_reports_each_child_as_what_it_is() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let original = run_turn(&srv.sock, None, "root").await;

    let fork = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.fork","params":{{"session_id":"{original}"}},"id":2}}"#
        ),
    )
    .await;
    let fork_id = fork["result"]["session_id"].as_str().unwrap().to_string();

    let listed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.list","params":{{"parent_session_id":"{original}"}},"id":3}}"#
        ),
    )
    .await;
    let sessions = listed["result"]["sessions"].as_array().unwrap();
    let entry = sessions
        .iter()
        .find(|s| s["session_id"] == fork_id.as_str())
        .unwrap_or_else(|| panic!("the fork should be listed under its source: {listed}"));
    assert_eq!(
        entry["session_kind"], "primary",
        "a fork listed under its source must not be reported as a sidechain: {listed}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_close_cascades_deletes_its_sidechains() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{parent}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(closed["result"]["closed"], parent);
    assert_eq!(closed["result"]["sidechains_deleted"], 1, "{closed}");

    assert!(
        srv.store.load(child).await.is_err(),
        "the sidechain's transcript must be gone after the parent closes"
    );
    assert!(
        srv.store
            .load(base::session::SessionId::parse(&parent).unwrap())
            .await
            .is_ok(),
        "session.close must leave the parent's own transcript on disk"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_delete_removes_the_parent_and_its_sidechains() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let deleted = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.delete","params":{{"session_id":"{parent}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(deleted["result"]["deleted"], true, "{deleted}");
    assert_eq!(deleted["result"]["sidechains_deleted"], 1, "{deleted}");
    assert_eq!(
        deleted["result"]["sidechain_ids"],
        serde_json::json!([child.to_string()])
    );

    assert!(srv.store.load(child).await.is_err());
    assert!(srv
        .store
        .load(base::session::SessionId::parse(&parent).unwrap())
        .await
        .is_err());

    srv.stop().await;
}

#[tokio::test]
async fn session_delete_dry_run_reports_without_deleting_anything() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let previewed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.delete","params":{{"session_id":"{parent}","dry_run":true}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(previewed["result"]["deleted"], false, "{previewed}");
    assert_eq!(previewed["result"]["sidechains_deleted"], 1, "{previewed}");

    assert!(
        srv.store.load(child).await.is_ok(),
        "dry_run must not actually delete the sidechain"
    );
    assert!(srv
        .store
        .load(base::session::SessionId::parse(&parent).unwrap())
        .await
        .is_ok());

    srv.stop().await;
}

/// A session's options are part of what it is, not of the call that
/// happened to build its engine: a reload rebuilds the engine, and the next
/// turn — sent with no options at all — must run under the ones the session
/// was created with. The options are also in the session's log, which is how
/// a daemon restart gets them back.
#[tokio::test]
async fn a_rebuilt_session_keeps_the_options_it_was_created_with() {
    let mut settings = ask_settings();
    settings.permissions.client_may_loosen = true;
    let mut rounds = asking_bash_rounds("first.txt");
    rounds.extend(asking_bash_rounds("second.txt"));
    let (srv, _seen) = start_scripted_server(rounds, settings, Duration::from_secs(5)).await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file","options":{"settings":{"permissions":{"mode":"bypassPermissions"}}}},"id":1}"#,
    )
    .await;
    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(events_of_kind(&frames, "prompt").is_empty());
    let sid = response["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let reloaded = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.reload","id":2}"#,
    )
    .await;
    assert!(reloaded["result"].is_object(), "reload failed: {reloaded}");

    let (frames, response) = rpc_streaming(
        &srv.sock,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session.run_turn",
            "params": {"session_id": sid, "message": "make another"},
            "id": 3,
        })
        .to_string(),
    )
    .await;
    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        events_of_kind(&frames, "prompt").is_empty(),
        "the rebuilt session prompted, so it lost the mode it was created with: {frames:?}"
    );

    let entries = srv
        .store
        .load(base::session::SessionId::parse(&sid).unwrap())
        .await
        .unwrap();
    let recorded = entries.iter().find_map(|env| match &env.entry {
        history::entry::LogEntry::Extension { ns, payload, .. } if ns == "agix.session" => {
            Some(payload.clone())
        }
        _ => None,
    });
    assert_eq!(
        recorded
            .as_ref()
            .map(|p| p["options"]["settings"]["permissions"]["mode"].clone()),
        Some(serde_json::json!("bypassPermissions")),
        "the session's log does not carry its options: {recorded:?}"
    );

    srv.stop().await;
}

/// A step that finished is on disk before the next one starts, so a daemon
/// killed while the model is answering loses only that answer.
#[tokio::test]
async fn a_finished_step_is_on_disk_while_the_next_one_is_still_running() {
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round("Glob", "tool-1", serde_json::json!({"pattern": "*"})),
            // An empty round never ends: the model is "still answering".
            vec![],
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let mut client = crate::support::begin(
        &srv.sock,
        "session.run_turn",
        serde_json::json!({"message": "look around"}),
    )
    .await;
    let sid = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let v = client
                .next_frame()
                .await
                .unwrap()
                .expect("connection closed");
            if v["params"]["event"]["kind"] == "tool_result" {
                return v["params"]["session_id"].as_str().unwrap().to_string();
            }
        }
    })
    .await
    .expect("the tool never ran");

    let on_disk = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let h = history(&srv.sock, &sid).await;
            let rendered = serde_json::to_string(&h["messages"]).unwrap();
            if rendered.contains("tool-1") && rendered.contains("tool_result") {
                return rendered;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the finished step never reached the transcript while the turn ran");
    assert!(on_disk.contains("look around"), "{on_disk}");

    srv.stop().await;
}

#[tokio::test]
async fn clear_reaches_the_transcript_and_what_follows_it_does_too() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("first answer"), text_round("second answer")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let sid = run_turn(&srv.sock, None, "before the clear").await;
    let (_, done) = turn_complete_of(&srv.sock, Some(&sid), "/clear").await;
    assert_eq!(done["stop_reason"], "command_executed");
    run_turn(&srv.sock, Some(&sid), "after the clear").await;

    let rendered = serde_json::to_string(&history(&srv.sock, &sid).await["messages"]).unwrap();
    assert!(!rendered.contains("before the clear"), "{rendered}");
    assert!(rendered.contains("after the clear"), "{rendered}");
    assert!(rendered.contains("second answer"), "{rendered}");

    srv.stop().await;
}
