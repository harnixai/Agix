//! A recording recorder is kept in the session's spec: after a restart the
//! session records again, into the place it started recording, without being
//! asked; the recording starts over, which `session.calls` reports as a new
//! `generation`. A replay is not kept and has to be asked for again.

use crate::support::text_round;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use tokio_util::sync::CancellationToken;

/// One daemon process over `root`; a second one over the same root is a restart.
async fn start(
    root: &Path,
    name: &str,
    model: Arc<MockAnthropicClient>,
) -> (PathBuf, CancellationToken) {
    let sock = root.join(format!("{name}.sock"));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        None,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(root.to_path_buf()),
    );
    config.settings.values.memory.enabled = false;
    crate::support::allow_tools(&mut config, &[]);
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(cwd),
            model_client: Some(model as Arc<dyn AnthropicClient>),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");
    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel.clone()));
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
    (sock, cancel)
}

async fn ok(sock: &Path, method: &str, params: serde_json::Value) -> serde_json::Value {
    crate::support::call(sock, method, params).await
}

async fn transcript(sock: &Path, session: &str) -> String {
    ok(
        sock,
        "session.history",
        serde_json::json!({"session_id": session}),
    )
    .await
    .to_string()
}

fn calls_of(frame: &serde_json::Value) -> (u64, u64) {
    let result = &frame["result"];
    (
        result["recording"]["generation"]
            .as_u64()
            .expect("a recording"),
        result["total"].as_u64().unwrap(),
    )
}

#[tokio::test]
async fn a_recording_session_records_again_after_a_restart_without_being_asked() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(text_round("first"));
    let (sock, first) = start(dir.path(), "first", model.clone()).await;
    let session = ok(
        &sock,
        "session.create",
        serde_json::json!({"options": {"recorder": {"mode": "record"}}}),
    )
    .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    ok(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": session, "message": "one"}),
    )
    .await;
    let before = crate::support::call_frame(
        &sock,
        "session.calls",
        serde_json::json!({"session_id": session}),
    )
    .await;
    let (generation, total) = calls_of(&before);
    assert_eq!(total, 1, "{before}");
    first.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(text_round("second"));
    let (sock, _second) = start(dir.path(), "second", model).await;
    let cold = crate::support::call_frame(
        &sock,
        "session.calls",
        serde_json::json!({"session_id": session}),
    )
    .await;
    assert_eq!(
        calls_of(&cold),
        (generation, 1),
        "a session out of memory reads the recording it left: {cold}"
    );
    ok(
        &sock,
        "session.resume",
        serde_json::json!({"session_id": session}),
    )
    .await;
    ok(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": session, "message": "two"}),
    )
    .await;
    let after = crate::support::call_frame(
        &sock,
        "session.calls",
        serde_json::json!({"session_id": session}),
    )
    .await;
    let (restarted, total) = calls_of(&after);
    assert_ne!(restarted, generation, "{after}");
    assert_eq!(total, 1, "the recording covers this run only: {after}");
    assert!(
        after["result"]["recording"]["name"] == session.as_str(),
        "{after}"
    );

    let stale = crate::support::call_frame(
        &sock,
        "session.calls",
        serde_json::json!({"session_id": session, "from": 1, "generation": generation}),
    )
    .await;
    assert_eq!(stale["error"]["code"], -32602, "{stale}");
    assert_eq!(stale["error"]["data"]["generation"], restarted, "{stale}");
}

#[tokio::test]
async fn a_replay_is_not_kept_and_a_session_brought_back_without_it_talks_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let recordings = dir.path().join("recordings");
    let recorder = |mode: &str| {
        serde_json::json!({"options": {"recorder": {
            "mode": mode, "name": "kept", "dir": recordings.display().to_string()
        }}})
    };

    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(text_round("recorded answer"));
    let (sock, first) = start(dir.path(), "first", model.clone()).await;
    let recorded = ok(&sock, "session.create", recorder("record")).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    ok(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": recorded, "message": "one"}),
    )
    .await;
    let replaying = ok(&sock, "session.create", recorder("replay")).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    ok(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": replaying, "message": "one"}),
    )
    .await;
    let replayed = transcript(&sock, &replaying).await;
    assert!(replayed.contains("recorded answer"), "{replayed}");
    let not_recording = ok(
        &sock,
        "session.calls",
        serde_json::json!({"session_id": replaying}),
    )
    .await;
    assert!(not_recording["recording"].is_null(), "{not_recording}");
    assert_eq!(not_recording["calls"], serde_json::json!([]));
    first.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(text_round("live answer"));
    let (sock, _second) = start(dir.path(), "second", model).await;
    ok(
        &sock,
        "session.resume",
        serde_json::json!({"session_id": replaying}),
    )
    .await;
    ok(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": replaying, "message": "two"}),
    )
    .await;
    let live = transcript(&sock, &replaying).await;
    assert!(live.contains("live answer"), "{live}");
}
