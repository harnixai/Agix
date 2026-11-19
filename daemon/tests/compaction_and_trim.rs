//! Compaction rewrites history and is the only thing `PreCompact` and
//! `PostCompact` cover; trimming old tool results is not a compaction, fires
//! neither, and still tells the host on a `compact` frame marked `trim`.
//!
//! Driven through the socket against a scripted model, so what is asserted is
//! what a host would see: frames, the history, and the hooks' own files.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base::interface::scene::SceneSpec;
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use model::types::MessagesRequest;
use tokio_util::sync::CancellationToken;

const PARTS: usize = 8;

struct Scripted {
    rounds: Mutex<VecDeque<Vec<StreamEvent>>>,
}

impl AnthropicClient for Scripted {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        let events = self
            .rounds
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text_round("ok"));
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

fn round(
    block: ContentBlockStart,
    delta: BlockDelta,
    stop: base::message::StopReason,
) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: block,
        },
        StreamEvent::ContentBlockDelta { index: 0, delta },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(stop),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        },
    ]
}

fn text_round(text: &str) -> Vec<StreamEvent> {
    round(
        ContentBlockStart::Text {
            text: String::new(),
        },
        BlockDelta::TextDelta { text: text.into() },
        base::message::StopReason::EndTurn,
    )
}

fn read_round(id: &str, path: &Path) -> Vec<StreamEvent> {
    round(
        ContentBlockStart::ToolUse {
            id: id.into(),
            name: "Read".into(),
            input: serde_json::Value::Null,
        },
        BlockDelta::InputJsonDelta {
            partial_json: serde_json::json!({ "file_path": path }).to_string(),
        },
        base::message::StopReason::ToolUse,
    )
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

struct Host {
    sock: PathBuf,
    hook_log: PathBuf,
    _dir: tempfile::TempDir,
}

impl Host {
    fn hook_lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.hook_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

async fn start(pre_compact: &str) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("trim.sock");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let hook_log = dir.path().join("hooks.log");

    let body: String = (0..80)
        .map(|l| format!("line {l}: all values nominal\n"))
        .collect();
    let mut rounds = VecDeque::new();
    for i in 0..PARTS {
        let part = cwd.join(format!("part{i}.log"));
        std::fs::write(&part, &body).unwrap();
        rounds.push_back(read_round(&format!("p{i}"), &part));
    }
    rounds.push_back(text_round("read them all"));

    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    let settings = &mut config.settings.values;
    settings.memory.enabled = false;
    settings.compaction.reactive = true;
    settings.compaction.threshold_tokens = Some(60_000);
    settings.compaction.keep_recent = Some(2);
    let log = hook_log.display();
    settings.hooks = Some(serde_json::json!({
        "PreCompact": [{ "type": "command", "command": format!("echo pre >> '{log}'; {pre_compact}") }],
        "PostCompact": [{ "type": "command", "command": format!("echo post >> '{log}'") }],
    }));

    let code = scene::code::spec(scene::code::Interaction::Interactive);
    let mut scene = SceneSpec::new("trim", "Trim", code.prompt);
    scene.token_budget = code.token_budget;

    let store: Arc<dyn history::store::HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &cwd,
            history::path::HistoryRoots::under(dir.path()),
        )
        .await
        .unwrap(),
    );
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene),
        daemon::Assembly {
            cwd: Some(cwd),
            model_client: Some(Arc::new(Scripted {
                rounds: Mutex::new(rounds),
            })),
            transcripts: daemon::Transcripts::In(store),
            host_policy: Some(Arc::new(PermitAll)),
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
    assert!(sock.exists(), "socket never bound");
    Host {
        sock,
        hook_log,
        _dir: dir,
    }
}

async fn call(
    sock: &Path,
    method: &str,
    params: serde_json::Value,
) -> (Vec<serde_json::Value>, serde_json::Value) {
    crate::support::call_streaming(sock, method, params).await
}

async fn run_turn(
    sock: &Path,
    session_id: Option<&str>,
    message: &str,
) -> (String, Vec<serde_json::Value>) {
    let mut params = serde_json::json!({ "message": message });
    if let Some(id) = session_id {
        params["session_id"] = id.into();
    }
    let (frames, response) = tokio::time::timeout(
        Duration::from_secs(60),
        call(sock, "session.run_turn", params),
    )
    .await
    .expect("the turn never ended");
    assert!(response["result"].is_object(), "turn failed: {response}");
    let compacts = frames
        .iter()
        .map(|f| f["params"]["event"].clone())
        .filter(|e| e["kind"] == "compact")
        .collect();
    (
        response["result"]["session_id"]
            .as_str()
            .unwrap()
            .to_string(),
        compacts,
    )
}

async fn message_count(sock: &Path, session_id: &str) -> usize {
    let (_, response) = call(
        sock,
        "session.history",
        serde_json::json!({ "session_id": session_id, "limit": 500 }),
    )
    .await;
    response["result"]["messages"].as_array().map_or_else(
        || panic!("no messages in {response}"),
        |messages| messages.len(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trim_is_not_a_compaction_and_a_compaction_is() {
    let host = start("true").await;

    let (session, frames) = run_turn(&host.sock, None, "read the parts").await;
    let trim = frames
        .iter()
        .find(|f| f["strategy"] == "MicroCompact")
        .unwrap_or_else(|| panic!("no trim frame: {frames:?}"));
    assert_eq!(trim["trim"], true, "{trim}");
    assert_eq!(trim["messages_before"], trim["messages_after"], "{trim}");
    assert!(
        host.hook_lines().is_empty(),
        "a trim ran compaction hooks: {:?}",
        host.hook_lines()
    );

    let (_, frames) = run_turn(&host.sock, Some(&session), "/compact").await;
    let compact = frames
        .iter()
        .find(|f| f.get("trim").is_none())
        .unwrap_or_else(|| panic!("no compaction frame: {frames:?}"));
    assert!(
        compact["messages_after"].as_u64() < compact["messages_before"].as_u64(),
        "{compact}"
    );
    assert_eq!(host.hook_lines(), ["pre", "post"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_compaction_changes_nothing_and_says_nothing() {
    let host = start(r#"echo '{"decision":"block"}'"#).await;

    let (session, _) = run_turn(&host.sock, None, "read the parts").await;
    assert!(host.hook_lines().is_empty());
    let before = message_count(&host.sock, &session).await;

    let (_, frames) = run_turn(&host.sock, Some(&session), "/compact").await;
    assert_eq!(host.hook_lines(), ["pre"]);
    assert!(
        frames.iter().all(|f| f["trim"] == true),
        "a blocked compaction announced itself: {frames:?}"
    );
    assert_eq!(message_count(&host.sock, &session).await, before);
}
