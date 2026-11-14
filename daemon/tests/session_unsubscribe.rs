//! `session.unsubscribe` ends one connection's stream of a session's
//! `session.event` frames, and no other connection's: after it, a turn the
//! session runs reaches the still-subscribed connection and not this one.

use crate::support::text_round;
use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl Connection {
    async fn open(sock: &std::path::Path) -> Self {
        let (r, writer) = UnixStream::connect(sock).await.unwrap().into_split();
        Self {
            reader: BufReader::new(r),
            writer,
            next_id: 0,
        }
    }

    async fn frame(&mut self) -> Value {
        let mut line = String::new();
        assert!(
            self.reader.read_line(&mut line).await.unwrap() > 0,
            "closed"
        );
        serde_json::from_str(&line).unwrap()
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({"jsonrpc":"2.0","method":method,"params":params,"id":id});
        self.writer
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let frame = self.frame().await;
            if frame["id"] == id {
                assert!(frame.get("result").is_some(), "{method} failed: {frame}");
                return frame["result"].clone();
            }
        }
    }

    async fn wait_for_event(&mut self, kind: &str) {
        loop {
            let frame = self.frame().await;
            if frame["method"] == "session.event" && frame["params"]["event"]["kind"] == kind {
                return;
            }
        }
    }

    async fn silent_for(&mut self, window: Duration) -> Option<Value> {
        tokio::time::timeout(window, self.frame()).await.ok()
    }
}

#[tokio::test]
async fn an_unsubscribed_connection_gets_no_more_frames_while_another_still_does() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("unsubscribe.sock");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(text_round("first"));
    model.push_turn(text_round("second"));
    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.settings.values.memory_enabled = false;
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(cwd),
            model_client: Some(model),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
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

    let mut owner = Connection::open(&sock).await;
    let session = owner
        .call("session.run_turn", json!({"message": "hello"}))
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut leaving = Connection::open(&sock).await;
    let mut staying = Connection::open(&sock).await;
    for watcher in [&mut leaving, &mut staying] {
        watcher
            .call("session.subscribe", json!({"session_id": session}))
            .await;
    }
    let off = leaving
        .call("session.unsubscribe", json!({"session_id": session}))
        .await;
    assert_eq!(off["subscribed"], false);

    owner
        .call(
            "session.run_turn",
            json!({"session_id": session, "message": "again"}),
        )
        .await;
    staying.wait_for_event("text_delta").await;

    let late = leaving.silent_for(Duration::from_millis(300)).await;
    assert!(
        late.is_none(),
        "a frame reached a connection that had unsubscribed: {late:?}"
    );
}
