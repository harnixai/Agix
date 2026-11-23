//! A turn's limits, from `session.create`'s `options.settings.turn` and the
//! settings' `turn` block: the token budget is one for the turn and every delegate it
//! starts — a background one after the turn has ended too — each call asks
//! for no more output than is left once its input is paid — the part of it
//! not read from the provider's cache — and a session's options only tighten what
//! the settings allow, surviving a restart. `turn_complete.usage` is every
//! call the turn made, its delegates' and the ones made on its side included.

use std::time::Duration;

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply, SeenRequest};
use rpc_client::{ToolCallReply, TurnEvents};
use serde_json::{json, Value};

const ROOT: &str = "ROOT-MARK";
const WAIT: Duration = Duration::from_secs(30);

fn stubbed(name: &str, stub: &ProviderStub, settings: Value) -> LiveRunBuilder {
    let mut merged = json!({ "permissions": { "allow": ["Agent", "Glob", "Read", "HostGate"] } });
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

fn glob(id: &str) -> Reply {
    Reply::calls(id, "Glob", json!({ "pattern": "*.md" }))
}

fn delegate(prompt: &str, background: bool) -> Reply {
    Reply::calls(
        "a1",
        "Agent",
        json!({ "description": "helper", "prompt": prompt, "background": background }),
    )
}

fn stop_reason(events: &TurnEvents) -> Value {
    events.turn_complete_event.clone().unwrap_or_default()["stop_reason"].clone()
}

fn max_tokens(call: &SeenRequest) -> u64 {
    call.body["max_tokens"].as_u64().unwrap_or_default()
}

fn agent_result(events: &TurnEvents) -> String {
    events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "Agent")
        .map(|(_, content, _)| content.clone())
        .unwrap_or_default()
}

