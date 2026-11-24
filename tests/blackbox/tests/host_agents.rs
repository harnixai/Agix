//! A sub-agent the host starts under a session (`session.startAgent`): a task
//! of the session like one the model starts, reported with the host's tag,
//! its own usage and its structured output, held to the session's limits and
//! permissions and to a token budget of its own when given one, and able to
//! ask the host while its parent is idle — its task showing as waiting until
//! the host answers.

use std::time::Duration;

use blackbox::{Block, Daemon, DaemonOptions, ProviderStub, Reply, World};
use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};

async fn daemon_with(
    name: &str,
    settings: Value,
) -> anyhow::Result<(World, ProviderStub, Daemon, DaemonRpcClient, String)> {
    let world = World::new()?;
    let project = world.project(name)?;
    let mut base = json!({ "memory": { "enabled": false } });
    if let (Some(base), Some(extra)) = (base.as_object_mut(), settings.as_object()) {
        base.extend(extra.clone());
    }
    world.write_project_settings(name, &base)?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    Ok((world, provider, daemon, client, session))
}

/// The next `session.event` whose event satisfies `wanted`, skipping the rest.
async fn event_where(
    watcher: &mut DaemonRpcClient,
    wanted: impl Fn(&Value) -> bool,
) -> anyhow::Result<Value> {
    let mut seen = events_until(watcher, wanted).await?;
    Ok(seen.pop().unwrap_or_default())
}

/// Every `session.event` up to and including the first whose event satisfies
/// `last`.
async fn events_until(
    watcher: &mut DaemonRpcClient,
    last: impl Fn(&Value) -> bool,
) -> anyhow::Result<Vec<Value>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(left, watcher.next_frame())
            .await
            .map_err(|_| anyhow::anyhow!("no such event within 20s"))??
            .ok_or_else(|| anyhow::anyhow!("the connection closed"))?;
        if frame["method"] != "session.event" {
            continue;
        }
        let done = last(&frame["params"]["event"]);
        seen.push(frame["params"].clone());
        if done {
            return Ok(seen);
        }
    }
}

async fn start_agent(client: &mut DaemonRpcClient, params: Value) -> anyhow::Result<Value> {
    let response = client.call("session.startAgent", params).await?;
    anyhow::ensure!(response.error.is_none(), "{response:?}");
    Ok(response.result.unwrap_or_default())
}

