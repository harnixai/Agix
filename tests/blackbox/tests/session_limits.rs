//! What a session spends over its turns: `session.get`'s `usage` counts its
//! delegates' calls as they are made, and a failed turn's, and survives a
//! restart; `session.token_budget` holds it to a ceiling across turns, and a
//! goal's `token_budget` blocks the goal once its rounds have spent it; the
//! `schedule` block holds what the model schedules for itself, not what a
//! host does.

use std::time::Duration;

use blackbox::{Block, LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Reply};
use rpc_client::ToolCallReply;
use serde_json::{json, Value};

const ROOT: &str = "ROOT-MARK";
const WAIT: Duration = Duration::from_secs(30);

fn stubbed(name: &str, stub: &ProviderStub, settings: Value) -> LiveRunBuilder {
    let mut merged = json!({ "permissions": { "allow": ["Agent", "Glob", "HostGate", "GoalCreate", "CronCreate"] } });
    if let (Some(base), Some(extra)) = (merged.as_object_mut(), settings.as_object()) {
        base.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    LiveRun::builder(name)
        .settings(merged)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
}

async fn session(run: &mut LiveRun, options: Value) -> anyhow::Result<String> {
    let mut options = options;
    options["auto_name"] = json!(false);
    run.session_with(json!({ "options": options })).await
}

fn delegate(prompt: &str, background: bool) -> Reply {
    Reply::calls(
        "a1",
        "Agent",
        json!({ "description": "helper", "prompt": prompt, "background": background }),
    )
}

fn gate_tool() -> Value {
    json!({
        "name": "HostGate",
        "description": "Waits for the host.",
        "input_schema": { "type": "object", "properties": {} },
        "read_only": true,
        "subagents": true
    })
}

fn stop_reason(events: &rpc_client::TurnEvents) -> Value {
    events.turn_complete_event.clone().unwrap_or_default()["stop_reason"].clone()
}

fn turn_message(events: &rpc_client::TurnEvents) -> String {
    let done = events.turn_complete_event.clone().unwrap_or_default();
    done["message"].as_str().unwrap_or_default().to_string()
}

/// `session.get`'s `usage`, as input and output tokens.
async fn usage(run: &mut LiveRun, s: &str) -> anyhow::Result<(u64, u64)> {
    let got = run
        .client
        .call("session.get", json!({ "session_id": s }))
        .await?
        .result
        .unwrap_or_default();
    let figure = |k: &str| got["usage"][k].as_u64().unwrap_or_default();
    Ok((figure("input_tokens"), figure("output_tokens")))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sessions_usage_counts_a_background_sub_agent_as_it_spends() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-usage-live", &stub, json!({}))
        .start()
        .await?;
    let s = session(&mut run, json!({ "host_tools": [gate_tool()] })).await?;
    let host = run.watch(&s, true).await?;
    stub.script_when(
        ROOT,
        [
            Reply::spent(1000, 100, delegate("wait at the gate", true)),
            Reply::spent(1000, 100, Reply::text("started it")),
            Reply::spent(10, 1, Reply::text("noted")),
        ],
    );
    stub.script([
        Reply::spent(100, 50, Reply::calls("h1", "HostGate", json!({}))),
        Reply::spent(400, 40, Reply::text("through the gate")),
    ]);
    run.turn(&s, &format!("{ROOT} start it")).await?;

    let call = host
        .wait_for("the gate", WAIT, |e| e["kind"] == "host_tool_call")
        .await?;
    assert_eq!(
        usage(&mut run, &s).await?,
        (2100, 250),
        "the running sub-agent's first call is not in the session's usage"
    );
    let answered = run
        .client
        .session_respond_tool_call(
            &s,
            call["event"]["call_id"].as_str().unwrap_or_default(),
            ToolCallReply::Result {
                content: json!([{ "type": "text", "text": "open" }]),
                is_error: false,
            },
        )
        .await?;
    assert!(answered.error.is_none(), "{answered:?}");
    run.wait_tasks_settled(&s, WAIT).await?;
    assert_eq!(usage(&mut run, &s).await?, (2500, 290));

    run.turn(&s, &format!("{ROOT} anything else?")).await?;
    assert_eq!(usage(&mut run, &s).await?, (2510, 291));
    drop(host);

    run.restart().await?;
    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": s }))
        .await?;
    assert!(resumed.error.is_none(), "{resumed:?}");
    assert_eq!(
        usage(&mut run, &s).await?,
        (2510, 291),
        "a restart lost what the session had spent"
    );
    run.finish().await;
    Ok(())
}

/// 4000 in the first turn; 1000 of the root's and 4500 of its sub-agent's in
/// the second leave 500 of 10000, too little for the root's next call. The
/// third turn, and a turn after a restart, send nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_budget_holds_across_turns_sub_agents_and_a_restart() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed(
        "session-budget",
        &stub,
        json!({ "session": { "token_budget": 10_000 } }),
    )
    .start()
    .await?;
    let s = session(&mut run, json!({})).await?;
    stub.script([Reply::spent(3000, 1000, Reply::text("first"))]);
    let first = run.turn(&s, "one").await?;
    assert_eq!(stop_reason(&first), "end_turn", "{first:?}");

    stub.script_when(
        ROOT,
        [
            Reply::spent(1000, 0, delegate("look around", false)),
            Reply::text("never sent"),
        ],
    );
    stub.script([Reply::spent(4000, 500, Reply::text("CHILD-DONE"))]);
    let second = run.turn(&s, &format!("{ROOT} delegate it")).await?;
    assert_eq!(stop_reason(&second), "budget_exceeded", "{second:?}");
    assert!(
        turn_message(&second).contains("this session's token budget of 10000"),
        "{second:?}"
    );
    assert_eq!(
        stub.call_count(),
        3,
        "the root called past the session's budget"
    );
    assert_eq!(usage(&mut run, &s).await?, (8000, 1500));

    let third = run.turn(&s, "and again").await?;
    assert_eq!(stop_reason(&third), "budget_exceeded", "{third:?}");
    assert_eq!(stub.call_count(), 3, "a spent session called the model");

    run.restart().await?;
    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": s }))
        .await?;
    assert!(resumed.error.is_none(), "{resumed:?}");
    let after = run.turn(&s, "after the restart").await?;
    assert_eq!(stop_reason(&after), "budget_exceeded", "{after:?}");
    assert_eq!(
        stub.call_count(),
        3,
        "a restart gave the session its budget back"
    );
    run.finish().await;
    Ok(())
}

