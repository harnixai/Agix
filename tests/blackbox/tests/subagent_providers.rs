//! A sub-agent on a provider of its own under `model.order: ["ds", "cc"]`:
//! the host's `session.startAgent`, the model's `Agent` call and an agent
//! type each pin it to `cc` with `provider`, a model id only `cc` lists
//! takes it there without one, and a provider the pair does not define is
//! refused. Without either it runs where its parent does.

use std::time::Duration;

use blackbox::{Block, LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Reply};
use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};

const PARENT_MARK: &str = "PARENT-MARK";

fn ds_and_cc(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    let at = |id: &str| format!("{}{id}/", stub.base_url());
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .settings(json!({
            "providers": [
                {
                    "id": "ds",
                    "api": "anthropic",
                    "base_url": at("ds"),
                    "api_key_env": "ANTHROPIC_AUTH_TOKEN",
                    "tiers": {"strong": "ds-strong", "balanced": "ds-balanced", "fast": "ds-fast"},
                },
                {
                    "id": "cc",
                    "api": "anthropic",
                    "base_url": at("cc"),
                    "api_key_env": "ANTHROPIC_AUTH_TOKEN",
                    "tiers": {"strong": "cc-opus", "balanced": "cc-sonnet", "fast": "cc-haiku"},
                    "models": ["cc-opus", "cc-sonnet", "cc-haiku", "cc-special"],
                },
            ],
            "model": {"order": ["ds", "cc"]},
            "permissions": {"allow": ["Agent"]},
        }))
}

/// Where each of a sub-agent's calls carrying `mark` went: its provider and
/// model.
fn calls_carrying(stub: &ProviderStub, mark: &str) -> Vec<(String, String)> {
    stub.calls()
        .into_iter()
        .filter(|c| c.messages_contain(mark) && !c.messages_contain(PARENT_MARK))
        .map(|c| {
            let provider = c
                .path
                .trim_start_matches('/')
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            (provider, c.model().to_string())
        })
        .collect()
}

async fn start_agent(client: &mut DaemonRpcClient, params: Value) -> anyhow::Result<Value> {
    let response = client.call("session.startAgent", params).await?;
    anyhow::ensure!(response.error.is_none(), "{response:?}");
    Ok(response.result.unwrap_or_default())
}

