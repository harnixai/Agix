//! A project nobody has trusted yet: its own hooks wait until the host
//! trusts it, and every answer about it says that only the default decided.

use std::path::PathBuf;
use std::sync::Arc;
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

struct SaysOk;

impl AnthropicClient for SaysOk {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        let events = vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::TextDelta { text: "ok".into() },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: Some(base::message::StopReason::EndTurn),
                    stop_sequence: None,
                },
                usage: Some(Usage::default()),
            },
        ];
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

/// `home` is the global tier, written with nothing about trust in it; `work`
/// is the daemon's own project, whose `SessionStart` hook creates `started`.
struct Daemon {
    sock: PathBuf,
    work: PathBuf,
    started: PathBuf,
    _dir: tempfile::TempDir,
    _handle: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (home, work) = (dir.path().join("home"), dir.path().join("work"));
        let started = dir.path().join("session-started");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(work.join(".agix")).unwrap();
        std::fs::write(home.join("settings.json"), "{}").unwrap();
        std::fs::write(
            work.join(".agix/settings.json"),
            json!({"hooks_config": {"SessionStart": [{
                "type": "command",
                "command": format!("cat >/dev/null; touch '{}'", started.display()),
            }]}})
            .to_string(),
        )
        .unwrap();

        let sock = dir.path().join("default.sock");
        let paths = StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home);
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.values.memory_enabled = false;
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(work.clone()),
                model_client: Some(Arc::new(SaysOk) as Arc<dyn AnthropicClient>),
                transcripts: daemon::Transcripts::Nowhere,
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");
        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        let bind = sock.clone();
        let handle = tokio::spawn(async move {
            let _ = server.serve_unix(&bind).await;
        });
        for _ in 0..80 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(sock.exists(), "socket never bound");
        Self {
            sock,
            work,
            started,
            _dir: dir,
            _handle: handle,
        }
    }

    async fn ok(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
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
                assert!(v.get("result").is_some(), "{method} failed: {v}");
                return v["result"].clone();
            }
        }
    }

    fn project(&self) -> serde_json::Value {
        json!({ "project_root": self.work })
    }

    /// A new session in the project with one turn run, so that its
    /// `SessionStart` hooks have run by the time this returns.
    async fn session_with_a_turn(&self) -> serde_json::Value {
        let created = self.ok("session.create", self.project()).await;
        self.ok(
            "session.run_turn",
            json!({"session_id": created["session_id"], "message": "hi"}),
        )
        .await;
        created
    }
}

#[tokio::test]
async fn a_project_nobody_trusted_does_not_run_its_own_hooks() {
    let daemon = Daemon::start().await;

    let created = daemon.session_with_a_turn().await;
    assert_eq!(created["project_trust"], "untrusted", "{created}");
    assert_eq!(created["trust_source"], "default", "{created}");
    assert!(
        !daemon.started.exists(),
        "the project's SessionStart hook ran before anybody trusted it"
    );

    let status = daemon.ok("project.status", daemon.project()).await;
    assert_eq!(status["trust"], "untrusted", "{status}");
    assert_eq!(status["default"], "untrusted", "{status}");
    assert_eq!(status["trust_source"], "default", "{status}");
    assert!(
        status["requires_trust"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k == "hooks_config"),
        "{status}"
    );

    let report = daemon.ok("daemon.doctor", json!({})).await;
    let check = report["health"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "project.trust")
        .unwrap_or_else(|| panic!("no project.trust check: {report}"))
        .clone();
    assert_eq!(check["status"], "degraded", "{check}");
    assert!(
        check["summary"].as_str().unwrap().contains("hooks_config"),
        "{check}"
    );
}

#[tokio::test]
async fn once_the_host_trusts_the_project_its_new_sessions_run_its_hooks() {
    let daemon = Daemon::start().await;
    daemon.session_with_a_turn().await;
    assert!(!daemon.started.exists());

    let mut params = daemon.project();
    params["trusted"] = json!(true);
    let trusted = daemon.ok("project.trust", params).await;
    assert_eq!(trusted["trust"], "trusted", "{trusted}");
    assert_eq!(trusted["trust_source"], "host", "{trusted}");

    let created = daemon.session_with_a_turn().await;
    assert_eq!(created["project_trust"], "trusted", "{created}");
    assert_eq!(created["trust_source"], "host", "{created}");
    assert!(
        daemon.started.exists(),
        "the project's SessionStart hook did not run once the project was trusted"
    );
}