/// What `session.goal` says once `host` has seen the goal blocked.
async fn blocked_goal(
    run: &mut LiveRun,
    host: &blackbox::Watcher,
    s: &str,
) -> anyhow::Result<Value> {
    host.wait_for("the goal blocked", WAIT, |e| {
        e["kind"] == "goal_changed" && e["goal"]["phase"] == "blocked"
    })
    .await?;
    let got = run
        .client
        .call("session.goal", json!({ "session_id": s }))
        .await?;
    Ok(got.result.unwrap_or_default()["goal"].clone())
}

/// Two rounds spend 2400 of 3000; the third has too little left for a call,
/// so the goal is blocked on its token budget and no round runs after.
#[tokio::test(flavor = "multi_thread")]
async fn a_hosts_goal_is_blocked_once_its_rounds_have_spent_its_budget() -> anyhow::Result<()> {
    a_goal_is_blocked_on_its_budget("goal-budget-host", true, 2400, 2).await
}

/// The first round spends 1200 of 3000 and its prompt was not cached: the
/// second resends that much and more, which the 1800 left cannot pay for
/// with a reply, so it is not sent and the goal is blocked on its budget.
#[tokio::test(flavor = "multi_thread")]
async fn a_goal_whose_next_round_cannot_pay_for_its_input_is_blocked() -> anyhow::Result<()> {
    a_goal_is_blocked_on_its_budget("goal-budget-input", false, 1200, 1).await
}

