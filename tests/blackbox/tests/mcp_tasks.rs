//! An MCP server turning a tool call into a task, against a scripted model and
//! the toy server's `work` tool: the host sees the task in `session.tasks` and
//! in `task_*` frames, stopping it or interrupting the turn makes the server
//! receive `tasks/cancel`, and a finished task's result reaches the model as
//! the call's own. A server without tasks answers the same call directly.

use std::path::Path;
use std::time::Duration;

use blackbox::{Answer, LiveConfig, LiveRun, LiveRunBuilder, McpToy, ProviderStub, Reply, Watcher};
use serde_json::{json, Value};

fn toy_run(
    name: &str,
    stub: &ProviderStub,
    calls: &Path,
    toy_args: &[&str],
) -> anyhow::Result<LiveRunBuilder> {
    let mut args = vec!["--calls", calls.to_str().unwrap_or_default()];
    args.extend_from_slice(toy_args);
    Ok(LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .settings(json!({
            "mcp": { "servers": { "demo": McpToy::stdio_config(&args)? } },
            "permissions": {"allow": ["mcp__demo"]},
        })))
}

async fn session(run: &mut LiveRun) -> anyhow::Result<String> {
    run.session_with(json!({
        "options": { "prompt_types": ["permission", "mcp_elicitation"] }
    }))
    .await
}

fn logged(calls: &Path, what: &str) -> Vec<String> {
    McpToy::calls(calls)
        .into_iter()
        .filter_map(|(_, line)| line.starts_with(what).then_some(line))
        .collect()
}

