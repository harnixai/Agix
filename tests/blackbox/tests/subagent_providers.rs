//! A sub-agent on a provider of its own under `model.order: ["ds", "cc"]`:
//! the host's `session.startAgent`, the model's `Agent` call and an agent
//! type each pin it to `cc` with `provider`, a model id only `cc` lists
//! takes it there without one, and a provider the pair does not define is
//! refused. Without either it runs where its parent does. Its task, and
//! `session.get` on its sidechain, name the model and provider its calls
//! went to, corrected by `task_updated` when a call falls back. A provider
//! outside the session's order is the host's alone to name; an untrusted
//! project's agent type names none; a live team member keeps its own.

use std::time::Duration;

use blackbox::{Block, LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Reply};
use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};

const PARENT_MARK: &str = "PARENT-MARK";

fn ds_and_cc(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    stubbed(name, stub).settings(two_providers(stub, &["ds", "cc"]))
}

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

fn two_providers(stub: &ProviderStub, order: &[&str]) -> Value {
    let at = |id: &str| format!("{}{id}/", stub.base_url());
    json!({
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
            "model": {"order": order},
            "permissions": {"allow": ["Agent"]},
    })
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

/// What a sub-agent's task says its calls go to.
fn placed(task: &Value) -> (String, String) {
    let field = |name: &str| task["detail"][name].as_str().unwrap_or("").to_string();
    (field("provider"), field("model"))
}

async fn sidechain_placed(client: &mut DaemonRpcClient, task: &Value) -> anyhow::Result<Value> {
    let got = client
        .call(
            "session.get",
            json!({"session_id": task["detail"]["agent_session_id"]}),
        )
        .await?;
    let got = got.result.unwrap_or_default();
    Ok(json!([got["provider"], got["model"]]))
}

/// The next `session.event` whose event satisfies `wanted`, skipping the rest.
async fn event_where(
    watcher: &mut DaemonRpcClient,
    wanted: impl Fn(&Value) -> bool,
) -> anyhow::Result<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(left, watcher.next_frame())
            .await
            .map_err(|_| anyhow::anyhow!("no such event within 20s"))??
            .ok_or_else(|| anyhow::anyhow!("the connection closed"))?;
        if frame["method"] == "session.event" && wanted(&frame["params"]["event"]) {
            return Ok(frame["params"]["event"].clone());
        }
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
    let (task, calls) = run_started(
        &mut run,
        &stub,
        &session,
        "CHILD-PINNED",
        json!({"provider": "cc", "model": "cc-special"}),
    )
    .await?;
    assert_eq!(calls, [("cc".to_string(), "cc-special".to_string())]);
    assert_eq!(placed(&task), calls[0], "{task}");
    assert_eq!(
        sidechain_placed(&mut run.client, &task).await?,
        json!(["cc", "cc-special"])
    );

    stub.script([Reply::text("ON-DS")]);
    let (task, calls) = run_started(&mut run, &stub, &session, "CHILD-PLAIN", json!({})).await?;
    assert_eq!(calls, [("ds".to_string(), "ds-balanced".to_string())]);
    assert_eq!(placed(&task), calls[0], "{task}");

    stub.script([Reply::text("ON-CC-BY-MODEL")]);
    let (task, calls) = run_started(
        &mut run,
        &stub,
        &session,
        "CHILD-BY-MODEL",
        json!({"model": "cc-special"}),
    )
    .await?;
    assert_eq!(calls, [("cc".to_string(), "cc-special".to_string())]);
    assert_eq!(placed(&task), calls[0], "{task}");
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

    let listed = run
        .client
        .call("session.tasks", json!({ "session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    let by_tool = listed["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["detail"]["tool_use_id"] == "call-1")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        placed(&by_tool),
        ("cc".to_string(), "cc-sonnet".to_string()),
        "{listed}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_whose_provider_fails_is_reported_where_its_calls_went() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = ds_and_cc("subagent-fallback", &stub).start().await?;
    let session = run
        .session_with(json!({"options": {"auto_name": false}}))
        .await?;
    let mut watcher = run.daemon.connect().await?;
    watcher.session_subscribe(&session).await?;
    stub.inject_failures_when(
        "\"model\":\"ds-balanced\"",
        10,
        Reply::status(
            501,
            r#"{"type":"error","error":{"type":"server_error","message":"down"}}"#,
        ),
    );
    stub.script([Reply::text("SERVED-BY-CC")]);

    let started = start_agent(
        &mut run.client,
        json!({"session_id": session, "prompt": "CHILD-FALLS-BACK review it"}),
    )
    .await?;
    let begun = event_where(&mut watcher, |e| e["kind"] == "task_started").await?;
    assert_eq!(begun["task"]["task_id"], started["task_id"], "{begun}");
    assert_eq!(
        placed(&begun["task"]),
        ("ds".to_string(), "ds-balanced".to_string()),
        "the prediction: {begun}"
    );
    let moved = event_where(&mut watcher, |e| {
        e["kind"] == "task_updated" && e["task"]["detail"]["provider"] == "cc"
    })
    .await?;
    assert_eq!(
        placed(&moved["task"]),
        ("cc".to_string(), "cc-sonnet".to_string()),
        "{moved}"
    );

    let task_id = started["task_id"].as_str().unwrap_or_default();
    let task = settled(&mut run.client, &session, task_id).await?;
    let went: Vec<_> = calls_carrying(&stub, "CHILD-FALLS-BACK")
        .into_iter()
        .filter(|(provider, _)| provider == "cc")
        .collect();
    assert_eq!(went, [("cc".to_string(), "cc-sonnet".to_string())]);
    assert_eq!(placed(&task), went[0], "{task}");
    assert_eq!(
        sidechain_placed(&mut run.client, &task).await?,
        json!(["cc", "cc-sonnet"])
    );
    run.finish().await;
    Ok(())
}

fn agent_type(project: &std::path::Path, name: &str, provider: &str) -> anyhow::Result<()> {
    let dir = project.join(".agix/agents");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join(format!("{name}.md")),
        format!(
            "---\ndescription: checks on another vendor\nprovider: {provider}\n---\nCheck it.\n"
        ),
    )?;
    Ok(())
}

/// The `Agent` call's answer in `turn`: its text, and whether it was an error.
fn agent_answer(turn: &rpc_client::TurnEvents) -> (String, bool) {
    turn.tool_results
        .iter()
        .find(|(name, _, _)| name == "Agent")
        .map(|(_, text, error)| (text.clone(), *error))
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_host_names_a_provider_outside_the_order() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("provider-outside-order", &stub)
        .settings(two_providers(&stub, &["ds"]))
        .before_start(|_world, project| agent_type(project, "cross-checker", "cc"))
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
                json!({"prompt": "CHILD-OUTSIDE check", "description": "check", "provider": "cc"}),
            )]),
            Reply::text("parent done"),
        ],
    );
    stub.script([Reply::text("MODEL-CHILD-ON-CC")]);
    let turn = run.turn(&session, "PARENT-MARK check on cc").await?;
    let (answer, is_error) = agent_answer(&turn);
    assert!(
        is_error && answer.contains("not in this session's model.order"),
        "{answer}"
    );
    assert!(calls_carrying(&stub, "CHILD-OUTSIDE").is_empty());

    stub.script([Reply::text("TYPED-ON-DS")]);
    let (_, calls) = run_started(
        &mut run,
        &stub,
        &session,
        "CHILD-TYPED",
        json!({"subagent_type": "cross-checker"}),
    )
    .await?;
    assert_eq!(calls, [("ds".to_string(), "ds-balanced".to_string())]);

    stub.script([Reply::text("HOST-ON-CC")]);
    let (_, calls) = run_started(
        &mut run,
        &stub,
        &session,
        "CHILD-HOST",
        json!({"provider": "cc"}),
    )
    .await?;
    assert_eq!(calls, [("cc".to_string(), "cc-sonnet".to_string())]);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_projects_agent_type_names_no_provider() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let providers = two_providers(&stub, &["ds", "cc"]);
    let mut run = stubbed("untrusted-type-provider", &stub)
        .untrusted()
        .before_start(move |world, project| {
            world.write_global_settings(&providers)?;
            agent_type(project, "x", "cc")
        })
        .start()
        .await?;
    let status = run
        .client
        .call(
            "project.status",
            json!({ "project_root": run.project.to_string_lossy() }),
        )
        .await?
        .result
        .unwrap_or_default();
    assert!(
        status["requires_trust"]
            .as_array()
            .is_some_and(|needs| needs.contains(&json!("agents/x"))),
        "{status}"
    );
    let session = run
        .session_with(json!({"options": {"auto_name": false}}))
        .await?;
    stub.script([Reply::text("UNTRUSTED-ON-DS")]);
    let (_, calls) = run_started(
        &mut run,
        &stub,
        &session,
        "CHILD-UNTRUSTED",
        json!({"subagent_type": "x"}),
    )
    .await?;
    assert_eq!(calls, [("ds".to_string(), "ds-balanced".to_string())]);

    let trusted = run
        .client
        .call(
            "project.trust",
            json!({ "project_root": run.project.to_string_lossy(), "trusted": true }),
        )
        .await?;
    anyhow::ensure!(trusted.error.is_none(), "{:?}", trusted.error);
    let session = run
        .session_with(json!({"options": {"auto_name": false}}))
        .await?;
    stub.script([Reply::text("TRUSTED-ON-CC")]);
    let (_, calls) = run_started(
        &mut run,
        &stub,
        &session,
        "CHILD-TRUSTED",
        json!({"subagent_type": "x"}),
    )
    .await?;
    assert_eq!(calls, [("cc".to_string(), "cc-sonnet".to_string())]);
    run.finish().await;
    Ok(())
}

