//! `session.rewind` and `session.checkpoints` from outside, against a scripted
//! model: what a host sees on disk, in `session.history` and in the next
//! request the model gets after going back a turn — for both halves, each half
//! alone, a file changed behind the session's back, one the model swapped for
//! a symlink, a restore that fails half way, and a session that is busy.
//!
//! The project allows `Write` and `Edit` by rule; nothing runs under
//! `bypassPermissions`.

use std::path::Path;
use std::time::Duration;

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Reply};
use serde_json::{json, Value};

/// Only a turn's requests carry a tool table, so a title or memory call the
/// engine makes on the side cannot take a scripted reply.
const TURN: &str = "\"input_schema\"";

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .settings(json!({
            "permission_rules": [
                { "tool": "Write", "action": "allow" },
                { "tool": "Edit", "action": "allow" }
            ]
        }))
}

fn write(id: &str, path: &Path, content: &str) -> Reply {
    Reply::calls(
        id,
        "Write",
        json!({ "file_path": path.to_string_lossy(), "content": content }),
    )
}

async fn rpc(run: &mut LiveRun, method: &str, params: Value) -> anyhow::Result<Value> {
    let response = run.client.call(method, params).await?;
    if let Some(error) = response.error {
        anyhow::bail!(
            "{method}: {} {} {:?}",
            error.code,
            error.message,
            error.data
        );
    }
    Ok(response.result.unwrap_or_default())
}

/// The error a call is refused with, as `(code, data)`.
async fn refused(run: &mut LiveRun, method: &str, params: Value) -> anyhow::Result<(i64, Value)> {
    let response = run.client.call(method, params).await?;
    let error = response
        .error
        .ok_or_else(|| anyhow::anyhow!("{method} was not refused: {:?}", response.result))?;
    Ok((i64::from(error.code), error.data.unwrap_or_default()))
}

async fn history(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    rpc(
        run,
        "session.history",
        json!({ "session_id": session, "limit": 500 }),
    )
    .await
}

fn last_text(history: &Value) -> String {
    history["messages"]
        .as_array()
        .and_then(|m| m.last())
        .map(|m| m["content"].to_string())
        .unwrap_or_default()
}