fn child_calls(stub: &ProviderStub) -> Vec<SeenRequest> {
    stub.calls()
        .into_iter()
        .filter(|c| !c.messages_contain(ROOT))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tight_session_budget_ends_the_turn_on_what_was_reported() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("tight-budget", &stub, json!({})).start().await?;
    let s = session(
        &mut run,
        json!({ "settings": { "turn": { "token_budget": 2000 } } }),
    )
    .await?;
    stub.script([Reply::spent(1800, 700, glob("g1"))]);
    let events = run.turn(&s, "look").await?;
    assert_eq!(stop_reason(&events), "budget_exceeded", "{events:?}");
    let calls = stub.calls();
    assert_eq!(calls.len(), 1, "a call went out past the budget");
    assert!(
        (1024..2000).contains(&max_tokens(&calls[0])),
        "the budget less the opening message: {}",
        max_tokens(&calls[0])
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn each_call_asks_for_what_is_left_and_none_goes_out_under_1024() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("budget-headroom", &stub, json!({})).start().await?;
    let s = session(
        &mut run,
        json!({ "settings": { "turn": { "token_budget": 6000 } } }),
    )
    .await?;
    stub.script([
        Reply::spent_cached(2000, 8000, 1000, glob("g1")),
        Reply::spent_cached(1800, 8000, 700, glob("g2")),
    ]);
    let events = run.turn(&s, "look twice").await?;
    assert_eq!(stop_reason(&events), "budget_exceeded", "{events:?}");
    let asked: Vec<u64> = stub.calls().iter().map(max_tokens).collect();
    assert!(
        asked.len() == 2 && (5000..6000).contains(&asked[0]) && (2800..3000).contains(&asked[1]),
        "500 tokens left is no call; each one asks for at most what is left after its \
         input: {asked:?}"
    );
    run.finish().await;
    Ok(())
}

/// The root spends 1100 of 10000; its sub-agent spends the rest. The
/// sub-agent stops before a third call, and the root sends no further one.
async fn a_sub_agent_spends_its_parents_budget(
    name: &str,
    settings: Value,
    options: Value,
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed(name, &stub, settings).start().await?;
    let s = session(&mut run, options).await?;
    stub.script_when(
        ROOT,
        [Reply::spent(1000, 100, delegate("look around", false))],
    );
    stub.script([
        Reply::spent_cached(4000, 1000, 500, glob("c1")),
        Reply::spent_cached(3500, 1000, 500, glob("c2")),
    ]);
    let events = run.turn(&s, &format!("{ROOT} delegate it")).await?;
    assert_eq!(stop_reason(&events), "budget_exceeded", "{events:?}");
    assert_eq!(stub.call_count(), 3, "a call went out on a spent budget");
    let child = child_calls(&stub);
    assert_eq!(child.len(), 2);
    assert!(max_tokens(&child[0]) <= 8900, "{}", max_tokens(&child[0]));
    assert!(max_tokens(&child[1]) <= 4400, "{}", max_tokens(&child[1]));
    let result = agent_result(&events);
    assert!(result.contains("budget_exceeded"), "{result}");

    let complete = events.turn_complete_event.clone().unwrap_or_default();
    assert_eq!(
        complete["usage_breakdown"]["own"]["input_tokens"], 1000,
        "{complete}"
    );
    assert_eq!(complete["usage_breakdown"]["own"]["output_tokens"], 100);
    assert_eq!(complete["usage"]["input_tokens"], 8500);
    assert_eq!(complete["usage"]["output_tokens"], 1100);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agents_calls_count_toward_the_sessions_budget() -> anyhow::Result<()> {
    a_sub_agent_spends_its_parents_budget(
        "shared-option",
        json!({}),
        json!({ "settings": { "turn": { "token_budget": 10000 } } }),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn the_settings_budget_counts_sub_agents_too() -> anyhow::Result<()> {
    a_sub_agent_spends_its_parents_budget(
        "shared-setting",
        json!({ "turn": { "token_budget": 10000 } }),
        json!({}),
    )
    .await
}

/// The background sub-agent waits on a host tool until its parent's turn is
/// over, then spends past the budget that turn started it under.
#[tokio::test(flavor = "multi_thread")]
async fn a_background_sub_agent_spends_its_starting_turns_budget_after_it_ended(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("background-budget", &stub, json!({}))
        .start()
        .await?;
    let gate = json!({
        "name": "HostGate",
        "description": "Waits for the host.",
        "input_schema": { "type": "object", "properties": {} },
        "read_only": true,
        "subagents": true
    });
    let s = session(
        &mut run,
        json!({ "settings": { "turn": { "token_budget": 10000 } }, "host_tools": [gate] }),
    )
    .await?;
    let host = run.watch(&s, true).await?;
    stub.script_when(
        ROOT,
        [
            Reply::spent(1000, 100, delegate("wait at the gate", true)),
            Reply::spent(1000, 100, Reply::text("started it")),
        ],
    );
    stub.script([
        Reply::spent(100, 50, Reply::calls("h1", "HostGate", json!({}))),
        Reply::spent_cached(4000, 1000, 500, glob("c1")),
        Reply::spent_cached(4000, 1000, 500, glob("c2")),
    ]);
    let events = run.turn(&s, &format!("{ROOT} start it")).await?;
    assert_eq!(stop_reason(&events), "end_turn", "{events:?}");

    let call = host
        .wait_for("the gate", WAIT, |e| e["kind"] == "host_tool_call")
        .await?;
    assert_eq!(child_calls(&stub).len(), 1);
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

    let tasks = run.wait_tasks_settled(&s, WAIT).await?;
    let task = tasks
        .iter()
        .find(|t| t["kind"] == "agent")
        .cloned()
        .unwrap_or_default();
    assert_eq!(task["status"], "done", "{tasks:?}");
    assert_eq!(
        child_calls(&stub).len(),
        3,
        "the background sub-agent called past its turn's budget"
    );
    let output = run
        .client
        .call(
            "session.taskOutput",
            json!({ "session_id": s, "task_id": task["task_id"] }),
        )
        .await?
        .result
        .unwrap_or_default();
    let text = output["output"].as_str().unwrap_or_default();
    assert!(text.contains("budget_exceeded"), "{output}");
    drop(host);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn options_wider_than_the_settings_are_held_to_them_and_reported() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed(
        "clamped-turn",
        &stub,
        json!({ "turn": { "token_budget": 5000, "max_model_calls": 10 } }),
    )
    .start()
    .await?;
    let project = run.project.to_string_lossy().to_string();
    let created = run
        .client
        .call(
            "session.create",
            json!({ "project_root": project, "options": {
                "auto_name": false,
                "settings": { "turn": { "token_budget": 9000, "max_model_calls": 3 } }
            } }),
        )
        .await?;
    let result = created.result.unwrap_or_default();
    assert_eq!(
        result["clamped_options"],
        json!([{ "field": "settings.turn.token_budget", "requested": 9000, "applied": 5000 }]),
        "{result}"
    );
    let s = result["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    stub.script([glob("g1"), glob("g2"), glob("g3")]);
    let events = run.turn(&s, "keep looking").await?;
    assert_eq!(stop_reason(&events), "max_turns", "{events:?}");
    let calls = stub.calls();
    assert_eq!(calls.len(), 3, "the option's tighter call limit holds");
    assert!(
        (4000..=5000).contains(&max_tokens(&calls[0])),
        "the settings' budget holds: {}",
        max_tokens(&calls[0])
    );

    let within = run
        .client
        .call(
            "session.create",
            json!({ "project_root": project, "options": { "settings": { "turn": { "token_budget": 100 } } } }),
        )
        .await?
        .result
        .unwrap_or_default();
    assert!(within.get("clamped_options").is_none(), "{within}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_call_limit_holds_for_it_and_its_sub_agents() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("call-limit", &stub, json!({})).start().await?;
    let own = session(
        &mut run,
        json!({ "settings": { "turn": { "max_model_calls": 2 } } }),
    )
    .await?;
    stub.script([glob("g1"), glob("g2")]);
    let events = run.turn(&own, "keep looking").await?;
    assert_eq!(stop_reason(&events), "max_turns", "{events:?}");
    assert_eq!(stub.call_count(), 2, "a third call went out");

    let parent = session(
        &mut run,
        json!({ "settings": { "turn": { "max_model_calls": 2 } } }),
    )
    .await?;
    stub.script_when(
        ROOT,
        [delegate("look around", false), Reply::text("ROOT-DONE")],
    );
    stub.script([glob("c1"), glob("c2")]);
    let events = run.turn(&parent, &format!("{ROOT} delegate it")).await?;
    assert_eq!(stop_reason(&events), "end_turn", "{events:?}");
    assert!(events.text.contains("ROOT-DONE"), "{events:?}");
    assert_eq!(
        child_calls(&stub).len(),
        4,
        "the sub-agent made a third call"
    );
    let result = agent_result(&events);
    assert!(result.contains("max_turns"), "{result}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_budget_holds_after_a_restart() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("budget-restart", &stub, json!({})).start().await?;
    let s = session(
        &mut run,
        json!({ "settings": { "turn": { "token_budget": 2000 } } }),
    )
    .await?;
    stub.script([Reply::text("hello")]);
    run.turn(&s, "hi").await?;

    run.restart().await?;
    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": s }))
        .await?;
    assert!(resumed.error.is_none(), "{resumed:?}");
    stub.script([Reply::spent(1800, 700, glob("g1"))]);
    let events = run.turn(&s, "look").await?;
    assert_eq!(stop_reason(&events), "budget_exceeded", "{events:?}");
    let calls = stub.calls();
    assert_eq!(calls.len(), 2);
    assert!(
        (1024..2000).contains(&max_tokens(&calls[1])),
        "{}",
        max_tokens(&calls[1])
    );
    run.finish().await;
    Ok(())
}

const TOO_LONG: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 212034 tokens > 200000 maximum"}}"#;

/// A turn whose first call is refused for size compacts, retries, and runs a
/// sub-agent: its `usage` is what the stub reported for every call it
/// answered in that turn.
#[tokio::test(flavor = "multi_thread")]
async fn turn_complete_usage_is_every_call_of_the_turn() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("usage-sum", &stub, json!({})).start().await?;
    let s = session(&mut run, json!({})).await?;
    let filler: String = (0..200).map(|i| format!("line {i} of filler\n")).collect();
    let mut reads = Vec::new();
    for i in 0..8 {
        let file = run.write_file(&format!("f{i}.txt"), &filler)?;
        reads.push(Reply::calls(
            &format!("r{i}"),
            "Read",
            json!({ "file_path": file.to_string_lossy() }),
        ));
    }
    reads.push(Reply::text("read them"));
    stub.script(reads);
    run.turn(&s, "read the files").await?;

    let before = stub.call_count();
    stub.script_when(
        "You are compacting an AI coding agent's conversation",
        [Reply::text("1. User intent — read eight files.")],
    );
    stub.inject_failures(1, Reply::status(400, TOO_LONG));
    stub.script([
        delegate("say hi", false),
        Reply::text("CHILD-HI"),
        Reply::text("ROOT-DONE"),
    ]);
    let events = run.turn(&s, "and now?").await?;
    assert!(events.text.contains("ROOT-DONE"), "{events:?}");
    let answered = stub.call_count() - before - 1;
    assert_eq!(answered, 4, "retry, compaction, sub-agent, last reply");

    let n = answered as u64;
    let complete = events.turn_complete_event.clone().unwrap_or_default();
    assert_eq!(
        complete["usage"],
        json!({
            "input_tokens": blackbox::provider::INPUT_TOKENS * n,
            "output_tokens": blackbox::provider::OUTPUT_TOKENS * n,
            "cache_creation_input_tokens": blackbox::provider::CACHE_CREATION_TOKENS * n,
            "cache_read_input_tokens": blackbox::provider::CACHE_READ_TOKENS * n,
        }),
        "{complete}"
    );
    let calls_of = |part: &str| {
        complete["usage_breakdown"][part]["input_tokens"]
            .as_u64()
            .unwrap_or_default()
            / blackbox::provider::INPUT_TOKENS
    };
    assert_eq!(
        (calls_of("own"), calls_of("side"), calls_of("delegates")),
        (2, 1, 1),
        "{complete}"
    );
    run.finish().await;
    Ok(())
}

/// A persistent member handed work by a turn spends into that turn's budget,
/// whichever of it and the turn's last call comes first.
#[tokio::test(flavor = "multi_thread")]
async fn a_team_member_handed_work_spends_the_turns_budget() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed(
        "member-budget",
        &stub,
        json!({ "team": { "enabled": true } }),
    )
    .start()
    .await?;
    let s = session(
        &mut run,
        json!({ "settings": { "turn": { "token_budget": 10000 } } }),
    )
    .await?;
    stub.script_when(
        ROOT,
        [
            Reply::spent(
                1000,
                100,
                Reply::calls(
                    "m1",
                    "Agent",
                    json!({ "prompt": "look around", "description": "member",
                            "team_name": "crew", "name": "worker" }),
                ),
            ),
            Reply::spent(1000, 100, Reply::text("handed it over")),
        ],
    );
    stub.script([
        Reply::spent_cached(4000, 1000, 500, glob("c1")),
        Reply::spent_cached(4000, 1000, 500, glob("c2")),
    ]);
    run.turn(&s, &format!("{ROOT} hand it over")).await?;
    let tasks = run.wait_tasks_settled(&s, WAIT).await?;
    let task = tasks
        .iter()
        .find(|t| t["kind"] == "teammate")
        .cloned()
        .unwrap_or_default();
    assert_eq!(task["status"], "done", "{tasks:?}");
    assert_eq!(
        child_calls(&stub).len(),
        2,
        "the member called past its turn's budget"
    );
    let output = run
        .client
        .call(
            "session.taskOutput",
            json!({ "session_id": s, "task_id": task["task_id"] }),
        )
        .await?
        .result
        .unwrap_or_default();
    let text = output["output"].as_str().unwrap_or_default();
    assert!(text.contains("budget_exceeded"), "{output}");
    run.finish().await;
    Ok(())
}

const HOOK: &str = "HOOK-MARK";

fn completed(events: &TurnEvents) -> Value {
    events.turn_complete_event.clone().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_hooks_call_is_spent_into_the_turn() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let settings = json!({ "hooks": { "UserPromptSubmit": [
        { "type": "prompt", "prompt": format!("{HOOK} judge this prompt") }
    ] } });
    let mut run = stubbed("hook-spend", &stub, settings).start().await?;
    let s = session(&mut run, json!({})).await?;
    stub.script_when(
        HOOK,
        [Reply::spent(700, 50, Reply::text(r#"{"ok": true}"#))],
    );
    stub.script_when(ROOT, [Reply::spent(1000, 100, Reply::text("done"))]);
    let events = run.turn(&s, ROOT).await?;
    assert_eq!(stub.call_count(), 2);
    let done = completed(&events);
    assert_eq!(done["usage"]["input_tokens"], 1700, "{done}");
    assert_eq!(done["usage"]["output_tokens"], 150, "{done}");
    assert_eq!(
        done["usage_breakdown"]["own"]["input_tokens"], 1000,
        "{done}"
    );
    assert_eq!(
        done["usage_breakdown"]["side"]["input_tokens"], 700,
        "{done}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_hook_asks_for_no_more_than_the_turn_has_left() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let settings = json!({ "hooks": { "PostToolUse": [
        { "type": "prompt", "prompt": format!("{HOOK} judge this call") }
    ] } });
    let mut run = stubbed("hook-headroom", &stub, settings).start().await?;
    let s = session(
        &mut run,
        json!({ "settings": { "turn": { "token_budget": 6000 } } }),
    )
    .await?;
    stub.script_when(HOOK, [Reply::spent(100, 10, Reply::text("{}"))]);
    stub.script_when(
        ROOT,
        [
            Reply::spent_cached(100, 2000, 2900, glob("g1")),
            Reply::text("done"),
        ],
    );
    run.turn(&s, ROOT).await?;
    let hook = stub
        .calls()
        .into_iter()
        .find(|c| c.system_contains(HOOK))
        .expect("the hook's call");
    let asked = max_tokens(&hook);
    assert!(
        (1024..3000).contains(&asked),
        "3000 of the turn's budget is left, less the hook's input: {asked}"
    );
    run.finish().await;
    Ok(())
}

/// The agent hook's sub-agent spends 1600 of the turn's 2000 after the
/// root's first call spent 250: what is left pays for no further call.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_hook_spends_the_turns_budget() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let settings = json!({ "hooks": { "PostToolUse": [
        { "type": "agent", "prompt": format!("{HOOK} check this call"), "if": "Glob", "timeout": 30000 }
    ] } });
    let mut run = stubbed("agent-hook-spend", &stub, settings).start().await?;
    let s = session(
        &mut run,
        json!({ "settings": { "turn": { "token_budget": 2000 } } }),
    )
    .await?;
    stub.script_when(
        HOOK,
        [Reply::spent(1500, 100, Reply::text(r#"{"ok": true}"#))],
    );
    stub.script_when(
        ROOT,
        [
            Reply::spent_cached(200, 1000, 50, glob("g1")),
            Reply::text("done"),
        ],
    );
    let events = run.turn(&s, &format!("{ROOT} look")).await?;
    assert_eq!(stop_reason(&events), "budget_exceeded", "{events:?}");
    assert_eq!(
        stub.call_count(),
        2,
        "the root called past the hook's spend"
    );
    let done = completed(&events);
    assert_eq!(done["usage"]["input_tokens"], 1700, "{done}");
    assert_eq!(
        done["usage_breakdown"]["delegates"]["input_tokens"], 1500,
        "{done}"
    );
    run.finish().await;
    Ok(())
}

fn read(run: &LiveRun, name: &str) -> anyhow::Result<Reply> {
    let lines: String = (0..400)
        .map(|i| format!("line {i} word word word word word\n"))
        .collect();
    let file = run.write_file(name, &lines)?;
    Ok(Reply::calls(
        "r1",
        "Read",
        json!({ "file_path": file.to_string_lossy() }),
    ))
}

/// 3000 of 6000 is left, and the file just read is more than 2000 tokens of
/// new input on its own: the call it would go out with is not sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_whose_input_alone_passes_what_is_left_is_not_sent() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("input-estimate", &stub, json!({})).start().await?;
    let s = session(
        &mut run,
        json!({ "settings": { "turn": { "token_budget": 6000 } } }),
    )
    .await?;
    stub.script([
        Reply::spent_cached(1000, 4000, 2000, read(&run, "big.md")?),
        Reply::spent(21000, 10, Reply::text("done")),
    ]);
    let events = run.turn(&s, "read it").await?;
    assert_eq!(stop_reason(&events), "budget_exceeded", "{events:?}");
    assert_eq!(stub.calls().len(), 1, "the call went out on what was left");
    let message = completed(&events)["message"].clone();
    assert!(
        message.as_str().is_some_and(|m| m.contains("this turn's")),
        "{message}"
    );
    run.finish().await;
    Ok(())
}

/// The first call's prompt was served from the cache: the next one is
/// judged by the little it adds, not by the whole conversation.
#[tokio::test(flavor = "multi_thread")]
async fn a_cached_prefix_does_not_count_against_what_is_left() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("input-cached", &stub, json!({})).start().await?;
    let s = session(
        &mut run,
        json!({ "settings": { "turn": { "token_budget": 6000 } } }),
    )
    .await?;
    stub.script([
        Reply::spent_cached(1000, 20000, 2000, glob("g1")),
        Reply::spent(100, 10, Reply::text("done")),
    ]);
    let events = run.turn(&s, "look").await?;
    assert_eq!(stop_reason(&events), "end_turn", "{events:?}");
    let asked: Vec<u64> = stub.calls().iter().map(max_tokens).collect();
    assert_eq!(asked.len(), 2);
    assert!(
        (2800..3000).contains(&asked[1]),
        "3000 left, less the little the glob added: {asked:?}"
    );
    run.finish().await;
    Ok(())
}

/// The provider read nothing from its cache: the next call resends all the
/// last one was billed for, and that does not fit in what is left.
#[tokio::test(flavor = "multi_thread")]
async fn an_uncached_prompt_counts_again_on_the_next_call() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("input-uncached", &stub, json!({})).start().await?;
    let s = session(
        &mut run,
        json!({ "settings": { "turn": { "token_budget": 6000 } } }),
    )
    .await?;
    stub.script([
        Reply::spent(2500, 500, glob("g1")),
        Reply::spent(100, 10, Reply::text("done")),
    ]);
    let events = run.turn(&s, "look").await?;
    assert_eq!(stop_reason(&events), "budget_exceeded", "{events:?}");
    assert_eq!(stub.calls().len(), 1);
    run.finish().await;
    Ok(())
}

