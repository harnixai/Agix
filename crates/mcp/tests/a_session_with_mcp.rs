//! MCP as the kernel sees it: a session assembled with [`McpModule`] offers
//! the servers' tools, runs their prompts as slash commands and inlines their
//! resources.
//!
//! [`McpModule`]: mcp::McpModule

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use async_trait::async_trait;
use base::interface::model::ThinkingMode;
use base::interface::model::{Model, ModelContentBlock, ModelMessage};
use base::interface::scene::SceneSpec;
use base::paths::PathSettings;
use mcp::manager::McpManager;
use runtime::agent::Builder;
use settings::*;

struct DummyModel;

#[async_trait]
impl Model for DummyModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        Err(base::interface::model::ModelError::protocol(
            "DummyModel does not stream",
        ))
    }
}

/// Answers every request directly and keeps the text of every message it
/// was sent.
struct RecordingAnswerModel {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl Model for RecordingAnswerModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<base::prompt::PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        messages: Vec<ModelMessage>,
        _params: base::interface::model::StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        let mut seen = self.seen.lock().unwrap();
        for m in &messages {
            for b in &m.content {
                if let ModelContentBlock::Text { text } = b {
                    seen.push(text.clone());
                }
            }
        }
        let events = vec![
            Ok(base::interface::model::ModelEvent::TextDelta {
                text: "answering directly".into(),
            }),
            Ok(base::interface::model::ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Default::default(),
            }),
        ];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

fn recall_test_settings(root: &std::path::Path) -> settings::LoadedSettings {
    let mut settings = Settings::defaults_for("test-model");
    settings.model.max_tokens = 4096;
    settings.model.thinking_mode = ThinkingMode::Off;
    settings.model.effort = None;
    settings.telemetry.enabled = false;
    let mut paths = PathSettings::default();
    paths.user_data_dir = root.join("user-data");
    paths.global_data_dir = root.join("global-data");
    paths.workspace = base::paths::Workspace::project_of(&root.join("local-data"));
    paths.local_data_dir = root.join("local-data");
    paths.scope = "code".into();
    settings::LoadedSettings::new(settings, paths)
}

fn test_settings() -> settings::LoadedSettings {
    recall_test_settings(&std::env::temp_dir().join("agix-mcp-module-tests"))
}

// `McpManager::all_prompts` / `list_resources` /
// `read_resource` must be reachable from a session, not only MCP *tools*.
// These drive a whole turn through `run_turn`, so they cover the
// registration in `Builder::build`, the slash-command interception, and
// the pre-push `@`-reference resolution together.

#[tokio::test]
async fn build_registers_mcp_tool_adapters_into_the_executable_tool_registry() {
    // `build_tool_defs()` (`agent/turn/request.rs`) advertises every `mcp.tool_adapters()`
    // entry to the model as a callable tool, so `Builder::build()` must
    // register each per-tool adapter, not just the 3 generic List/Read/Dispatch
    // MCP tools — `agent/turn/tool_exec.rs::execute_tool_inner` looks up `ctx.tools`, a clone
    // of this same registry, and would answer "Tool not found".
    let settings = Arc::new(test_settings());
    let model: Arc<dyn Model> = Arc::new(DummyModel);
    let scene: Arc<SceneSpec> = Arc::new(scene::code::spec(scene::code::Interaction::Interactive));

    let mock_client = Arc::new(mcp::client::MockMcpClient::new(
        "test-server",
        vec![mcp::client::McpToolMeta {
            name: "do-thing".into(),
            description: Some("does a thing".into()),
            input_schema: serde_json::json!({"type": "object"}),
            hints: Default::default(),
        }],
    ));
    let mut mcp_manager = McpManager::from_clients(vec![mock_client]).await;
    mcp_manager.refresh_tools().await;
    assert_eq!(
        mcp_manager.tool_adapters().len(),
        1,
        "sanity check: mock client should have produced one adapter"
    );

    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(settings)
        .module(Arc::new(mcp::McpModule::new(mcp_manager)))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    assert!(
        agent.tools().get("mcp__test-server__do-thing").is_some(),
        "MCP tool adapter should be registered in the executable tool registry, found: {:?}",
        agent.tools().names()
    );
}

fn prompt_arg(name: &str, required: bool) -> mcp::client::McpPromptArg {
    mcp::client::McpPromptArg {
        name: name.into(),
        description: None,
        required: Some(required),
    }
}

/// A mock MCP server exposing one prompt (two required args) and one
/// readable resource.
async fn mcp_manager_for_turn_tests(down: bool) -> McpManager {
    let client = Arc::new(
        mcp::client::MockMcpClient::new("github", Vec::new())
            .with_prompts(vec![mcp::client::McpPromptMeta {
                name: "review_pr".into(),
                description: Some("Review a pull request".into()),
                arguments: vec![prompt_arg("repo", true), prompt_arg("pr", true)],
            }])
            .with_prompt_body(
                "review_pr",
                "PROMPT-BODY: review PR {pr} in {repo} thoroughly.",
            )
            .with_resource_text("doc://guide/install", "RESOURCE-BODY: run cargo install."),
    );
    let mut mgr = McpManager::from_clients(vec![client]).await;
    mgr.refresh_prompts().await;
    if down {
        // Collect the prompt list while the server is up, *then* knock it
        // over — that is what a mid-session disconnect actually looks
        // like, and the case where the command is still offered but the
        // invocation must degrade rather than fail.
        let prompts = mgr.all_prompts().to_vec();
        let dead = Arc::new(mcp::client::MockMcpClient::new("github", Vec::new()).down());
        let mut dead_mgr = McpManager::from_clients(vec![dead]).await;
        dead_mgr.set_prompts_for_test(prompts);
        return dead_mgr;
    }
    mgr
}

