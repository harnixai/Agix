//! Settings that exist only to bound or replace something the engine does:
//! each is set, and what the model's side sees is what the setting says.
//! A setting that is parsed and trusted but read by nothing looks exactly
//! like one that works until a case like these asks.

use std::time::{Duration, Instant};

use blackbox::{Block, LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply};
use serde_json::{json, Value};

const LEAD: &str = "LEAD-MARK";

fn stubbed(name: &str, stub: &ProviderStub, settings: Value) -> LiveRunBuilder {
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .settings(settings)
}

async fn until(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    ready()
}

fn asked_with(stub: &ProviderStub, mark: &str) -> bool {
    stub.calls().iter().any(|c| {
        c.purpose() == Purpose::Turn && c.messages_contain(mark) && !c.messages_contain(LEAD)
    })
}

fn turn_calls(stub: &ProviderStub) -> usize {
    stub.calls()
        .iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .count()
}

fn todo_call(id: &str, content: &str) -> Reply {
    Reply::calls(
        id,
        "TodoWrite",
        json!({ "todos": [{ "content": content, "status": "pending", "activeForm": "Working" }] }),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn execution_max_api_calls_per_turn_ends_a_turn_that_keeps_calling_tools(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed(
        "max-api-calls",
        &stub,
        json!({ "execution": { "max_api_calls_per_turn": 2 } }),
    )
    .start()
    .await?;
    stub.script_when("标题生成器", [Reply::text("title")]);
    let session = run.session().await?;
    stub.script((0..6).map(|i| todo_call(&format!("t{i}"), &format!("step {i}"))));

    let turn = run.turn(&session, "keep going").await?;
    assert_eq!(
        turn_calls(&stub),
        2,
        "the turn made more model calls than allowed"
    );
    let done = turn.turn_complete_event.expect("turn_complete");
    assert_eq!(done["stop_reason"], "max_turns", "{done}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_override_replaces_the_whole_system_prompt() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed(
        "prompt-override",
        &stub,
        json!({ "prompt_override": "OVERRIDE-ONLY-PROMPT" }),
    )
    .start()
    .await?;
    stub.script_when("标题生成器", [Reply::text("title")]);
    let session = run.session().await?;
    stub.script([Reply::text("answered")]);
    run.turn(&session, "hello").await?;

    let call = stub
        .calls()
        .into_iter()
        .find(|c| c.purpose() == Purpose::Turn)
        .expect("a turn call");
    assert_eq!(
        call.system_text().trim(),
        "OVERRIDE-ONLY-PROMPT",
        "{}",
        call.system_text()
    );
    assert!(
        call.has_tools(),
        "an overridden prompt still gets the tool table"
    );
    run.finish().await;
    Ok(())
}

fn two_agents() -> Reply {
    Reply::Blocks(vec![
        Block::tool(
            "a1",
            "Agent",
            json!({ "description": "first", "prompt": "SUB-A-MARK work" }),
        ),
        Block::tool(
            "a2",
            "Agent",
            json!({ "description": "second", "prompt": "SUB-B-MARK work" }),
        ),
    ])
}

/// Whether the second sibling's model call arrived while the first's was
/// still unanswered.
async fn second_overlaps_first(name: &str, settings: Value) -> anyhow::Result<bool> {
    second_overlaps_first_of(name, settings, two_agents()).await
}

async fn second_overlaps_first_of(
    name: &str,
    settings: Value,
    fan_out: Reply,
) -> anyhow::Result<bool> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed(name, &stub, settings).start().await?;
    stub.script_when("标题生成器", [Reply::text("title")]);
    let session = run.session().await?;
    stub.script_when(LEAD, [fan_out, Reply::text("both done")]);
    stub.script_when(
        "SUB-A-MARK",
        [Reply::delayed(
            Duration::from_secs(6),
            Reply::text("a done"),
        )],
    );
    stub.script_when("SUB-B-MARK", [Reply::text("b done")]);

    let started = {
        let session = session.clone();
        run.spawn_turn(&session, &format!("{LEAD} fan out"), json!({}))
            .await?
    };
    anyhow::ensure!(
        until(Duration::from_secs(20), || asked_with(&stub, "SUB-A-MARK")).await,
        "the first sub-agent never asked the model"
    );
    let overlapped = until(Duration::from_secs(2), || asked_with(&stub, "SUB-B-MARK")).await;
    started.await??;
    anyhow::ensure!(
        asked_with(&stub, "SUB-B-MARK"),
        "the second sub-agent never ran at all"
    );
    run.finish().await;
    Ok(overlapped)
}

fn allow_agents(extra: Value) -> Value {
    let mut settings = json!({
        "permissions": {"allow": ["Agent", "TeamCreate"]},
        "team": { "enabled": true }
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        settings[k] = v;
    }
    settings
}

#[tokio::test(flavor = "multi_thread")]
async fn sibling_tool_calls_overlap_by_default() -> anyhow::Result<()> {
    assert!(
        second_overlaps_first("parallel-default", allow_agents(json!({}))).await?,
        "two concurrency-safe calls in one reply did not run together"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn execution_max_parallelism_one_runs_sibling_tool_calls_one_at_a_time() -> anyhow::Result<()>
{
    let overlapped = second_overlaps_first(
        "parallel-one",
        allow_agents(json!({ "execution": { "max_parallelism": 1 } })),
    )
    .await?;
    assert!(
        !overlapped,
        "max_parallelism 1 still ran two tool calls together"
    );
    Ok(())
}

fn a_stage_of_two() -> Reply {
    Reply::calls(
        "team-1",
        "TeamCreate",
        json!({ "name": "crew", "stages": [{
            "name": "only",
            "agents": [
                { "label": "a", "prompt": "SUB-A-MARK work" },
                { "label": "b", "prompt": "SUB-B-MARK work" }
            ]
        }] }),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stages_members_run_together_by_default() -> anyhow::Result<()> {
    assert!(
        second_overlaps_first_of("stage-default", allow_agents(json!({})), a_stage_of_two())
            .await?,
        "the members of one stage did not run together"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn team_stage_concurrency_one_runs_a_stages_members_one_at_a_time() -> anyhow::Result<()> {
    let overlapped = second_overlaps_first_of(
        "stage-one",
        allow_agents(json!({ "team": { "enabled": true, "stage_concurrency": 1 } })),
        a_stage_of_two(),
    )
    .await?;
    assert!(
        !overlapped,
        "stage_concurrency 1 still ran two members together"
    );
    Ok(())
}