fn message_worker(id: &str, prompt: &str, extra: Value) -> Reply {
    let mut input = json!({
        "prompt": prompt, "description": "worker", "team_name": "crew", "name": "worker",
    });
    if let (Some(i), Some(e)) = (input.as_object_mut(), extra.as_object()) {
        i.extend(e.clone());
    }
    Reply::Blocks(vec![Block::tool(id, "Agent", input)])
}

/// A `session.tasks` listing taken while a `teammate` task is running.
async fn while_a_message_runs(
    client: &mut DaemonRpcClient,
    session: &str,
) -> anyhow::Result<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let listed = client
            .call("session.tasks", json!({ "session_id": session }))
            .await?
            .result
            .unwrap_or_default();
        let tasks = listed["tasks"].as_array().cloned().unwrap_or_default();
        let teammates = tasks.iter().filter(|t| t["kind"] == "teammate");
        if teammates.clone().any(|t| t["status"] == "running") {
            return Ok(listed);
        }
        anyhow::ensure!(
            teammates.count() == 0 && tokio::time::Instant::now() < deadline,
            "the message was never seen running: {listed}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A live member is `running` while one of its messages is, and keeps the
/// provider it was started on.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_member_keeps_its_provider() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut settings = two_providers(&stub, &["ds", "cc"]);
    settings["team"] = json!({"enabled": true});
    let mut run = stubbed("member-provider", &stub)
        .settings(settings)
        .start()
        .await?;
    let session = run
        .session_with(json!({"options": {"auto_name": false}}))
        .await?;
    stub.script_when(
        PARENT_MARK,
        [
            message_worker("w-1", "MEMBER-FIRST hello", json!({})),
            Reply::text("sent"),
        ],
    );
    stub.script_when(
        "MEMBER-FIRST",
        [Reply::delayed(
            Duration::from_secs(3),
            Reply::text("first done"),
        )],
    );
    run.turn(&session, "PARENT-MARK message the worker").await?;
    let busy = while_a_message_runs(&mut run.client, &session).await?;
    let member = busy["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["kind"] == "member")
        .cloned()
        .unwrap_or_default();
    assert_eq!(member["status"], "running", "{busy}");
    run.wait_tasks_settled(&session, Duration::from_secs(30))
        .await?;
    let first: Vec<_> = calls_carrying(&stub, "MEMBER-FIRST")
        .into_iter()
        .map(|(provider, _)| provider)
        .collect();
    assert_eq!(first, ["ds"]);

    stub.script_when(
        PARENT_MARK,
        [
            message_worker("w-2", "MEMBER-SECOND again", json!({"provider": "cc"})),
            Reply::text("sent again"),
        ],
    );
    let turn = run.turn(&session, "message it on cc").await?;
    let (answer, is_error) = agent_answer(&turn);
    assert!(
        is_error && answer.contains("already runs on provider `ds`"),
        "{answer}"
    );
    run.wait_tasks_settled(&session, Duration::from_secs(30))
        .await?;
    assert!(calls_carrying(&stub, "MEMBER-SECOND").is_empty());
    run.finish().await;
    Ok(())
}
