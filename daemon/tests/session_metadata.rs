//! Session names (rename, the automatic namer, surviving a resume) and `session.get` / `config.get` / `ping`.

use std::time::Duration;

use crate::support::*;
use daemon::rpc::codes;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// A session's name belongs to the person looking at it, and it has to
/// survive that person closing the window. Kept beside the log rather than in
/// a host's own overlay, so two hosts sharing `~/.agix` agree about what a
/// session is called.
#[tokio::test]
async fn a_renamed_session_keeps_its_name_after_it_leaves_memory() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("answer")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "hello").await;
    let renamed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.rename","id":1,"params":{{"session_id":"{sid}","name":"重构 daemon 装配"}}}}"#
        ),
    )
    .await;
    assert_eq!(renamed["result"]["name"], "重构 daemon 装配");

    let listed_live = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","id":2}"#,
    )
    .await;
    let live_entry = listed_live["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == sid.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("session missing from the list: {listed_live}"));
    assert_eq!(live_entry["name"], "重构 daemon 装配");

    // Out of memory entirely — the case a host's own overlay gets wrong,
    // since it never had a row for a session it did not open.
    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","id":3,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    let listed_cold = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","id":4}"#,
    )
    .await;
    let cold_entry = listed_cold["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == sid.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("session missing from the list: {listed_cold}"));
    assert_eq!(
        cold_entry["status"], "inactive",
        "the session really did leave memory: {cold_entry}"
    );
    assert_eq!(
        cold_entry["name"], "重构 daemon 装配",
        "a name outlives the running session: {cold_entry}"
    );

    // Clearing puts it back to whatever names it automatically.
    let cleared = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.rename","id":5,"params":{{"session_id":"{sid}","name":null}}}}"#
        ),
    )
    .await;
    assert_eq!(cleared["result"]["name"], serde_json::Value::Null);

    srv.stop().await;
}

#[tokio::test]
async fn a_name_that_would_break_a_list_is_refused() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("answer")], ask_settings(), Duration::ZERO).await;
    let sid = run_turn(&srv.sock, None, "hello").await;

    for bad in [r#""  ""#, r#""a\nb""#] {
        let response = rpc(
            &srv.sock,
            &format!(
                r#"{{"jsonrpc":"2.0","method":"session.rename","id":1,"params":{{"session_id":"{sid}","name":{bad}}}}}"#
            ),
        )
        .await;
        assert_eq!(
            response["error"]["code"],
            codes::INVALID_PARAMS as i64,
            "`{bad}` is not a name: {response}"
        );
    }

    let missing = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.rename","id":2,"params":{"session_id":"WnZ55VTERDNsFKVQdkQ7hi","name":"x"}}"#,
    )
    .await;
    assert_eq!(
        missing["error"]["code"], codes::SESSION_NOT_FOUND as i64,
        "naming a session that does not exist would put an unopenable row in every listing: {missing}"
    );

    srv.stop().await;
}