/// Three reads leave 2900 of 30000 and the conversation over the compaction
/// threshold: the summary is a call of the turn like any other, asking for no
/// more than is left once its input is paid.
#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_summary_asks_for_no_more_than_the_turn_has_left() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed(
        "compact-budget",
        &stub,
        json!({ "compaction": { "threshold_tokens": 22000, "keep_recent": 2 } }),
    )
    .start()
    .await?;
    let s = session(
        &mut run,
        json!({ "settings": { "turn": { "token_budget": 30000 } } }),
    )
    .await?;
    let mut script = Vec::new();
    for i in 1..=3 {
        let body: String = (0..600)
            .map(|l| format!("part {i} line {l}: routine reading, all values nominal\n"))
            .collect();
        let path = run.write_file(&format!("part{i}.log"), &body)?;
        let spent = [1000, 10000, 15900][i - 1];
        script.push(Reply::spent_cached(
            spent,
            5000,
            100,
            Reply::calls(
                &format!("p{i}"),
                "Read",
                json!({ "file_path": path.to_string_lossy() }),
            ),
        ));
    }
    script.push(Reply::text("DONE"));
    stub.script(script);
    stub.script_when(
        "You are compacting an AI coding agent's conversation",
        [Reply::text("1. User intent - read the parts.")],
    );
    let events = run.turn(&s, "read the parts").await?;
    let summaries: Vec<u64> = stub
        .calls()
        .iter()
        .filter(|c| c.purpose() == Purpose::Compact)
        .map(max_tokens)
        .collect();
    assert!(
        !summaries.is_empty() && summaries.iter().all(|&asked| asked < 2900),
        "a summary went out on 2900 tokens left asking for {summaries:?}: {events:?}"
    );
    run.finish().await;
    Ok(())
}