/// The task `task_id` of `session`, once it is no longer running.
async fn settled(
    client: &mut DaemonRpcClient,
    session: &str,
    task_id: &str,
) -> anyhow::Result<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let listed = client
            .call("session.tasks", json!({ "session_id": session }))
            .await?
            .result
            .unwrap_or_default();
        let task = listed["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| t["task_id"] == task_id && t["status"] != "running")
            .cloned();
        if let Some(task) = task {
            return Ok(task);
        }
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "{listed}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Start a sub-agent with `extra` params, wait for it, and say where the
/// calls carrying its prompt went.
async fn run_started(
    run: &mut LiveRun,
    stub: &ProviderStub,
    session: &str,
    mark: &str,
    extra: Value,
) -> anyhow::Result<(Value, Vec<(String, String)>)> {
    let mut params = json!({ "session_id": session, "prompt": format!("{mark} review it") });
    if let (Some(p), Some(e)) = (params.as_object_mut(), extra.as_object()) {
        p.extend(e.clone());
    }
    let started = start_agent(&mut run.client, params).await?;
    let task_id = started["task_id"].as_str().unwrap_or_default().to_string();
    let task = settled(&mut run.client, session, &task_id).await?;
    assert_eq!(task["status"], "done", "{task}");
    Ok((task, calls_carrying(stub, mark)))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_started_agent_runs_on_the_provider_it_names() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = ds_and_cc("startagent-provider", &stub).start().await?;
    let session = run
        .session_with(json!({"options": {"auto_name": false}}))
        .await?;
    stub.script([Reply::text("ON-CC")]);
    let (_, calls) = run_started(
        &mut run,
        &stub,
        &session,
        "CHILD-PINNED",
        json!({"provider": "cc", "model": "cc-special"}),
    )
    .await?;
    assert_eq!(calls, [("cc".to_string(), "cc-special".to_string())]);

    stub.script([Reply::text("ON-DS")]);
    let (_, calls) = run_started(&mut run, &stub, &session, "CHILD-PLAIN", json!({})).await?;
    assert_eq!(calls, [("ds".to_string(), "ds-balanced".to_string())]);

    stub.script([Reply::text("ON-CC-BY-MODEL")]);
    let (_, calls) = run_started(
        &mut run,
        &stub,
        &session,
        "CHILD-BY-MODEL",
        json!({"model": "cc-special"}),
    )
    .await?;
    assert_eq!(calls, [("cc".to_string(), "cc-special".to_string())]);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_the_pair_does_not_define_is_refused() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = ds_and_cc("startagent-unknown-provider", &stub)
        .start()
        .await?;
    let session = run
        .session_with(json!({"options": {"auto_name": false}}))
        .await?;
    let response = run
        .client
        .call(
            "session.startAgent",
            json!({"session_id": session, "prompt": "CHILD-NOWHERE", "provider": "nope"}),
        )
        .await?;
    let error = response.error.expect("refused");
    assert_eq!(error.code, -32602, "{error:?}");
    assert!(error.message.contains("nope"), "{error:?}");
    let listed = run
        .client
        .call("session.tasks", json!({ "session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(listed["tasks"], json!([]), "{listed}");
    assert!(calls_carrying(&stub, "CHILD-NOWHERE").is_empty());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_models_agent_call_and_an_agent_type_pick_the_provider() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = ds_and_cc("agent-tool-provider", &stub)
        .before_start(|_world, project| {
            let dir = project.join(".agix/agents");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join("cross-checker.md"),
                "---\ndescription: checks on another vendor\nprovider: cc\n---\nCheck it.\n",
            )?;
            std::fs::write(
                dir.join("astray.md"),
                "---\ndescription: names no such provider\nprovider: nowhere\n---\nCheck it.\n",
            )?;
            Ok(())
        })
        .start()
        .await?;
    let session = run
        .session_with(json!({"options": {"auto_name": false}}))
        .await?;
    stub.script_when(
        PARENT_MARK,
        [
            Reply::Blocks(vec![Block::tool(
                "call-1",
                "Agent",
                json!({"prompt": "CHILD-BY-TOOL check", "description": "check", "provider": "cc"}),
            )]),
            Reply::Blocks(vec![Block::tool(
                "call-2",
                "Agent",
                json!({"prompt": "CHILD-BAD-TOOL check", "description": "check", "provider": "nope"}),
            )]),
            Reply::text("parent done"),
        ],
    );
    stub.script([Reply::text("TOOL-CHILD-ON-CC")]);
    let turn = run.turn(&session, "PARENT-MARK check twice").await?;
    assert!(turn.text.contains("parent done"), "{}", turn.text);
    assert_eq!(
        calls_carrying(&stub, "CHILD-BY-TOOL"),
        [("cc".to_string(), "cc-sonnet".to_string())]
    );
    assert!(calls_carrying(&stub, "CHILD-BAD-TOOL").is_empty());
    let refused = stub
        .calls()
        .into_iter()
        .rev()
        .find(|c| c.messages_contain(PARENT_MARK))
        .map(|c| c.messages_text())
        .unwrap_or_default();
    assert!(
        refused.contains("provider `nope` is not configured"),
        "{refused}"
    );

    stub.script([Reply::text("TYPED-ON-CC")]);
    let (_, calls) = run_started(
        &mut run,
        &stub,
        &session,
        "CHILD-TYPED",
        json!({"subagent_type": "cross-checker"}),
    )
    .await?;
    assert_eq!(calls, [("cc".to_string(), "cc-sonnet".to_string())]);

    stub.script([Reply::text("ASTRAY-ON-DS")]);
    let (_, calls) = run_started(
        &mut run,
        &stub,
        &session,
        "CHILD-ASTRAY",
        json!({"subagent_type": "astray"}),
    )
    .await?;
    assert_eq!(calls, [("ds".to_string(), "ds-balanced".to_string())]);
    run.finish().await;
    Ok(())
}
