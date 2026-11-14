//! The file tools refuse every credential store a sandboxed command cannot
//! read, not only `.ssh` / `.aws` / `.gnupg`, in every permission mode.

use std::path::PathBuf;
use std::sync::Arc;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

fn stop(reason: base::message::StopReason) -> StreamEvent {
    StreamEvent::MessageDelta {
        delta: MessageDeltaPayload {
            stop_reason: Some(reason),
            stop_sequence: None,
        },
        usage: Some(Usage::default()),
    }
}

fn text_round(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::TextDelta { text: text.into() },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        stop(base::message::StopReason::EndTurn),
    ]
}

fn tool_round(id: &str, tool: &str, input: Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.into(),
                name: tool.into(),
                input: Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        stop(base::message::StopReason::ToolUse),
    ]
}

struct Daemon {
    sock: PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (home, work) = (dir.path().join("home"), dir.path().join("work"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(work.join(".agix")).unwrap();
        std::fs::write(
            home.join("settings.json"),
            json!({"sandbox": {"read_anywhere": true, "dangerously_disable_sandbox": true}})
                .to_string(),
        )
        .unwrap();

        let sock = dir.path().join("home-credentials.sock");
        let paths =
            StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.values.memory_enabled = false;
        config.settings.values.permission_mode =
            base::permission::PermissionMode::BypassPermissions;
        let model = Arc::new(MockAnthropicClient::new());
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(work.clone()),
                model_client: Some(model.clone()),
                transcripts: daemon::Transcripts::Nowhere,
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");
        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        let listener = DaemonServer::bind_unix(&sock).unwrap();
        tokio::spawn(async move {
            let _ = server.serve_unix_listener(listener).await;
        });
        Self {
            sock,
            model,
            _dir: dir,
        }
    }

    async fn turn(&self) {
        let request = json!({"jsonrpc":"2.0","method":"session.run_turn",
                             "params":{"message":"search"},"id":1});
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut reader = BufReader::new(conn);
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                assert!(frame.get("result").is_some(), "turn failed: {frame}");
                return;
            }
        }
    }

    fn request(&self, n: usize) -> String {
        serde_json::to_string(&self.model.nth_request(n).unwrap()).unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_home_credential_store_is_refused_to_read_and_to_search() {
    let daemon = Daemon::start().await;
    let home = base::paths::home_dir().expect("a home directory");
    daemon.model.push_turn(tool_round(
        "toolu_read",
        "Read",
        json!({"file_path": home.join(".git-credentials").display().to_string()}),
    ));
    daemon.model.push_turn(tool_round(
        "toolu_grep",
        "Grep",
        json!({"pattern": "x", "path": home.join(".kube").display().to_string()}),
    ));
    daemon.model.push_turn(text_round("done"));
    daemon.turn().await;

    for (n, name) in [(1, ".git-credentials"), (2, ".kube")] {
        let shown = daemon.request(n);
        assert!(
            shown.contains("holds credentials"),
            "{name} was not refused as a credential: {shown}"
        );
    }
}