async fn wait_logged(calls: &Path, what: &str) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while logged(calls, what).is_empty() {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the server never logged `{what}`: {:?}",
            McpToy::calls(calls)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

fn is_mcp_task(kind: &'static str) -> impl Fn(&Value) -> bool {
    move |event| event["kind"] == kind && event["task"]["kind"] == "mcp_task"
}

async fn wait_started(host: &Watcher) -> anyhow::Result<Value> {
    host.wait_for(
        "the task starting",
        Duration::from_secs(30),
        is_mcp_task("task_started"),
    )
    .await
}

fn result_of(events: &rpc_client::TurnEvents) -> &(String, String, bool) {
    events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "mcp__demo__work")
        .expect("the call's result")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_the_server_runs_as_a_task_is_listed_reports_progress_and_returns_its_result(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("task-done", &stub, &calls, &["--task"])?
        .start()
        .await?;
    let session = session(&mut run).await?;
    let host = run.watch(&session, true).await?;
    let nonce = "NONCE-5521";
    stub.script([
        Reply::calls(
            "w1",
            "mcp__demo__work",
            json!({ "steps": 3, "step_ms": 300, "code": nonce }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "work").await?;

    let started = host.events("task_started");
    let task = started
        .iter()
        .map(|f| &f["event"]["task"])
        .find(|t| t["kind"] == "mcp_task")
        .unwrap_or_else(|| panic!("no mcp_task started: {:?}", host.frames()));
    assert_eq!(task["detail"]["server"], "demo", "{task}");
    assert_eq!(task["detail"]["tool_use_id"], "w1", "{task}");
    let created = logged(&calls, "work:task ");
    assert_eq!(created.len(), 1, "{:?}", McpToy::calls(&calls));
    assert_eq!(
        task["detail"]["mcp_task_id"],
        created[0].trim_start_matches("work:task "),
        "{task}"
    );
    let progress: String = host
        .events("task_progress")
        .iter()
        .filter(|f| f["event"]["task_id"] == task["task_id"])
        .map(|f| {
            f["event"]["appended"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert!(progress.contains("step 2/3"), "{progress:?}");
    let finished = host
        .events("task_finished")
        .into_iter()
        .find(|f| f["event"]["task"]["task_id"] == task["task_id"])
        .expect("task_finished");
    assert_eq!(finished["event"]["task"]["status"], "done", "{finished}");

    let result = result_of(&events);
    assert!(!result.2, "{result:?}");
    assert!(
        result.1.starts_with("<external-content")
            && blackbox::inside_fence(&result.1).contains(&format!("work done: 3 steps {nonce}")),
        "{result:?}"
    );
    let sent = stub.calls().last().cloned().expect("a model call");
    assert!(
        sent.messages_text()
            .contains(&format!("work done: 3 steps {nonce}")),
        "the model never saw the task's result"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn session_stop_task_cancels_the_mcp_task_on_the_server() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("task-stop", &stub, &calls, &["--task"])?
        .start()
        .await?;
    let session = session(&mut run).await?;
    let host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls(
            "w1",
            "mcp__demo__work",
            json!({ "steps": 100, "step_ms": 200 }),
        ),
        Reply::text("carried on"),
    ]);
    let turn = run.spawn_turn(&session, "work", json!({})).await?;
    let started = wait_started(&host).await?;
    let task_id = started["event"]["task"]["task_id"].clone();
    let stopped = run
        .client
        .call(
            "session.stopTask",
            json!({ "session_id": session, "task_id": task_id }),
        )
        .await?;
    assert_eq!(stopped.result.unwrap_or_default()["stopped"], json!(true));
    let events = tokio::time::timeout(Duration::from_secs(30), turn).await???;

    wait_logged(&calls, "work:cancel-requested").await?;
    wait_logged(&calls, "work:cancelled").await?;
    let result = result_of(&events);
    assert!(
        result.2 && result.1.to_lowercase().contains("cancel"),
        "{result:?}"
    );
    assert!(events.turn_complete, "the turn went on after the stop");
    let tasks = run.tasks(&session).await?;
    let task = tasks
        .iter()
        .find(|t| t["task_id"] == task_id)
        .expect("listed");
    assert_eq!(task["status"], "stopped", "{task}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupting_the_turn_cancels_the_mcp_task() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("task-interrupt", &stub, &calls, &["--task"])?
        .start()
        .await?;
    let session = session(&mut run).await?;
    let host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls(
            "w1",
            "mcp__demo__work",
            json!({ "steps": 100, "step_ms": 200 }),
        ),
        Reply::text("never reached"),
    ]);
    let turn = run.spawn_turn(&session, "work", json!({})).await?;
    let started = wait_started(&host).await?;
    let task_id = started["event"]["task"]["task_id"].clone();
    let interrupted = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    assert_eq!(
        interrupted.result.unwrap_or_default()["interrupted"],
        json!(true)
    );
    tokio::time::timeout(Duration::from_secs(30), turn).await???;
    run.wait_turn_state(&session, "idle", Duration::from_secs(10))
        .await?;

    wait_logged(&calls, "work:cancel-requested").await?;
    wait_logged(&calls, "work:cancelled").await?;
    let tasks = run.tasks(&session).await?;
    let task = tasks
        .iter()
        .find(|t| t["task_id"] == task_id)
        .expect("listed");
    assert_eq!(task["status"], "stopped", "{task}");
    run.finish_allowing_refusals().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_that_asks_mid_way_is_answered_through_the_prompt_frame() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("task-ask", &stub, &calls, &["--task"])?
        .start()
        .await?;
    let session = session(&mut run).await?;
    let host = run
        .watch_with(&session, |event| {
            (event["prompt_type"] == "mcp_elicitation")
                .then(|| Answer::accept(Some(json!({ "color": "blue" }))))
        })
        .await?;
    stub.script([
        Reply::calls(
            "w1",
            "mcp__demo__work",
            json!({ "steps": 2, "step_ms": 100, "ask": true }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "work").await?;

    let prompt = host
        .frames()
        .into_iter()
        .map(|f| f["event"].clone())
        .find(|e| e["kind"] == "prompt" && e["prompt_type"] == "mcp_elicitation")
        .expect("the task's question reached the host");
    let waiting = host
        .events("task_updated")
        .into_iter()
        .find(|f| {
            f["event"]["task"]["kind"] == "mcp_task"
                && f["event"]["task"]["status"] == "waiting"
                && f["event"]["task"]["waiting_for"] == json!([prompt["prompt_id"]])
        })
        .is_some();
    assert!(waiting, "{:?}", host.frames());
    assert_eq!(
        logged(&calls, "ask ").len(),
        1,
        "{:?}",
        McpToy::calls(&calls)
    );
    assert!(
        logged(&calls, "ask ")[0].contains("blue"),
        "{:?}",
        McpToy::calls(&calls)
    );
    let finished = host
        .events("task_finished")
        .into_iter()
        .find(|f| f["event"]["task"]["kind"] == "mcp_task")
        .expect("task_finished");
    assert_eq!(finished["event"]["task"]["status"], "done", "{finished}");
    assert!(!result_of(&events).2);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_that_fails_on_the_server_is_an_error_result() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("task-fail", &stub, &calls, &["--task"])?
        .start()
        .await?;
    let session = session(&mut run).await?;
    let host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls(
            "w1",
            "mcp__demo__work",
            json!({ "steps": 1, "step_ms": 100, "fail": true }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "work").await?;

    let result = result_of(&events);
    assert!(
        result.2 && result.1.contains("work failed on purpose"),
        "{result:?}"
    );
    let finished = host
        .events("task_finished")
        .into_iter()
        .find(|f| f["event"]["task"]["kind"] == "mcp_task")
        .expect("task_finished");
    let task = &finished["event"]["task"];
    assert_eq!(task["status"], "failed", "{task}");
    assert!(
        task["error"]
            .as_str()
            .unwrap_or_default()
            .contains("work failed on purpose"),
        "{task}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_tasks_answers_the_same_call_directly() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("task-none", &stub, &calls, &[])?.start().await?;
    let session = session(&mut run).await?;
    let host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls(
            "w1",
            "mcp__demo__work",
            json!({ "steps": 1, "step_ms": 50 }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "work").await?;

    let result = result_of(&events);
    assert!(!result.2 && result.1.contains("work done"), "{result:?}");
    assert_eq!(logged(&calls, "work:sync").len(), 1);
    assert!(logged(&calls, "work:task").is_empty());
    assert!(
        host.events("task_started")
            .iter()
            .all(|f| f["event"]["task"]["kind"] != "mcp_task"),
        "{:?}",
        host.frames()
    );
    run.finish().await;
    Ok(())
}