/// A param of the wrong type is refused, not read as absent: a rename with a
/// number for a name must not clear the name.
#[tokio::test]
async fn a_lifecycle_param_of_the_wrong_type_is_refused() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("answer")], ask_settings(), Duration::ZERO).await;
    let sid = run_turn(&srv.sock, None, "hello").await;
    let renamed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.rename","id":1,"params":{{"session_id":"{sid}","name":"kept"}}}}"#
        ),
    )
    .await;
    assert_eq!(renamed["result"]["name"], "kept", "{renamed}");

    for (method, params) in [
        (
            "session.rename",
            format!(r#"{{"session_id":"{sid}","name":3}}"#),
        ),
        ("session.create", r#"{"project_root":5}"#.to_string()),
        ("session.list", r#"{"include_children":"yes"}"#.to_string()),
        (
            "session.fork",
            format!(r#"{{"session_id":"{sid}","at_message":-1}}"#),
        ),
        (
            "session.setPrompt",
            format!(r#"{{"session_id":"{sid}","append":7}}"#),
        ),
        ("session.subscribe", r#"{"resume":true}"#.to_string()),
    ] {
        let response = rpc(
            &srv.sock,
            &format!(r#"{{"jsonrpc":"2.0","method":"{method}","id":2,"params":{params}}}"#),
        )
        .await;
        assert_eq!(
            response["error"]["code"],
            codes::INVALID_PARAMS as i64,
            "{method} {params}: {response}"
        );
        let message = response["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.starts_with(&format!("invalid params for {method}: ")),
            "{message}"
        );
    }

    let (name, _) = session_name(&srv.sock, &sid).await;
    assert_eq!(name, "kept", "a refused rename left the name alone");

    srv.stop().await;
}

/// A scene that names its own sessions does so after the first turn — by
/// which time the user may already have typed a name. The guess must not win:
/// it arrives second and it is a guess.
#[tokio::test]
async fn the_automatic_namer_does_not_overwrite_a_name_somebody_typed() {
    let (srv, _seen) = start_scripted_server_in(
        "harnix",
        vec![text_round("answer"), text_round("模型猜的标题")],
        ask_settings(),
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
        .unwrap_or_else(|| panic!("no session: {created}"))
        .to_string();

    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.rename","id":2,"params":{{"session_id":"{sid}","name":"我起的名字"}}}}"#
        ),
    )
    .await;

    // The turn that would otherwise trigger automatic naming.
    run_turn(&srv.sock, Some(&sid), "hello").await;

    let detail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","id":3,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    assert_eq!(
        detail["result"]["name"], "我起的名字",
        "the name somebody typed survived the turn: {detail}"
    );

    srv.stop().await;
}

async fn session_name(sock: &std::path::Path, sid: &str) -> (serde_json::Value, serde_json::Value) {
    let detail = rpc(
        sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","id":7,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    let listed = rpc(sock, r#"{"jsonrpc":"2.0","method":"session.list","id":8}"#).await;
    let row = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == sid)
        .cloned()
        .unwrap_or_else(|| panic!("session missing from the list: {listed}"));
    (detail["result"]["name"].clone(), row["name"].clone())
}

#[tokio::test]
async fn a_resumed_session_keeps_the_name_somebody_typed() {
    let (srv, _seen) = start_scripted_server_in(
        "harnix",
        vec![
            text_round("answer"),
            text_round("自动起的标题"),
            text_round("second answer"),
            text_round("模型猜的标题"),
        ],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let sid = run_turn(&srv.sock, None, "hello").await;
    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.rename","id":2,"params":{{"session_id":"{sid}","name":"我起的名字"}}}}"#
        ),
    )
    .await;

    close_and_resume(&srv.sock, &sid).await;
    let (got, listed) = session_name(&srv.sock, &sid).await;
    assert_eq!(got, "我起的名字", "session.get after resume");
    assert_eq!(listed, "我起的名字", "session.list after resume");

    run_turn(&srv.sock, Some(&sid), "and then").await;
    let (got, listed) = session_name(&srv.sock, &sid).await;
    assert_eq!(
        got, "我起的名字",
        "the turn after a resume is not a first turn"
    );
    assert_eq!(listed, "我起的名字");

    srv.stop().await;
}

#[tokio::test]
async fn an_automatic_name_survives_a_resume() {
    let (srv, _seen) = start_scripted_server_in(
        "harnix",
        vec![text_round("answer"), text_round("自动起的标题")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let sid = run_turn(&srv.sock, None, "hello").await;
    let (before, _) = session_name(&srv.sock, &sid).await;
    assert_eq!(before, "自动起的标题");

    close_and_resume(&srv.sock, &sid).await;
    let (got, listed) = session_name(&srv.sock, &sid).await;
    assert_eq!(got, "自动起的标题", "session.get after resume");
    assert_eq!(listed, "自动起的标题", "session.list after resume");

    srv.stop().await;
}

#[tokio::test]
async fn a_harnix_session_names_itself_on_a_daemon_whose_own_scene_does_not() {
    // Two rounds: the turn's own answer, then the naming call `harnix` makes
    // after a first turn. `code` — this daemon's own scene — makes neither.
    let (srv, _seen) = start_scripted_server(
        vec![text_round("answered"), text_round("会话标题")],
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
    let harnix_sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (_frames, resp) = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{harnix_sid}","message":"hello"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(
        resp["result"]["name"], "会话标题",
        "a harnix session must name itself even when the daemon's own scene would not: {resp}"
    );

    // The control: the same daemon's own scene still does not.
    let (_frames, resp) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"hello"},"id":4}"#,
    )
    .await;
    assert!(
        resp["result"]["name"].is_null(),
        "code does not auto-name, and reading the session's scene must not change that: {resp}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn ping_answers_without_a_session() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"daemon.ping","id":1}"#,
    )
    .await;
    assert_eq!(resp["result"]["pong"], true, "{resp}");
    assert_eq!(
        resp["result"]["protocol_version"],
        daemon::discovery::INSTANCE_PROTOCOL_VERSION,
        "{resp}"
    );
    assert!(
        resp["result"]["capabilities"]
            .as_array()
            .is_some_and(|c| c.iter().any(|c| c == "subscription_dropped_notice")),
        "{resp}"
    );
    assert!(
        resp["result"]["capabilities"]
            .as_array()
            .is_some_and(|c| c.iter().any(|c| c == "goals")),
        "{resp}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn subscribing_to_daemon_events_twice_delivers_each_event_once() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("x")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let mut events = UnixStream::connect(&srv.sock).await.unwrap();
    events
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"daemon.subscribeEvents\",\"id\":1}\n\
              {\"jsonrpc\":\"2.0\",\"method\":\"daemon.subscribeEvents\",\"id\":2}\n",
        )
        .await
        .unwrap();
    let (r, _w) = events.split();
    let mut reader = BufReader::new(r);
    let mut acks = 0;
    while acks < 2 {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v.get("id").is_some() {
            assert_eq!(v["result"]["subscribed"], true, "{v}");
            acks += 1;
        }
    }

    run_turn(&srv.sock, None, "hello").await;
    let mut started = 0;
    let _ = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["params"]["kind"] == "turn_started" {
                started += 1;
            }
        }
    })
    .await;
    assert_eq!(started, 1);

    srv.stop().await;
}

