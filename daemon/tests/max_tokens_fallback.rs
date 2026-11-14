//! `--max-tokens` is every project's `model.max_tokens` where its settings
//! files do not set one, not only the daemon's own project's.

use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const FALLBACK: u32 = 1234;

/// One assistant turn that says "ok" and stops.
fn one_turn() -> Vec<StreamEvent> {
    vec![
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
    ]
}

struct PermitAll;

#[async_trait::async_trait]
impl base::interface::permission::PermissionPolicy for PermitAll {
    async fn decide(
        &self,
        _req: &base::interface::permission::PermissionRequest<'_>,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit {
            basis: base::interface::permission::Basis::Bypass,
            profile: Default::default(),
            obligations: Default::default(),
        }
    }
}

struct Daemon {
    sock: std::path::PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
    _handle: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let request = serde_json::json!({
            "jsonrpc":"2.0","method":method,"params":params,"id":1
        });
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let (r, _w) = conn.split();
        let mut reader = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            assert!(n > 0, "{method}: connection closed before a response");
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                return frame;
            }
        }
    }

    async fn ok(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let response = self.call(method, params).await;
        assert!(
            response.get("result").is_some(),
            "{method} failed: {response}"
        );
        response["result"].clone()
    }
}

async fn start_daemon() -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("tokens.sock");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(one_turn());
    model.push_turn(one_turn());

    let mut config = load_daemon_config(
        DEFAULT_MODEL,
        FALLBACK,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.settings.values.memory_enabled = false;

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(cwd.clone()),
            model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
            host_policy: Some(Arc::new(PermitAll)),
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

    Daemon {
        sock,
        model,
        _dir: dir,
        _handle: handle,
    }
}

async fn max_tokens_asked_in(daemon: &Daemon, project: &std::path::Path, needle: &str) -> u32 {
    let session = daemon
        .ok(
            "session.create",
            serde_json::json!({"project_root": project}),
        )
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": session, "message": needle}),
        )
        .await;
    for i in 0.. {
        let Some(request) = daemon.model.nth_request(i) else {
            break;
        };
        if serde_json::to_string(&request).unwrap().contains(needle) {
            return request.max_tokens;
        }
    }
    panic!("no request carried `{needle}`")
}

#[tokio::test]
async fn another_project_falls_back_to_the_flag_and_its_own_setting_wins() {
    let daemon = start_daemon().await;

    let plain = tempfile::tempdir().unwrap();
    assert_eq!(
        max_tokens_asked_in(&daemon, plain.path(), "MSG-PLAIN").await,
        FALLBACK,
        "a project whose files set nothing gets the flag's value"
    );

    let own = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(own.path().join(".agix")).unwrap();
    std::fs::write(
        own.path().join(".agix/settings.json"),
        r#"{"model": {"max_tokens": 4321}}"#,
    )
    .unwrap();
    assert_eq!(
        max_tokens_asked_in(&daemon, own.path(), "MSG-OWN").await,
        4321,
        "a project that sets it keeps its own"
    );
}