/// The task `started` reported, once it is no longer running.
async fn settled(
    client: &mut DaemonRpcClient,
    session: &str,
    started: &Value,
) -> anyhow::Result<Value> {
    let task_id = started["task_id"].as_str().unwrap_or_default();
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

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_started_on_an_idle_session_runs_as_its_task() -> anyhow::Result<()> {
    let (_world, provider, daemon, mut client, session) = daemon_with("idle", json!({})).await?;
    let mut watcher = daemon.connect().await?;
    watcher.session_subscribe(&session).await?;
    provider.script([Reply::text("REVIEWED-OK")]);

    let started = start_agent(
        &mut client,
        json!({
            "session_id": session,
            "prompt": "review the change",
            "description": "review",
            "tag": "flow:run-1/review/t1"
        }),
    )
    .await?;
    let task_id = started["task_id"].as_str().unwrap_or_default().to_string();
    let agent_session = started["agent_session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !task_id.is_empty() && !agent_session.is_empty(),
        "{started}"
    );

    let begun = event_where(&mut watcher, |e| e["kind"] == "task_started").await?;
    let mut between = events_until(&mut watcher, |e| e["kind"] == "task_finished").await?;
    let ended = between.pop().unwrap_or_default();
    for frame in [&begun, &ended] {
        let task = &frame["event"]["task"];
        assert_eq!(task["task_id"], task_id.as_str(), "{frame}");
        assert_eq!(task["detail"]["tag"], "flow:run-1/review/t1", "{frame}");
        assert_eq!(task["detail"]["agent_session_id"], agent_session.as_str());
        assert_eq!(frame["turn_id"], Value::Null, "{frame}");
    }
    let task = &ended["event"]["task"];
    assert_eq!(task["status"], "done", "{task}");
    let calls = provider.calls().len() as u64;
    assert_eq!(calls, 1);
    let updates: Vec<&Value> = between
        .iter()
        .filter(|f| f["event"]["kind"] == "task_updated")
        .collect();
    assert_eq!(updates.len() as u64, calls, "one update per model call");
    assert_eq!(updates[0]["event"]["task"]["usage"], task["usage"]);
    let progress: Vec<&Value> = between
        .iter()
        .filter(|f| f["event"]["kind"] == "subagent_progress")
        .collect();
    assert!(!progress.is_empty());
    assert!(
        progress
            .iter()
            .all(|f| f["event"]["task_id"] == task_id.as_str()),
        "the agent's progress names its task"
    );
    assert_eq!(
        task["usage"]["input_tokens"],
        blackbox::provider::INPUT_TOKENS * calls
    );
    assert_eq!(
        task["usage"]["output_tokens"],
        blackbox::provider::OUTPUT_TOKENS * calls
    );

    let listed = client
        .call("session.tasks", json!({ "session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    let entry = listed["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["task_id"] == task_id.as_str())
        .cloned()
        .unwrap_or_default();
    assert_eq!(entry["detail"]["tag"], "flow:run-1/review/t1", "{listed}");
    assert_eq!(entry["usage"], task["usage"], "{listed}");

    let children = client
        .call("session.list", json!({ "parent_session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    assert!(
        children["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| s["session_id"] == agent_session.as_str()),
        "{children}"
    );
    let history = client
        .call(
            "session.history",
            json!({ "session_id": agent_session, "limit": 50 }),
        )
        .await?;
    assert!(
        history
            .result
            .unwrap_or_default()
            .to_string()
            .contains("REVIEWED-OK"),
        "the sidechain holds what the agent said"
    );

    provider.script([Reply::text("noted")]);
    client
        .session_run_turn(&session, "anything new?", "t1", None)
        .await?;
    let told = provider.calls().last().cloned().expect("a call");
    assert!(
        told.body.to_string().contains("<task-notification>"),
        "notify_parent defaults to true"
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_output_is_on_the_task_and_the_parent_can_be_left_out() -> anyhow::Result<()> {
    let (_world, provider, daemon, mut client, session) = daemon_with("schema", json!({})).await?;
    provider.script_when(
        "grade the change",
        [
            Reply::calls("so-1", "StructuredOutput", json!({ "grade": 3 })),
            Reply::calls("so-2", "StructuredOutput", json!({ "verdict": "pass" })),
            Reply::text("handed in"),
        ],
    );
    let schema = json!({
        "type": "object",
        "required": ["verdict"],
        "properties": { "verdict": { "type": "string" } }
    });
    let started = start_agent(
        &mut client,
        json!({
            "session_id": session,
            "prompt": "grade the change",
            "output_schema": schema,
            "notify_parent": false
        }),
    )
    .await?;
    let task = settled(&mut client, &session, &started).await?;
    assert_eq!(task["status"], "done", "{task}");
    assert_eq!(
        task["structured_output"],
        json!({ "verdict": "pass" }),
        "{task}"
    );
    let calls = provider.calls();
    assert!(
        calls[1].body.to_string().contains("is_error"),
        "the first hand-in did not match the schema and was sent back: {}",
        calls[1].body["messages"]
    );

    provider.script([Reply::text("nothing")]);
    client
        .session_run_turn(&session, "anything new?", "t1", None)
        .await?;
    let asked = provider.calls().last().cloned().expect("a call");
    assert!(
        !asked.body.to_string().contains("task-notification"),
        "notify_parent: false keeps it out of the parent's conversation"
    );
    daemon.stop().await;
    Ok(())
}

/// The session has no output schema, so no `StructuredOutput` of its own;
/// the agent's, bound to the schema it was started with, is judged as the
/// tool it is rather than as one the session does not know.
#[tokio::test(flavor = "multi_thread")]
async fn handing_in_needs_no_approval_when_the_session_has_no_schema() -> anyhow::Result<()> {
    let (_world, provider, daemon, mut client, session) = daemon_with(
        "schema-default",
        json!({ "permissions": {"mode": "default"} }),
    )
    .await?;
    provider.script_when(
        "hand in the verdict",
        [
            Reply::calls("so-1", "StructuredOutput", json!({ "verdict": "pass" })),
            Reply::text("handed in"),
        ],
    );
    let started = start_agent(
        &mut client,
        json!({
            "session_id": session,
            "prompt": "hand in the verdict",
            "output_schema": {
                "type": "object",
                "required": ["verdict"],
                "properties": { "verdict": { "type": "string" } }
            },
            "notify_parent": false
        }),
    )
    .await?;
    let task = settled(&mut client, &session, &started).await?;
    assert_eq!(task["status"], "done", "{task}");
    assert_eq!(
        task["structured_output"],
        json!({ "verdict": "pass" }),
        "{task}"
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_cap_refuses_one_agent_too_many() -> anyhow::Result<()> {
    let (_world, provider, daemon, mut client, session) = daemon_with(
        "cap",
        json!({
            "permissions": {"allow": ["Bash"]},
            "agents": { "max_concurrent": 1 }
        }),
    )
    .await?;
    provider.script([
        Reply::calls("b-1", "Bash", json!({ "command": "sleep 2" })),
        Reply::text("slept"),
    ]);
    let first = start_agent(
        &mut client,
        json!({ "session_id": session, "prompt": "take a while" }),
    )
    .await?;
    let second = client
        .call(
            "session.startAgent",
            json!({ "session_id": session, "prompt": "one more" }),
        )
        .await?;
    let error = second.error.expect("over the cap");
    assert_eq!(error.code, -32019, "{error:?}");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let listed = client
            .call("session.tasks", json!({ "session_id": session }))
            .await?
            .result
            .unwrap_or_default();
        if listed["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| t["task_id"] == first["task_id"] && t["status"] == "done")
        {
            break;
        }
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "{listed}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hosts_deny_rules_hold_and_a_question_reaches_it_between_turns() -> anyhow::Result<()> {
    let (world, provider, daemon, mut client, session) = daemon_with("ask", json!({})).await?;
    let mut watcher = daemon.connect().await?;
    watcher.session_subscribe(&session).await?;
    provider.script([
        Reply::Blocks(vec![
            Block::tool(
                "b-1",
                "Bash",
                json!({ "command": "echo asked > asked.txt" }),
            ),
            Block::tool(
                "w-2",
                "Write",
                json!({ "file_path": "draft.md", "content": "leak" }),
            ),
        ]),
        Reply::text("did what I could"),
    ]);
    start_agent(
        &mut client,
        json!({
            "session_id": session,
            "prompt": "do the two things",
            "permission_rules": [{
                "source": "session", "behavior": "deny",
                "tool_name": "Write", "rule_content": "**/draft.md"
            }]
        }),
    )
    .await?;

    let asked = event_where(&mut watcher, |e| e["kind"] == "prompt").await?;
    assert_eq!(asked["turn_id"], Value::Null, "{asked}");
    let message = asked["event"]["message"].as_str().unwrap_or_default();
    assert!(message.starts_with("[sub-agent "), "{asked}");
    let prompt_id = asked["event"]["prompt_id"].clone();
    let listed = client
        .call("session.tasks", json!({ "session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    let waiting = &listed["tasks"][0];
    assert_eq!(waiting["status"], "waiting", "{listed}");
    assert_eq!(waiting["waiting_for"], json!([prompt_id]), "{listed}");
    let answered = client
        .call(
            "session.respondToPrompt",
            json!({
                "session_id": session,
                "prompt_id": asked["event"]["prompt_id"],
                "decision": { "type": "permit" }
            }),
        )
        .await?;
    assert!(answered.error.is_none(), "{answered:?}");

    let mut between = events_until(&mut watcher, |e| e["kind"] == "task_finished").await?;
    let ended = between.pop().unwrap_or_default();
    assert_eq!(ended["event"]["task"]["status"], "done", "{ended}");
    assert!(
        between.iter().any(|f| f["event"]["kind"] == "task_updated"
            && f["event"]["task"]["status"] == "running"
            && f["event"]["task"].get("waiting_for").is_none()),
        "the answer puts the task back to running"
    );
    let project = world.project("ask")?;
    assert_eq!(
        std::fs::read_to_string(project.join("asked.txt"))
            .ok()
            .as_deref(),
        Some("asked\n"),
        "the answered question let the command run"
    );
    assert!(
        !project.join("draft.md").exists(),
        "the host's deny rule refused a write default mode would have allowed"
    );
    daemon.stop().await;
    Ok(())
}

/// 4500 counted tokens on its first call put a sub-agent with
/// `token_budget: 4000` past it: it stops there, `failed` with
/// `budget_exceeded`, while its sibling without one finishes, the session
/// counts both and its next turn runs.
#[tokio::test(flavor = "multi_thread")]
async fn an_agents_own_token_budget_stops_it_and_nothing_else() -> anyhow::Result<()> {
    let (_world, provider, daemon, mut client, session) = daemon_with("budget", json!({})).await?;
    let mut watcher = daemon.connect().await?;
    watcher.session_subscribe(&session).await?;
    provider.script_when(
        "BUDGETED-MARK",
        [
            Reply::spent(
                4000,
                500,
                Reply::calls("g-1", "Glob", json!({ "pattern": "*" })),
            ),
            Reply::text("never sent"),
        ],
    );
    provider.script_when(
        "SIBLING-MARK",
        [Reply::spent(4000, 500, Reply::text("SIBLING-DONE"))],
    );

    let budgeted = start_agent(
        &mut client,
        json!({
            "session_id": session,
            "prompt": "BUDGETED-MARK look around",
            "description": "budgeted",
            "token_budget": 4000
        }),
    )
    .await?;
    let sibling = start_agent(
        &mut client,
        json!({
            "session_id": session,
            "prompt": "SIBLING-MARK look around",
            "description": "sibling"
        }),
    )
    .await?;

    let mut finished = Vec::new();
    while finished.len() < 2 {
        let frame = event_where(&mut watcher, |e| e["kind"] == "task_finished").await?;
        finished.push(frame["event"]["task"].clone());
    }
    let task_of = |started: &Value| {
        finished
            .iter()
            .find(|t| t["task_id"] == started["task_id"])
            .cloned()
            .unwrap_or_default()
    };
    let stopped = task_of(&budgeted);
    assert_eq!(stopped["status"], "failed", "{stopped}");
    assert_eq!(
        stopped["error"], "budget_exceeded: the task's token budget of 4000 is used up",
        "{stopped}"
    );
    assert_eq!(stopped["usage"]["input_tokens"], 4000, "{stopped}");
    let output = client
        .call(
            "session.taskOutput",
            json!({ "session_id": session, "task_id": budgeted["task_id"] }),
        )
        .await?
        .result
        .unwrap_or_default();
    assert!(
        output["output"]
            .as_str()
            .unwrap_or_default()
            .contains("budget_exceeded"),
        "{output}"
    );
    let done = task_of(&sibling);
    assert_eq!(done["status"], "done", "{done}");
    assert!(done.get("error").is_none(), "{done}");
    let budgeted_calls = provider
        .calls()
        .iter()
        .filter(|c| c.body.to_string().contains("BUDGETED-MARK"))
        .count();
    assert_eq!(budgeted_calls, 1, "the sub-agent called past its budget");

    let got = client
        .call("session.get", json!({ "session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(got["usage"]["input_tokens"], 8000, "{got}");
    assert_eq!(got["usage"]["output_tokens"], 1000, "{got}");

    provider.script([Reply::text("noted")]);
    let next = client
        .session_run_turn(&session, "anything new?", "t1", None)
        .await?;
    let ended = next.turn_complete_event.clone().unwrap_or_default();
    assert_eq!(ended["stop_reason"], "end_turn", "{ended}");
    let told = provider.calls().last().cloned().expect("a call");
    assert!(
        told.body.to_string().contains("budget_exceeded"),
        "the parent is not told how its sub-agent ended"
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_budget_of_zero_is_refused() -> anyhow::Result<()> {
    let (_world, provider, daemon, mut client, session) =
        daemon_with("budget-zero", json!({})).await?;
    let refused = client
        .call(
            "session.startAgent",
            json!({ "session_id": session, "prompt": "look around", "token_budget": 0 }),
        )
        .await?;
    let error = refused.error.expect("refused");
    assert_eq!(error.code, -32602, "{error:?}");
    assert!(error.message.contains("token_budget"), "{error:?}");
    let listed = client
        .call("session.tasks", json!({ "session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(listed["tasks"], json!([]), "{listed}");
    assert_eq!(provider.call_count(), 0);
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn two_agents_under_one_session_ask_with_their_own_task() -> anyhow::Result<()> {
    let (_world, provider, daemon, mut client, session) = daemon_with("tasks", json!({})).await?;
    let mut watcher = daemon.connect().await?;
    watcher.session_subscribe(&session).await?;
    provider.script([
        Reply::Blocks(vec![Block::tool(
            "b-1",
            "Bash",
            json!({ "command": "echo one > one.txt" }),
        )]),
        Reply::Blocks(vec![Block::tool(
            "b-2",
            "Bash",
            json!({ "command": "echo two > two.txt" }),
        )]),
        Reply::text("done one"),
        Reply::text("done two"),
    ]);
    let mut started = Vec::new();
    for prompt in ["first", "second"] {
        started.push(
            start_agent(
                &mut client,
                json!({ "session_id": session, "prompt": prompt }),
            )
            .await?,
        );
    }

    let mut asked = Vec::new();
    while asked.len() < 2 {
        let frame = event_where(&mut watcher, |e| e["kind"] == "prompt").await?;
        asked.push(frame["event"].clone());
    }
    for start in &started {
        let own: Vec<&Value> = asked
            .iter()
            .filter(|p| {
                p["task_id"] == start["task_id"]
                    && p["agent_session_id"] == start["agent_session_id"]
            })
            .collect();
        assert_eq!(own.len(), 1, "{start} among {asked:?}");
        assert_eq!(own[0]["prompt_type"], "permission", "{own:?}");
        assert!(
            own[0]["tool_use_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "{own:?}"
        );
    }
    assert_ne!(asked[0]["task_id"], asked[1]["task_id"]);
    for prompt in &asked {
        let answered = client
            .call(
                "session.respondToPrompt",
                json!({
                    "session_id": session,
                    "prompt_id": prompt["prompt_id"],
                    "decision": { "type": "permit" }
                }),
            )
            .await?;
        assert!(answered.error.is_none(), "{answered:?}");
    }
    for start in &started {
        assert_eq!(
            settled(&mut client, &session, start).await?["status"],
            "done"
        );
    }
    daemon.stop().await;
    Ok(())
}