/// `turn_state` is what a client polls to decide whether its send button is
/// live, so it has to be right on both sides of a turn.
#[tokio::test]
async fn session_get_reports_the_summary_and_the_turn_state() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("hello")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "hi").await;
    let got = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;

    assert_eq!(got["result"]["session_id"], sid, "{got}");
    assert_eq!(got["result"]["scene"], "code", "{got}");
    assert_eq!(got["result"]["scene_active"], true, "{got}");
    assert_eq!(got["result"]["turn_state"], "idle", "{got}");
    assert!(got["result"]["current_turn_id"].is_null(), "{got}");
    // `message_count` comes straight from `session.list`'s summary — asserted
    // present, not non-zero: a live session reports 0 until its transcript is
    // read back, which is `session.list`'s existing behavior and not something
    // `session.get` should quietly diverge from.
    assert!(got["result"]["message_count"].is_number(), "{got}");

    srv.stop().await;
}

#[tokio::test]
async fn session_get_on_an_unknown_session_is_not_found() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let got = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.get","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(got["error"]["code"], codes::SESSION_NOT_FOUND, "{got}");

    srv.stop().await;
}

/// A tier with no file on disk is `null`, not `{}` — "nothing configured
/// here" and "configured empty" are different states a config UI has to be
/// able to tell apart.
#[tokio::test]
async fn config_get_distinguishes_an_absent_tier_from_an_empty_one() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let scene_tier = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.get","params":{"scene":"code","tier":"scene"},"id":1}"#,
    )
    .await;
    assert_eq!(scene_tier["result"]["tier"], "scene", "{scene_tier}");
    assert!(scene_tier["result"]["settings"].is_null(), "{scene_tier}");

    let effective = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.get","params":{"scene":"code"},"id":2}"#,
    )
    .await;
    assert_eq!(effective["result"]["tier"], "effective", "{effective}");
    assert!(effective["result"]["settings"].is_object(), "{effective}");

    srv.stop().await;
}

#[tokio::test]
async fn config_get_rejects_an_unknown_tier() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.get","params":{"scene":"code","tier":"nope"},"id":1}"#,
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::INVALID_PARAMS, "{resp}");

    srv.stop().await;
}

/// Credentials are redacted unless asked for, at whatever depth they sit.
#[tokio::test]
async fn config_get_redacts_credentials_by_default() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"config.get","params":{"scene":"code"},"id":1}"#,
    )
    .await;
    let dumped = resp.to_string();
    assert!(
        !dumped.contains("test-key"),
        "the scripted server's auth token leaked into config.get: {dumped}"
    );

    srv.stop().await;
}
