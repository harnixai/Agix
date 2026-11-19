//! A command hook starts its process on the execution layer the host gave the
//! session, not on this machine behind its back.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base::interface::exec::{ExecError, Process, ProcessHandle, ProcessSpec};
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio_util::sync::CancellationToken;

struct WatchedProcess(Arc<Mutex<Vec<String>>>);

#[async_trait::async_trait]
impl Process for WatchedProcess {
    async fn spawn(
        &self,
        spec: ProcessSpec,
        cancel: CancellationToken,
    ) -> Result<Box<dyn ProcessHandle>, ExecError> {
        let mut line = vec![spec.program.clone()];
        line.extend(spec.args.iter().cloned());
        self.0.lock().unwrap().push(line.join(" "));
        platform::LocalProcess::default().spawn(spec, cancel).await
    }
}

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

async fn call(
    sock: &std::path::Path,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    crate::support::call(sock, method, params).await
}

#[tokio::test]
async fn a_command_hook_runs_on_the_execution_layer_the_host_swapped_in() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let global = dir.path().join("agix");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(
        global.join("settings.json"),
        serde_json::json!({
            "hooks": {"UserPromptSubmit": [
                {"type": "command", "command": "echo hook-marker-7f3a"}
            ]},
        })
        .to_string(),
    )
    .unwrap();

    let sock = dir.path().join("agix.sock");
    let paths = StaticDaemonPaths::with_project(global.clone(), work.clone()).with_global(global);
    let mut config = load_daemon_config("claude-sonnet-4-6", None, Some(&sock), "code", &paths);
    config.settings.values.memory.enabled = false;
    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(one_turn());
    let spawned: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = spawned.clone();
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(work),
            model_client: Some(model as Arc<dyn AnthropicClient>),
            transcripts: daemon::Transcripts::Nowhere,
            customize_session: Some(Arc::new(move |builder, _ctx| {
                let mut exec = platform::local_providers();
                exec.process = Arc::new(WatchedProcess(seen.clone()));
                builder.exec(exec)
            })),
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

    let sid = call(&sock, "session.create", serde_json::json!({})).await["session_id"].clone();
    call(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": sid, "message": "hello"}),
    )
    .await;

    for _ in 0..80 {
        if spawned
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.contains("hook-marker-7f3a"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let spawned = spawned.lock().unwrap();
    assert!(
        spawned.iter().any(|c| c.contains("hook-marker-7f3a")),
        "the hook's command never reached the host's process layer: {spawned:?}"
    );
}
