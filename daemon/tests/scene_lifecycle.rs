//! Scene list, activate, deactivate and describe, and sessions recorded under a scene the daemon no longer serves.

use std::time::Duration;

use crate::support::*;
use daemon::rpc::codes;

/// A session recorded under a scene other than this daemon's must
/// be rejected, not silently resumed into the wrong scene.
#[tokio::test]
async fn session_resume_rejects_a_scene_mismatch() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let sid = inject_session_in_scene(&srv, "harnix").await;

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(resumed["error"]["code"], codes::SCENE_MISMATCH, "{resumed}");
    assert_eq!(resumed["error"]["data"]["session_id"], sid.to_string());
    assert_eq!(resumed["error"]["data"]["recorded_scene"], "harnix");
    assert_eq!(resumed["error"]["data"]["requested_scene"], "code");

    srv.stop().await;
}

/// `docs/daemon_rpc_protocol.md` §3.4 binds every method that names a session. A guard is added per method and the newest one is the
/// easiest to forget — `session.rename` in particular reads like metadata
/// rather than like reaching into a session, and naming a conversation that
/// belongs to another scene is how it would end up labelled in a list it does
/// not belong to.
#[tokio::test]
async fn the_newer_methods_reject_a_scene_mismatch_too() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;
    let sid = inject_session_in_scene(&srv, "harnix").await;

    let calls = [
        (
            "session.rename",
            format!(r#"{{"session_id":"{sid}","name":"别的场景的会话"}}"#),
        ),
        (
            "session.setModel",
            format!(r#"{{"session_id":"{sid}","model":"claude-opus-4-6"}}"#),
        ),
        (
            "session.subscribe",
            format!(r#"{{"session_id":"{sid}","resume":true}}"#),
        ),
    ];

    for (method, params) in calls {
        let response = rpc(
            &srv.sock,
            &format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":{params},"id":1}}"#),
        )
        .await;
        assert_eq!(
            response["error"]["code"],
            codes::SCENE_MISMATCH,
            "{method} did not refuse a session from another scene: {response}"
        );
        assert_eq!(
            response["error"]["data"]["recorded_scene"], "harnix",
            "{method}"
        );
        assert_eq!(
            response["error"]["data"]["requested_scene"], "code",
            "{method}"
        );
    }

    srv.stop().await;
}

/// `session.close`/`.delete` apply the same scene-isolation check as
/// `session.resume`/`.fork`: a session recorded under a scene this daemon
/// isn't serving cannot be closed or deleted through it. Mirrors
/// `session_resume_rejects_a_scene_mismatch`'s setup —
/// a session recorded under `"harnix"`, which this daemon never activated.
#[tokio::test]
async fn session_close_rejects_a_scene_mismatch() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let sid = base::session::SessionId::new();
    srv.store
        .append(
            sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                agix_version: "0.0.1".into(),
                parent_session_id: None,
                scene: Some("harnix".into()),
                project_root: None,
                session_kind: history::entry::SessionKind::Primary,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();

    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(closed["error"]["code"], codes::SCENE_MISMATCH, "{closed}");
    assert_eq!(closed["error"]["data"]["recorded_scene"], "harnix");
    assert!(
        srv.store.load(sid).await.is_ok(),
        "a rejected session.close must not touch the transcript"
    );

    let deleted = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.delete","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(deleted["error"]["code"], codes::SCENE_MISMATCH, "{deleted}");
    assert!(
        srv.store.load(sid).await.is_ok(),
        "a rejected session.delete must not touch the transcript"
    );

    srv.stop().await;
}

#[tokio::test]
async fn scene_list_reports_the_default_scene_as_active() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.list","id":1}"#,
    )
    .await;
    let scenes = resp["result"]["scenes"].as_array().unwrap();
    let code = scenes
        .iter()
        .find(|s| s["scene"] == "code")
        .expect("code should be registered");
    assert_eq!(code["active"], true, "{resp}");
    let harnix = scenes
        .iter()
        .find(|s| s["scene"] == "harnix")
        .expect("harnix should be registered even though inactive");
    assert_eq!(harnix["active"], false, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn session_create_rejects_an_inactive_scene() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"harnix"},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::SCENE_NOT_FOUND, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn scene_activate_makes_a_new_scene_available_for_session_create() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let activated = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"harnix"},"id":1}"#,
    )
    .await;
    assert_eq!(activated["result"]["active"], true, "{activated}");

    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"harnix"},"id":2}"#,
    )
    .await;
    let sid = match created["result"]["session_id"].as_str() {
        Some(s) => s,
        None => panic!("session.create should succeed once harnix is active: {created}"),
    };
    assert_eq!(created["result"]["scene"], "harnix");

    let entries = srv
        .store
        .load(base::session::SessionId::parse(sid).unwrap())
        .await
        .unwrap();
    match &entries[0].entry {
        history::entry::LogEntry::Meta { scene, .. } => {
            assert_eq!(scene.as_deref(), Some("harnix"));
        }
        other => panic!("expected Meta as the first entry, got {other:?}"),
    }

    srv.stop().await;
}

