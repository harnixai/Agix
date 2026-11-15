//! `recorder` is never written to a session's log, so after the daemon
//! restarts, the call that brings the session back is where it comes from.

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
        2000,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(root.to_path_buf()),
    );
    config.settings.values.memory_enabled = false;
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

#[tokio::test]
async fn a_session_brought_back_after_a_restart_records_again_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let recordings = dir.path().join("recordings");
    let recorder = serde_json::json!({"mode": "record", "dir": recordings.display().to_string()});

    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(text_round("first"));
    let (sock, first) = start(dir.path(), "first", model.clone()).await;
    let session = ok(
        &sock,
        "session.create",
        serde_json::json!({"options": {"recorder": recorder}}),
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
    let recording = recordings.join(&session);
    assert!(recording.exists(), "the first daemon did not record");
    first.cancel();
    std::fs::remove_dir_all(&recording).unwrap();

    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(text_round("second"));
    let (sock, _second) = start(dir.path(), "second", model).await;
    ok(
        &sock,
        "session.resume",
        serde_json::json!({"session_id": session, "options": {"recorder": recorder}}),
    )
    .await;
    ok(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": session, "message": "two"}),
    )
    .await;
    assert!(recording.exists(), "the resumed session did not record");
}