async fn a_goal_is_blocked_on_its_budget(
    name: &str,
    cached: bool,
    used: u64,
    calls: usize,
) -> anyhow::Result<()> {
    let cache_read = if cached { 5000 } else { 0 };
    let stub = ProviderStub::start().await?;
    let mut run = stubbed(name, &stub, json!({})).start().await?;
    let s = session(&mut run, json!({})).await?;
    let host = run.watch(&s, false).await?;
    stub.script([
        Reply::spent_cached(1000, cache_read, 200, Reply::text("round one")),
        Reply::spent_cached(1000, cache_read, 200, Reply::text("round two")),
        Reply::text("never sent"),
    ]);
    let set = run
        .client
        .call(
            "session.setGoal",
            json!({ "session_id": s, "objective": "keep at it", "token_budget": 3000 }),
        )
        .await?;
    assert!(set.error.is_none(), "{set:?}");
    assert_eq!(set.result.unwrap_or_default()["goal"]["token_budget"], 3000);

    let goal = blocked_goal(&mut run, &host, &s).await?;
    assert_eq!(goal["tokens_used"], used, "{goal}");
    assert!(
        goal["blocked_reason"]
            .as_str()
            .unwrap_or_default()
            .starts_with("token budget"),
        "{goal}"
    );
    assert_eq!(
        stub.call_count(),
        calls,
        "a round called past the goal's budget"
    );
    drop(host);
    run.finish().await;
    Ok(())
}

/// The same with the model setting the goal: the turn it sets it in is not
/// one of the goal's rounds and does not spend its budget.
#[tokio::test(flavor = "multi_thread")]
async fn a_models_goal_is_blocked_once_its_rounds_have_spent_its_budget() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("goal-budget-model", &stub, json!({}))
        .start()
        .await?;
    let s = session(&mut run, json!({})).await?;
    let host = run.watch(&s, false).await?;
    stub.script([
        Reply::spent_cached(
            5000,
            5000,
            500,
            Reply::calls(
                "g1",
                "GoalCreate",
                json!({ "objective": "keep at it", "token_budget": 2500 }),
            ),
        ),
        Reply::spent_cached(5000, 5000, 500, Reply::text("goal set")),
        Reply::spent_cached(1000, 5000, 200, Reply::text("round one")),
        Reply::spent_cached(1000, 5000, 200, Reply::text("round two")),
        Reply::text("never sent"),
    ]);
    run.turn(&s, "set yourself a goal").await?;
    let goal = blocked_goal(&mut run, &host, &s).await?;
    assert_eq!(goal["tokens_used"], 2400, "{goal}");
    assert_eq!(goal["token_budget"], 2500, "{goal}");
    assert!(
        goal["blocked_reason"]
            .as_str()
            .unwrap_or_default()
            .starts_with("token budget"),
        "{goal}"
    );
    assert_eq!(stub.call_count(), 4);
    drop(host);
    run.finish().await;
    Ok(())
}

fn cron(id: &str, expr: &str) -> Reply {
    Reply::calls(
        id,
        "CronCreate",
        json!({ "cron": expr, "prompt": "check on it", "recurring": true }),
    )
}

fn cron_results(events: &rpc_client::TurnEvents) -> Vec<(String, bool)> {
    events
        .tool_results
        .iter()
        .filter(|(name, _, _)| name == "CronCreate")
        .map(|(_, content, is_error)| (content.clone(), *is_error))
        .collect()
}