#[tokio::test]
async fn scene_activate_rejects_an_unknown_scene_name() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"not-a-real-scene"},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::SCENE_NOT_FOUND, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn scene_deactivate_is_blocked_by_active_sessions_then_succeeds_after_close() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"harnix"},"id":1}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"harnix"},"id":2}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let blocked = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.deactivate","params":{"scene":"harnix"},"id":3}"#,
    )
    .await;
    assert_eq!(
        blocked["error"]["code"],
        codes::SCENE_HAS_ACTIVE_SESSIONS,
        "{blocked}"
    );

    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":4}}"#
        ),
    )
    .await;

    let deactivated = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.deactivate","params":{"scene":"harnix"},"id":5}"#,
    )
    .await;
    assert_eq!(deactivated["result"]["scene"], "harnix", "{deactivated}");

    let listed = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.list","id":6}"#,
    )
    .await;
    let harnix = listed["result"]["scenes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["scene"] == "harnix")
        .unwrap();
    assert_eq!(harnix["active"], false, "{listed}");

    srv.stop().await;
}

#[tokio::test]
async fn a_reattached_session_comes_back_under_its_own_scene() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("a"), text_round("b")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"harnix"},"id":1}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"harnix"},"id":2}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    run_turn(&srv.sock, Some(&sid), "first").await;
    // The last request that carried a tool surface at all: `harnix` renames a
    // session after its first turn, and that naming call is a bare request
    // with no tools on it, so `last()` alone would be asking the wrong one.
    let offered_notebook_edit = |seen: &SeenRequests| {
        seen.lock()
            .unwrap()
            .iter()
            .rev()
            .find(|req| !req.tools.is_empty())
            .expect("some request carried a tool surface")
            .tools
            .iter()
            .any(|t| t.name == "NotebookEdit")
    };
    assert!(
        !offered_notebook_edit(&seen),
        "harnix's tool whitelist has no NotebookEdit in it"
    );

    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":3}}"#
        ),
    )
    .await;

    run_turn(&srv.sock, Some(&sid), "second, after reattach").await;
    assert!(
        !offered_notebook_edit(&seen),
        "reattaching must rebuild against the session's own recorded scene, not the pool default"
    );

    srv.stop().await;
}

#[tokio::test]
async fn run_turn_refuses_a_session_recorded_under_a_scene_this_daemon_stopped_serving() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"harnix"},"id":1}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"harnix"},"id":2}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":3}}"#
        ),
    )
    .await;
    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.deactivate","params":{"scene":"harnix"},"id":4}"#,
    )
    .await;

    let resp = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"hi"}},"id":5}}"#
        ),
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::SCENE_MISMATCH, "{resp}");
    assert_eq!(resp["error"]["data"]["recorded_scene"], "harnix", "{resp}");

    srv.stop().await;
}

/// `code` requires a project, and says so at creation rather than letting
/// the session exist and fail on its first tool call.
#[tokio::test]
async fn a_scene_that_requires_a_project_refuses_a_session_without_one() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"code","project_root":null},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::PROJECT_REQUIRED, "{resp}");

    srv.stop().await;
}

/// The capability bits are what a host reads to decide whether to ask for a
/// project or show a team entry point, so they have to be on the wire.
#[tokio::test]
async fn scene_list_reports_the_capability_bits() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.list","id":1}"#,
    )
    .await;
    let scenes = resp["result"]["scenes"].as_array().unwrap();
    let by_id = |id: &str| {
        scenes
            .iter()
            .find(|s| s["scene"] == id)
            .unwrap_or_else(|| panic!("{id} missing from scene.list: {resp}"))
            .clone()
    };

    assert_eq!(by_id("code")["requires_project"], true);
    assert_eq!(by_id("code")["supports_team"], true);
    assert_eq!(by_id("harnix")["requires_project"], false);
    assert_eq!(by_id("harnix")["supports_team"], true);

    srv.stop().await;
}

#[tokio::test]
async fn scene_describe_reports_capabilities_tools_and_effective_settings() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.describe","params":{"scene":"code"},"id":1}"#,
    )
    .await;

    assert_eq!(resp["result"]["scene"], "code", "{resp}");
    assert_eq!(resp["result"]["capabilities"]["requires_project"], true);
    assert_eq!(resp["result"]["capabilities"]["supports_team"], true);
    // `code` sets no whitelist, which is `null` rather than `[]` — the
    // latter would read as "no tools allowed", the opposite of the truth.
    assert!(resp["result"]["tools"]["allowed"].is_null(), "{resp}");
    assert!(
        !resp["result"]["tools"]["deferred"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{resp}"
    );
    assert!(resp["result"]["settings"].is_object(), "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn scene_describe_rejects_an_unknown_scene() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.describe","params":{"scene":"nope"},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::SCENE_NOT_FOUND, "{resp}");

    srv.stop().await;
}
