//! A sub-agent cannot put a session into plan mode: plan mode is the
//! conversation's, entered by the model the user is talking to.

#![cfg(feature = "agents")]

use crate::support::call;
use crate::support::{text_round, tool_round};
use std::sync::Arc;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_calling_enter_plan_mode_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("agix");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let sock = dir.path().join("agix.sock");
    let paths = StaticDaemonPaths::with_project(global.clone(), work.clone()).with_global(global);
    let mut config = load_daemon_config("claude-sonnet-4-6", None, Some(&sock), "code", &paths);
    config.settings.values.memory.enabled = false;
    let model = Arc::new(MockAnthropicClient::new());
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(work),
            model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
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

    model.push_turn(tool_round(
        "Agent",
        "toolu_agent",
        json!({"description": "plan it", "prompt": "plan it",
               "subagent_type": "general-purpose"}),
    ));
    model.push_turn(tool_round("EnterPlanMode", "toolu_plan", json!({})));
    model.push_turn(text_round("planned"));
    model.push_turn(text_round("done"));
    call(&sock, "session.run_turn", json!({"message": "delegate"})).await;

    let answered: Vec<Value> = (0..model.calls())
        .filter_map(|n| model.nth_request(n))
        .flat_map(|r| {
            serde_json::to_value(&r).unwrap()["messages"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["type"] == "tool_result" && b["tool_use_id"] == "toolu_plan")
        .collect();
    assert!(
        !answered.is_empty(),
        "the sub-agent's call was never answered"
    );
    let answer = answered[0].to_string();
    assert!(
        answered[0]["is_error"] == true && answer.contains("sub-agent"),
        "a sub-agent entered plan mode: {answer}"
    );
}