fn sha256_hex(bytes: &[u8]) -> String {
    history::checkpoint::sha256_hex(bytes)
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn last_turn_request(stub: &ProviderStub) -> blackbox::SeenRequest {
    stub.calls()
        .into_iter()
        .rfind(|c| c.purpose() == blackbox::Purpose::Turn)
        .expect("a turn reached the model")
}

#[tokio::test(flavor = "multi_thread")]
async fn writing_twice_and_rewinding_to_the_first_turn_puts_files_and_history_back(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("rewind-both", &stub).start().await?;
    let (a, b) = (run.project.join("a.txt"), run.project.join("sub/b.txt"));
    let session = run.session().await?;
    let watch = run.watch(&session, false).await?;

    stub.script_when(TURN, [write("w1", &a, "v1\n"), Reply::text("FIRST-DONE")]);
    run.turn(&session, "first MARK-ONE").await?;
    let after_first = history(&mut run, &session).await?;
    stub.script_when(
        TURN,
        [
            write("w2", &a, "v2\n"),
            write("w3", &b, "b\n"),
            Reply::text("SECOND-DONE"),
        ],
    );
    run.turn(&session, "second MARK-TWO").await?;
    assert_eq!(read(&a).as_deref(), Some("v2\n"));

    let menu = rpc(
        &mut run,
        "session.checkpoints",
        json!({ "session_id": session }),
    )
    .await?;
    assert_eq!(menu["enabled"], true, "{menu}");
    assert_eq!(menu["interrupted_rewind"], Value::Null, "{menu}");
    assert_eq!(menu["turns"][0]["turn_id"], "rewind-both-t1", "{menu}");
    assert!(menu["turns"][0]["prompt"]
        .as_str()
        .unwrap_or_default()
        .contains("MARK-ONE"));
    assert_eq!(
        menu["turns"][1]["files"],
        json!([
            { "path": "a.txt", "tool": "Write", "agent": null },
            { "path": "sub/b.txt", "tool": "Write", "agent": null }
        ]),
        "{menu}"
    );
    assert_eq!(menu["turns"][1]["restorable"], true);

    let archive = run.world.root().join("before-rewind.agix");
    rpc(
        &mut run,
        "session.export",
        json!({ "session_id": session, "path": archive.to_string_lossy(), "format": "dir",
                "include": ["checkpoints"] }),
    )
    .await?;
    let kept = archive.join("checkpoints").join(sha256_hex(b"v1\n"));
    assert_eq!(
        read(&kept).as_deref(),
        Some("v1\n"),
        "the export carries what a.txt was"
    );

    let report = rpc(
        &mut run,
        "session.rewind",
        json!({ "session_id": session, "to_turn": "rewind-both-t1", "scope": "both",
                "fork_first": true }),
    )
    .await?;
    assert_eq!(read(&a).as_deref(), Some("v1\n"));
    let fork = report["fork_session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let forked = history(&mut run, &fork).await?;
    assert!(
        forked["messages"].to_string().contains("MARK-TWO"),
        "{forked}"
    );
    let fork_menu = rpc(
        &mut run,
        "session.checkpoints",
        json!({ "session_id": fork }),
    )
    .await?;
    assert!(
        fork_menu["turns"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["files"] == json!([])),
        "a fork starts with no write to roll back: {fork_menu}"
    );
    assert!(!b.exists(), "a file the rewound turn created is removed");
    assert_eq!(report["dry_run"], false);
    assert_eq!(report["cut"]["to_turn"], "rewind-both-t1");
    let removed = &report["conversation"]["removed_turns"];
    assert_eq!(removed[0]["turn_id"], "rewind-both-t2", "{report}");
    assert!(
        removed[0]["prompt"].to_string().contains("MARK-TWO"),
        "{report}"
    );
    let restored: Vec<(&str, &str)> = report["files"]["restored"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["path"].as_str().unwrap(), f["to"].as_str().unwrap()))
        .collect();
    assert_eq!(restored, [("a.txt", "file"), ("sub/b.txt", "absent")]);

    let rewound = history(&mut run, &session).await?;
    assert_eq!(rewound["total"], after_first["total"]);
    assert_eq!(rewound["messages"], after_first["messages"]);
    assert!(last_text(&rewound).contains("FIRST-DONE"));
    let frame = watch
        .wait_for("a rewound frame", Duration::from_secs(10), |e| {
            e["kind"] == "rewound"
        })
        .await?;
    assert_eq!(frame["event"]["seq"], rewound["total"], "{frame}");
    assert_eq!(frame["event"]["files_restored"], 2, "{frame}");
    assert_eq!(frame["turn_id"], Value::Null, "{frame}");

    stub.script_when(TURN, [Reply::text("THIRD-DONE")]);
    run.turn(&session, "third MARK-THREE").await?;
    let asked = last_turn_request(&stub);
    assert!(asked.messages_contain("MARK-ONE"));
    assert!(
        !asked.messages_contain("MARK-TWO"),
        "the model still sees the rewound turn"
    );
    assert!(!asked.messages_contain("SECOND-DONE"));

    let before_restart = history(&mut run, &session).await?;
    run.restart().await?;
    rpc(&mut run, "session.resume", json!({ "session_id": session })).await?;
    let after_restart = history(&mut run, &session).await?;
    assert_eq!(after_restart["messages"], before_restart["messages"]);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_rewound_alone_leaves_the_conversation_and_the_model_must_read_again(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("rewind-files", &stub).start().await?;
    let notes = run.write_file("notes.txt", "foo\n")?;
    let session = run.session().await?;
    stub.script_when(
        TURN,
        [
            Reply::calls("r1", "Read", json!({ "file_path": notes.to_string_lossy() })),
            Reply::calls(
                "e1",
                "Edit",
                json!({ "file_path": notes.to_string_lossy(), "old_string": "foo", "new_string": "bar" }),
            ),
            Reply::text("EDITED"),
        ],
    );
    run.turn(&session, "edit it").await?;
    assert_eq!(read(&notes).as_deref(), Some("bar\n"));
    let before = history(&mut run, &session).await?;

    let report = rpc(
        &mut run,
        "session.rewind",
        json!({ "session_id": session, "to_start": true, "scope": "files" }),
    )
    .await?;
    assert_eq!(report["conversation"], Value::Null, "{report}");
    assert_eq!(read(&notes).as_deref(), Some("foo\n"));
    let after = history(&mut run, &session).await?;
    assert_eq!(after["messages"], before["messages"]);

    stub.script_when(
        TURN,
        [
            Reply::calls(
                "e2",
                "Edit",
                json!({ "file_path": notes.to_string_lossy(), "old_string": "foo", "new_string": "baz" }),
            ),
            Reply::text("TRIED"),
        ],
    );
    let turn = run.turn(&session, "edit it again").await?;
    assert!(
        last_turn_request(&stub).messages_contain("restored these files"),
        "the model is told its files went back"
    );
    assert!(
        turn.tool_results
            .iter()
            .any(|(name, _, is_error)| name == "Edit" && *is_error),
        "an edit without a fresh read is refused: {:?}",
        turn.tool_results
    );
    assert_eq!(read(&notes).as_deref(), Some("foo\n"));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_changed_behind_the_sessions_back_needs_force_and_a_dry_run_says_so(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("rewind-conflict", &stub).start().await?;
    let a = run.project.join("a.txt");
    let session = run.session().await?;
    let watch = run.watch(&session, false).await?;
    stub.script_when(TURN, [write("w1", &a, "v1\n"), Reply::text("ONE")]);
    run.turn(&session, "one").await?;
    stub.script_when(TURN, [write("w2", &a, "v2\n"), Reply::text("TWO")]);
    run.turn(&session, "two").await?;
    std::fs::write(&a, "by hand\n")?;
    let before = history(&mut run, &session).await?;
    let ask = |extra: Value| {
        let mut params =
            json!({ "session_id": session, "to_turn": "rewind-conflict-t1", "scope": "both" });
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        params
    };

    let (code, data) = refused(&mut run, "session.rewind", ask(json!({}))).await?;
    assert_eq!(code, -32023, "{data}");
    assert_eq!(data["issues"][0]["path"], "a.txt", "{data}");
    assert_eq!(data["issues"][0]["issue"], "modified", "{data}");
    assert_eq!(data["issues"][0]["overridable_by"], "force", "{data}");
    assert_eq!(read(&a).as_deref(), Some("by hand\n"));
    assert_eq!(history(&mut run, &session).await?["total"], before["total"]);

    let plan = rpc(&mut run, "session.rewind", ask(json!({ "dry_run": true }))).await?;
    assert_eq!(plan["dry_run"], true);
    assert_eq!(plan["files"]["skipped"][0]["issue"], "modified", "{plan}");
    assert_eq!(read(&a).as_deref(), Some("by hand\n"));
    assert_eq!(history(&mut run, &session).await?["total"], before["total"]);

    let forced = rpc(&mut run, "session.rewind", ask(json!({ "force": true }))).await?;
    assert_eq!(read(&a).as_deref(), Some("v1\n"));
    assert_eq!(
        forced["files"]["overwrote_external"],
        json!(["a.txt"]),
        "{forced}"
    );
    let frame = watch
        .wait_for("a rewound frame", Duration::from_secs(10), |e| {
            e["kind"] == "rewound"
        })
        .await?;
    assert_eq!(
        frame["event"]["overwrote_external"],
        json!(["a.txt"]),
        "{frame}"
    );
    run.finish().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_path_swapped_for_a_symlink_is_never_written_through() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("rewind-symlink", &stub).start().await?;
    let a = run.project.join("a.txt");
    let outside = run.world.root().join("outside.txt");
    std::fs::write(&outside, "keep me\n")?;
    let session = run.session().await?;
    stub.script_when(TURN, [write("w1", &a, "v1\n"), Reply::text("ONE")]);
    run.turn(&session, "one").await?;
    stub.script_when(TURN, [write("w2", &a, "v2\n"), Reply::text("TWO")]);
    run.turn(&session, "two").await?;
    std::fs::remove_file(&a)?;
    std::os::unix::fs::symlink(&outside, &a)?;
    let params = |extra: Value| {
        let mut p =
            json!({ "session_id": session, "to_turn": "rewind-symlink-t1", "scope": "files" });
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        p
    };

    let (code, data) =
        refused(&mut run, "session.rewind", params(json!({ "force": true }))).await?;
    assert_eq!(code, -32023, "{data}");
    assert_eq!(data["issues"][0]["issue"], "symlink", "{data}");
    assert_eq!(
        data["issues"][0]["overridable_by"], "allow_partial",
        "{data}"
    );

    let report = rpc(
        &mut run,
        "session.rewind",
        params(json!({ "allow_partial": true })),
    )
    .await?;
    assert_eq!(
        report["files"]["skipped"][0]["issue"], "symlink",
        "{report}"
    );
    assert_eq!(report["files"]["restored"], json!([]), "{report}");
    assert_eq!(read(&outside).as_deref(), Some("keep me\n"));
    assert!(std::fs::symlink_metadata(&a)?.file_type().is_symlink());
    run.finish().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_restore_that_fails_half_way_puts_back_what_it_had_done() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("rewind-fails", &stub).start().await?;
    let (a, locked) = (run.project.join("a.txt"), run.project.join("locked"));
    let session = run.session().await?;
    let watch = run.watch(&session, false).await?;
    stub.script_when(TURN, [Reply::text("ONE")]);
    run.turn(&session, "one").await?;
    stub.script_when(
        TURN,
        [
            write("w1", &a, "new a\n"),
            write("w2", &locked.join("b.txt"), "new b\n"),
            Reply::text("TWO"),
        ],
    );
    run.turn(&session, "two").await?;
    let before = history(&mut run, &session).await?;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555))?;

    let outcome = refused(
        &mut run,
        "session.rewind",
        json!({ "session_id": session, "to_turn": "rewind-fails-t1", "scope": "both" }),
    )
    .await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))?;
    let (code, data) = outcome?;
    assert_eq!(code, -32024, "{data}");
    assert_eq!(data["failed"]["path"], "locked/b.txt", "{data}");
    assert_eq!(data["rolled_back"], json!(["a.txt"]), "{data}");
    assert_eq!(data["partial"], false, "{data}");
    assert_eq!(
        read(&a).as_deref(),
        Some("new a\n"),
        "the first file is put back"
    );
    assert_eq!(read(&locked.join("b.txt")).as_deref(), Some("new b\n"));
    assert_eq!(
        history(&mut run, &session).await?["messages"],
        before["messages"]
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        watch.events("rewound").is_empty(),
        "a failed rewind sends no frame"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_session_and_a_request_that_does_not_fit_are_refused() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("rewind-busy", &stub).start().await?;
    let session = run.session().await?;
    stub.script_when(TURN, [Reply::text("ONE")]);
    run.turn(&session, "one").await?;

    let (code, _) = refused(
        &mut run,
        "session.rewind",
        json!({ "session_id": session, "to_tool_use": "toolu_x", "scope": "both" }),
    )
    .await?;
    assert_eq!(code, -32602, "to_tool_use rewinds files only");
    let (code, _) = refused(
        &mut run,
        "session.rewind",
        json!({ "session_id": session, "to_turn": "no-such-turn", "scope": "files" }),
    )
    .await?;
    assert_eq!(code, -32602);
    let (code, _) = refused(
        &mut run,
        "session.rewind",
        json!({ "session_id": session, "to_turn": "rewind-busy-t1", "to_start": true, "scope": "files" }),
    )
    .await?;
    assert_eq!(code, -32602, "exactly one target");

    stub.script_when(
        TURN,
        [Reply::delayed(Duration::from_secs(3), Reply::text("SLOW"))],
    );
    let slow = run.spawn_turn(&session, "slow", json!({})).await?;
    run.wait_turn_state(&session, "running", Duration::from_secs(10))
        .await?;
    let (code, data) = refused(
        &mut run,
        "session.rewind",
        json!({ "session_id": session, "to_turn": "rewind-busy-t1", "scope": "conversation" }),
    )
    .await?;
    assert_eq!(code, -32015, "{data}");
    assert_eq!(data["reason"], "turn_running", "{data}");
    slow.await??;
    run.finish().await;
    Ok(())
}
