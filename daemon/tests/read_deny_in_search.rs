//! A `Read(...)` deny rule keeps a file out of what `Grep` and `Glob` return,
//! not only out of `Read`: searching the directory above it does not walk
//! past the rule.

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
    work: PathBuf,
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
            work.join(".agix/settings.json"),
            json!({"permission_rules": [{"tool": "Read(secret/**)", "action": "deny"}]})
                .to_string(),
        )
        .unwrap();

        let sock = dir.path().join("read-deny.sock");
        let paths =
            StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.values.memory_enabled = false;
        config.settings.values.permission_mode = base::permission::PermissionMode::AcceptEdits;
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
            work,
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
async fn grep_and_glob_from_the_project_root_leave_out_what_a_read_deny_covers() {
    let daemon = Daemon::start().await;
    std::fs::create_dir_all(daemon.work.join("secret")).unwrap();
    std::fs::write(daemon.work.join("secret/k.txt"), "TOKEN-IN-SECRET\n").unwrap();
    std::fs::write(daemon.work.join("open.txt"), "TOKEN-IN-OPEN\n").unwrap();

    daemon.model.push_turn(tool_round(
        "toolu_grep",
        "Grep",
        json!({"pattern": "TOKEN-IN", "output_mode": "content"}),
    ));
    daemon.model.push_turn(tool_round(
        "toolu_glob",
        "Glob",
        json!({"pattern": "**/*.txt"}),
    ));
    daemon.model.push_turn(text_round("done"));
    daemon.turn().await;

    let after_grep = daemon.request(1);
    assert!(after_grep.contains("TOKEN-IN-OPEN"), "{after_grep}");
    assert!(
        !after_grep.contains("TOKEN-IN-SECRET"),
        "Grep read a file a Read deny covers: {after_grep}"
    );
    let after_glob = daemon.request(2);
    assert!(after_glob.contains("open.txt"), "{after_glob}");
    assert!(
        !after_glob.contains("k.txt"),
        "Glob listed a file a Read deny covers: {after_glob}"
    );
}