async fn schedule_from_the_host(run: &mut LiveRun, s: &str, expr: &str) -> anyhow::Result<()> {
    let created = run
        .client
        .call(
            "schedule.create",
            json!({ "session_id": s, "cron": expr, "prompt": "from the host", "durable": false }),
        )
        .await?;
    assert!(created.error.is_none(), "{created:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_holds_no_more_jobs_than_max_model_jobs() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed(
        "model-jobs-cap",
        &stub,
        json!({ "schedule": { "max_model_jobs": 1 } }),
    )
    .start()
    .await?;
    let s = session(&mut run, json!({})).await?;
    stub.script([
        cron("c1", "0 9 * * *"),
        cron("c2", "0 10 * * *"),
        Reply::text("scheduled what I could"),
    ]);
    let events = run.turn(&s, "schedule two jobs").await?;
    let results = cron_results(&events);
    assert_eq!(results.len(), 2, "{events:?}");
    assert!(!results[0].1, "{results:?}");
    assert!(results[1].1, "{results:?}");
    assert!(
        results[1].0.contains("schedule.max_model_jobs"),
        "{results:?}"
    );

    schedule_from_the_host(&mut run, &s, "0 11 * * *").await?;
    schedule_from_the_host(&mut run, &s, "0 12 * * *").await?;
    let jobs = run
        .client
        .call("schedule.list", json!({ "session_id": s }))
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(jobs["jobs"].as_array().map(Vec::len), Some(3), "{jobs}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_cannot_schedule_closer_than_min_model_interval_minutes() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed(
        "model-jobs-interval",
        &stub,
        json!({ "schedule": { "min_model_interval_minutes": 10 } }),
    )
    .start()
    .await?;
    let s = session(&mut run, json!({})).await?;
    stub.script([
        cron("c1", "* * * * *"),
        cron("c2", "*/30 * * * *"),
        Reply::text("done"),
    ]);
    let events = run.turn(&s, "schedule them").await?;
    let results = cron_results(&events);
    assert_eq!(results.len(), 2, "{events:?}");
    assert!(results[0].1, "{results:?}");
    assert!(
        results[0].0.contains("schedule.min_model_interval_minutes"),
        "{results:?}"
    );
    assert!(!results[1].1, "{results:?}");

    schedule_from_the_host(&mut run, &s, "* * * * *").await?;
    run.finish().await;
    Ok(())
}

/// The stream opens, reports its input, then fails with an error nothing
/// retries: the turn fails, and what the provider reported is spent.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_turns_call_is_in_the_sessions_usage() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-usage-failed", &stub, json!({}))
        .start()
        .await?;
    let s = session(&mut run, json!({})).await?;
    stub.script([Reply::errored(
        vec![Block::text("half an answer")],
        "invalid_request_error",
        "this request is refused",
    )]);
    let failed = run.turn(&s, "go").await;
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(stub.call_count(), 1);
    let (input, _) = usage(&mut run, &s).await?;
    assert_eq!(
        input,
        blackbox::provider::INPUT_TOKENS,
        "the failed call's input is not in the session's usage"
    );
    run.finish_allowing_refusals().await;
    Ok(())
}

/// A summary `session.compact` writes between turns is the session's spend:
/// `session.token_budget` counts from creation, not only within turns.
#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_between_turns_is_counted_in_the_sessions_usage() -> anyhow::Result<()> {
    const SUMMARY: &str = "You are compacting an AI coding agent's conversation";
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("between-turns-spend", &stub, json!({}))
        .start()
        .await?;
    let s = session(&mut run, json!({})).await?;
    let mut script = Vec::new();
    for i in 0..4 {
        let body: String = (0..200)
            .map(|l| format!("file {i} line {l} of filler\n"))
            .collect();
        let path = run.write_file(&format!("f{i}.txt"), &body)?;
        script.push(Reply::calls(
            &format!("r{i}"),
            "Read",
            json!({ "file_path": path.to_string_lossy() }),
        ));
    }
    script.push(Reply::text("read them"));
    stub.script(script);
    run.turn(&s, &format!("{ROOT} read the files")).await?;
    let before = usage(&mut run, &s).await?;

    stub.script_when(
        SUMMARY,
        [Reply::spent(5000, 700, Reply::text("SUMMARY-OF-READS"))],
    );
    let compacted = run
        .client
        .call("session.compact", json!({ "session_id": s }))
        .await?;
    assert!(compacted.error.is_none(), "{compacted:?}");
    let after = usage(&mut run, &s).await?;
    assert_eq!(
        (after.0 - before.0, after.1 - before.1),
        (5000, 700),
        "before {before:?}, after {after:?}"
    );
    run.finish().await;
    Ok(())
}
