//! A reply cut off at `max_tokens` is retried as a conversation that ends
//! with the user asking it to go on, at a limit the model accepts.

use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use model::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use tokio_util::sync::CancellationToken;

const MODEL: &str = "claude-opus-4-1";
const CONFIGURED_MAX_TOKENS: u32 = 4096;
/// What Anthropic documents as the most this model writes in one reply.
const MODEL_OUTPUT_LIMIT: u32 = 32_000;

fn reply(text: &str, stop: base::message::StopReason) -> Vec<StreamEvent> {
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
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(stop),
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

async fn rpc(sock: &std::path::Path, method: &str, params: serde_json::Value) -> serde_json::Value {
    crate::support::call(sock, method, params).await
}

/// The request that retried a reply cut off at `max_tokens`, in a project
/// whose `.agix/settings.json` is `project_settings`.
async fn the_retry(project_settings: Option<serde_json::Value>) -> model::types::MessagesRequest {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("recovery.sock");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(cwd.join(".agix")).unwrap();
    if let Some(settings) = project_settings {
        std::fs::write(cwd.join(".agix/settings.json"), settings.to_string()).unwrap();
    }

    let model = Arc::new(MockAnthropicClient::new());
    model.push_turn(reply(
        "the first half of the answer\n",
        base::message::StopReason::MaxTokens,
    ));
    model.push_turn(reply("and the rest", base::message::StopReason::EndTurn));

    let mut config = load_daemon_config(
        MODEL,
        CONFIGURED_MAX_TOKENS,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.settings.values.memory.enabled = false;
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
    tokio::spawn(async move {
        let _ = serving.serve_unix(&bind).await;
    });
    for _ in 0..80 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let session = rpc(
        &sock,
        "session.create",
        serde_json::json!({"project_root": cwd}),
    )
    .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let turn = rpc(
        &sock,
        "session.run_turn",
        serde_json::json!({"session_id": session, "message": "write it all"}),
    )
    .await;
    assert_eq!(turn["api_calls"], 2, "{turn}");
    model.nth_request(1).expect("the cut-off reply was retried")
}

#[tokio::test]
async fn a_reply_cut_off_at_max_tokens_is_continued_by_a_user_message_within_the_models_limit() {
    let retry = the_retry(None).await;
    let body = serde_json::to_value(&retry).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let last = messages.last().unwrap();
    assert_eq!(
        last["role"], "user",
        "the retry must not end on the cut-off reply as a prefill: {messages:?}"
    );
    assert!(
        last.to_string().contains("cut off"),
        "the user message says the reply was cut off: {last}"
    );
    let before = &messages[messages.len() - 2];
    assert_eq!(before["role"], "assistant", "{messages:?}");
    assert!(before.to_string().contains("the first half"), "{before}");
    assert!(
        retry.max_tokens > CONFIGURED_MAX_TOKENS,
        "the limit is raised: {}",
        retry.max_tokens
    );
    assert!(
        retry.max_tokens <= MODEL_OUTPUT_LIMIT,
        "{} is past what {MODEL} accepts",
        retry.max_tokens
    );
}
