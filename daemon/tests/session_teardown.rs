//! What `session.delete`, `session.close` and `daemon.shutdown` wait for: an
//! engine taken out of memory may still be finishing its turn, and nothing it
//! writes on the way out may outlive a delete or be cut off by a shutdown.
//! The turn under test is held open by an `AskUserQuestion` nobody answers,
//! so the engine stops only once the question times out.

use crate::support::text_round;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use model::types::MessagesRequest;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const QUESTION_TIMEOUT: Duration = Duration::from_secs(2);

struct AsksThenSaysOk {
    asked: StdMutex<bool>,
}

impl AnthropicClient for AsksThenSaysOk {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        let first = !std::mem::replace(&mut *self.asked.lock().unwrap(), true);
        let events = if first { ask_round() } else { text_round("ok") };
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

fn ask_round() -> Vec<StreamEvent> {
    let input = json!({
        "question": "which branch?",
        "header": "Branch",
        "options": [{"key": "a", "label": "main"}, {"key": "b", "label": "dev"}],
    });
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: "ask-1".into(),
                name: "AskUserQuestion".into(),
                input: serde_json::Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::ToolUse),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        },
    ]
}

/// `root` is the global tier and the daemon's own project; `ended` is the
/// file a `SessionEnd` hook creates once an engine has stopped.
struct Daemon {
    server: Arc<DaemonServer>,
    sock: PathBuf,
    root: PathBuf,
    ended: PathBuf,
    _dir: tempfile::TempDir,
    handle: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let sock = root.join("teardown.sock");
        let ended = root.join("session-ended");
        let hooks = json!({ "SessionEnd": [{
            "type": "command",
            "command": format!("cat >/dev/null; touch '{}'", ended.display()),
        }]});
        std::fs::write(
            root.join("settings.json"),
            json!({ "hooks_config": hooks, "memory_enabled": false }).to_string(),
        )
        .unwrap();
        let config = load_daemon_config(
            "claude-sonnet-4-6",
            2000,
            Some(&sock),
            "code",
            &StaticDaemonPaths::new(root.clone()),
        );
        let store: Arc<dyn history::store::HistoryStore> = Arc::new(
            history::store::JsonlHistoryStore::with_roots(
                &root,
                history::path::HistoryRoots::under(&root),
            )
            .await
            .unwrap(),
        );
        let client = Arc::new(AsksThenSaysOk {
            asked: StdMutex::new(false),
        });
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(root.clone()),
                permission_prompt_timeout: Some(QUESTION_TIMEOUT),
                model_client: Some(client as Arc<dyn AnthropicClient>),
                transcripts: daemon::Transcripts::In(store),
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");
        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        let serving = server.clone();
        let bind = sock.clone();
        let handle = tokio::spawn(async move {
            let _ = serving.serve_unix(&bind).await;
        });
        for _ in 0..80 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(sock.exists(), "socket never bound");
        Self {
            server,
            sock,
            root,
            ended,
            _dir: dir,
            handle,
        }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        let request = json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let (r, _w) = conn.split();
        let mut reader = BufReader::new(r);
        loop {
            let mut line = String::new();
            assert!(
                reader.read_line(&mut line).await.unwrap() > 0,
                "{method}: EOF"
            );
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v.get("id").is_some() {
                return v;
            }
        }
    }

    async fn create(&self, project_root: &Path) -> String {
        let created = self
            .call(
                "session.create",
                json!({
                    "project_root": project_root,
                    "options": {"prompt_types": ["permission", "question"]},
                }),
            )
            .await;
        created["result"]["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("session.create: {created}"))
            .to_string()
    }

    /// Start a turn on `sid` and return once its question is on the screen,
    /// leaving the turn running.
    async fn start_asking(&self, sid: &str) -> tokio::task::JoinHandle<()> {
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        let request = json!({"jsonrpc":"2.0","method":"session.run_turn",
            "params":{"session_id": sid, "message": "ask me"},"id":1});
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut reader = BufReader::new(conn);
        loop {
            let mut line = String::new();
            assert!(
                reader.read_line(&mut line).await.unwrap() > 0,
                "the turn ended before it asked"
            );
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert!(v.get("id").is_none(), "the turn ended before it asked: {v}");
            if v["params"]["event"]["kind"] == "prompt"
                && v["params"]["event"]["prompt_type"] == "question"
            {
                break;
            }
        }
        tokio::spawn(async move {
            let mut rest = String::new();
            while reader.read_line(&mut rest).await.unwrap_or(0) > 0 {
                rest.clear();
            }
        })
    }

    async fn engine_ended(&self) {
        for _ in 0..400 {
            if self.ended.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the session's engine never stopped");
    }

    async fn stop(self) {
        self.server.shutdown_token().cancel();
        let _ = self.handle.await;
    }
}

fn files_named(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_named(&path, name));
        } else if path.file_name().is_some_and(|n| n == name) {
            found.push(path);
        }
    }
    found
}

#[tokio::test]
async fn a_deleted_session_leaves_nothing_behind_once_its_engine_stops() {
    let daemon = Daemon::start().await;
    let project = daemon.root.join("project-x");
    std::fs::create_dir_all(&project).unwrap();
    let sid = daemon.create(&project).await;
    let turn = daemon.start_asking(&sid).await;

    let deleted = daemon
        .call("session.delete", json!({"session_id": sid}))
        .await;
    assert_eq!(deleted["result"]["deleted"], true, "{deleted}");
    daemon.engine_ended().await;
    turn.abort();

    let logs = files_named(&daemon.root.join("projects"), &format!("{sid}.jsonl"));
    assert!(
        logs.is_empty(),
        "the deleted session's log is back: {logs:?}"
    );
    let sidecar = daemon.root.join("sessions").join(&sid);
    assert!(!sidecar.exists(), "{} is back", sidecar.display());
    let listed = daemon.call("session.list", json!({})).await;
    assert!(
        !listed["result"].to_string().contains(&sid),
        "session.list lists the deleted session: {listed}"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_shutdown_right_after_a_close_waits_for_the_closed_engine() {
    let daemon = Daemon::start().await;
    let sid = daemon.create(&daemon.root).await;
    let turn = daemon.start_asking(&sid).await;

    let closed = daemon
        .call("session.close", json!({"session_id": sid}))
        .await;
    assert!(closed["result"].is_object(), "{closed}");
    let shutdown = daemon.call("daemon.shutdown", json!({})).await;
    assert_eq!(shutdown["result"]["shutting_down"], true, "{shutdown}");
    assert!(
        daemon.ended.exists(),
        "daemon.shutdown answered before the closed session's SessionEnd hook ran"
    );
    turn.abort();
    daemon.stop().await;
}
