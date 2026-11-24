//! The calls a session makes with thinking off — naming it, summarizing it
//! for compaction — go out in the shape the session's Claude model takes:
//! Anthropic's API refuses `{"type": "disabled"}` from Sonnet 5.5, and any
//! `thinking` that is not adaptive from Opus 5.5.

use std::time::{Duration, Instant};

use blackbox::{
    Daemon, DaemonOptions, ModelEndpoint, ProviderStub, Purpose, Reply, SeenRequest, World,
};
use serde_json::{json, Value};

const TITLE_NEEDLE: &str = "标题生成器";
const SUMMARY_NEEDLE: &str = "You are compacting an AI coding agent's conversation";

/// The stub, serving sessions under a model name of the case's choosing.
struct Named<'a> {
    stub: &'a ProviderStub,
    model: &'a str,
}

impl ModelEndpoint for Named<'_> {
    fn base_url(&self) -> url::Url {
        self.stub.base_url()
    }

    fn model(&self) -> &str {
        self.model
    }
}

/// Every request of a `harnix` session on `model` — the scene that names
/// its sessions — after a few turns and a `session.compact`.
async fn calls_of(model: &str) -> anyhow::Result<Vec<SeenRequest>> {
    let stub = ProviderStub::start().await?;
    let world = World::new()?;
    let project = world.project("thinking-off-wire")?;
    let daemon = Daemon::start(
        &world,
        &Named { stub: &stub, model },
        DaemonOptions::new(project.clone()).scene("harnix"),
    )
    .await?;
    let mut client = daemon.connect().await?;
    stub.script_when(TITLE_NEEDLE, (0..4).map(|_| Reply::text("a title")));
    stub.script_when(SUMMARY_NEEDLE, [Reply::text("SUMMARY: three questions.")]);
    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    for i in 0..3 {
        stub.script([Reply::text(format!(
            "answer {i} {}",
            "padding ".repeat(200)
        ))]);
        client
            .session_run_turn(
                &session,
                &format!("question {i} {}", "context ".repeat(200)),
                &format!("t{i}"),
                None,
            )
            .await?;
    }
    let compacted = client
        .call("session.compact", json!({ "session_id": session }))
        .await?;
    anyhow::ensure!(compacted.error.is_none(), "{compacted:?}");

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let calls = stub.calls();
        let purposes: Vec<Purpose> = calls.iter().map(SeenRequest::purpose).collect();
        if purposes.contains(&Purpose::Title) && purposes.contains(&Purpose::Compact) {
            return Ok(calls);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "no naming and compaction call among {purposes:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn side(calls: &[SeenRequest]) -> Vec<&SeenRequest> {
    calls
        .iter()
        .filter(|c| matches!(c.purpose(), Purpose::Title | Purpose::Compact))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn sonnet_5_5_never_gets_thinking_disabled() -> anyhow::Result<()> {
    let calls = calls_of("claude-sonnet-5-5").await?;
    for call in &calls {
        assert_ne!(
            call.body.pointer("/thinking/type"),
            Some(&json!("disabled")),
            "{:?} call: {}",
            call.purpose(),
            call.body
        );
    }
    for call in side(&calls) {
        assert_eq!(
            call.body["thinking"],
            json!({"type": "between_tools"}),
            "{:?} call: {}",
            call.purpose(),
            call.body
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn opus_5_5_side_calls_send_no_thinking_and_a_low_effort() -> anyhow::Result<()> {
    let calls = calls_of("claude-opus-5-5").await?;
    for call in side(&calls) {
        assert_eq!(
            call.body.get("thinking"),
            None::<&Value>,
            "{:?} call: {}",
            call.purpose(),
            call.body
        );
        assert_eq!(
            call.body.pointer("/output_config/effort"),
            Some(&json!("low")),
            "{:?} call: {}",
            call.purpose(),
            call.body
        );
    }
    Ok(())
}