async fn run_one_turn(
    mcp_manager: McpManager,
    input: &str,
) -> (runtime::turn::TurnOutcome, String, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model: Arc<dyn base::interface::model::Model> =
        Arc::new(RecordingAnswerModel { seen: seen.clone() });
    let scene: Arc<base::interface::scene::SceneSpec> =
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive));
    let mut settings = recall_test_settings(tmp.path());
    // Keep the transcript to just what this test injects.
    settings.values.memory.enabled = false;

    let (mut agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(Arc::new(settings))
        .module(Arc::new(mcp::McpModule::new(mcp_manager)))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let outcome = agent
        .run_turn(
            input.into(),
            "t1".into(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("a broken MCP server must never fail the turn");
    let transcript = seen.lock().unwrap().join("\n");
    (outcome, transcript, tmp)
}

#[tokio::test]
async fn mcp_prompt_is_registered_as_a_slash_command() {
    let tmp = tempfile::tempdir().unwrap();
    let model: Arc<dyn base::interface::model::Model> = Arc::new(RecordingAnswerModel {
        seen: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let scene: Arc<base::interface::scene::SceneSpec> =
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive));
    let (agent, _event_rx, _input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(Arc::new(recall_test_settings(tmp.path())))
        .module(Arc::new(mcp::McpModule::new(
            mcp_manager_for_turn_tests(false).await,
        )))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let cmd = agent
        .commands()
        .resolve("mcp__github__review_pr")
        .expect("MCP prompt should be a slash command");
    assert_eq!(cmd.description(), "Review a pull request");
    assert!(agent
        .commands()
        .list()
        .iter()
        .any(|(n, _)| *n == "mcp__github__review_pr"));
}

#[tokio::test]
async fn invoking_an_mcp_prompt_command_injects_the_servers_messages() {
    let (outcome, transcript, _tmp) = run_one_turn(
        mcp_manager_for_turn_tests(false).await,
        "/mcp__github__review_pr repo=acme/widgets pr=42",
    )
    .await;
    assert_eq!(outcome.stop_reason, "end_turn");
    assert!(
        transcript.contains("PROMPT-BODY: review PR 42 in acme/widgets thoroughly."),
        "the server's prompt messages never reached the model: {transcript}"
    );
}

#[tokio::test]
async fn a_missing_required_prompt_argument_surfaces_a_clear_error() {
    let (outcome, transcript, _tmp) = run_one_turn(
        mcp_manager_for_turn_tests(false).await,
        "/mcp__github__review_pr repo=acme/widgets",
    )
    .await;
    assert_eq!(outcome.stop_reason, "end_turn", "the turn must not fail");
    assert!(
        transcript.contains("missing required argument: pr"),
        "no clear argument error reached the model: {transcript}"
    );
    assert!(
        !transcript.contains("PROMPT-BODY"),
        "the prompt must not have been called at all: {transcript}"
    );
}

#[tokio::test]
async fn a_down_mcp_server_degrades_to_visible_text_without_failing_the_turn() {
    let (outcome, transcript, _tmp) = run_one_turn(
        mcp_manager_for_turn_tests(true).await,
        "/mcp__github__review_pr repo=acme/widgets pr=42",
    )
    .await;
    assert_eq!(outcome.stop_reason, "end_turn");
    assert!(
        transcript.contains("could not be run") && transcript.contains("is not connected"),
        "the failure was not surfaced as text the model can explain: {transcript}"
    );
}

#[tokio::test]
async fn an_at_reference_inlines_the_resource_into_the_user_message() {
    let (_outcome, transcript, _tmp) = run_one_turn(
        mcp_manager_for_turn_tests(false).await,
        "summarise @github:doc://guide/install for me",
    )
    .await;
    assert!(
        transcript.contains("RESOURCE-BODY: run cargo install."),
        "the resource was never inlined: {transcript}"
    );
    assert!(transcript.contains("<mcp-resource server=\"github\""));
    // The original reference stays put so the model can correlate.
    assert!(transcript.contains("summarise @github:doc://guide/install for me"));
}

#[tokio::test]
async fn at_signs_that_are_not_resource_references_never_trigger_a_fetch() {
    for text in [
        "email me at xbitshans@gmail.com when done",
        "why does @Component({ selector: 'x' }) fail here?",
        "fix fn f<'a>(x: &'a str) -> &'a str { x }",
        "run npm i @anthropic-ai/sdk @types/node",
    ] {
        let (_outcome, transcript, _tmp) =
            run_one_turn(mcp_manager_for_turn_tests(false).await, text).await;
        assert!(
            !transcript.contains("<mcp-resources>"),
            "false-positive `@` fetch on {text:?}: {transcript}"
        );
    }
}

#[tokio::test]
async fn an_unresolvable_at_reference_degrades_to_visible_text() {
    let (outcome, transcript, _tmp) = run_one_turn(
        mcp_manager_for_turn_tests(false).await,
        "read @github:doc://guide/does-not-exist",
    )
    .await;
    assert_eq!(outcome.stop_reason, "end_turn", "the turn must not fail");
    assert!(
        transcript.contains("error=\"true\"") && transcript.contains("no such resource"),
        "the bad URI was not surfaced as visible text: {transcript}"
    );
}
