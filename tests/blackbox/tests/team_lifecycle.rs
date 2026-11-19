//! A team's life over JSON-RPC against a model stub: what creating a team
//! under a name already in use does, what deleting one stops, and what a
//! stage reports about its members.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply, SeenRequest};
use serde_json::{json, Value};

const LEAD: &str = "LEAD-MARK";

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .settings(json!({
            "team": { "enabled": true },
            "permission_rules": [
                { "tool": "Agent", "action": "allow" },
                { "tool": "TeamCreate", "action": "allow" },
                { "tool": "TeamDelete", "action": "allow" }
            ]
        }))
}

/// The member's own model calls that carry `mark`, not the lead's.
fn member_calls(stub: &ProviderStub, mark: &str) -> Vec<SeenRequest> {
    stub.calls()
        .into_iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .filter(|c| c.messages_contain(mark) && !c.messages_contain(LEAD))
        .collect()
}

fn message_worker(id: &str, prompt: &str) -> Reply {
    Reply::calls(
        id,
        "Agent",
        json!({ "prompt": prompt, "description": "worker", "team_name": "crew", "name": "worker" }),
    )
}

/// A `TeamCreate` batch under the name of a persistent member's team adds to
/// that team rather than writing over it: `TeamList` still shows the member,
/// and `TeamDelete` stops it, so the next message under its name starts a
/// fresh one.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_under_a_persistent_members_team_name_leaves_the_member_deletable(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("team-same-name", &stub).start().await?;
    let session = run.session().await?;

    stub.script_when(
        LEAD,
        [
            message_worker("w-1", "WORKER-FIRST-MARK hello"),
            Reply::text("sent"),
        ],
    );
    stub.script_when("WORKER-FIRST-MARK", [Reply::text("first done")]);
    run.turn(&session, &format!("{LEAD} message the worker"))
        .await?;
    run.wait_tasks_settled(&session, Duration::from_secs(30))
        .await?;

    stub.script_when(
        LEAD,
        [
            Reply::calls(
                "c-1",
                "TeamCreate",
                json!({ "name": "crew", "stages": [
                    { "name": "only", "agents": [{ "label": "a", "prompt": "BATCH-MARK work" }] }
                ] }),
            ),
            Reply::calls("l-1", "TeamList", json!({})),
            Reply::text("created"),
        ],
    );
    stub.script_when("BATCH-MARK", [Reply::text("batch done")]);
    let created = run.turn(&session, "create the team").await?;

    stub.script_when(
        LEAD,
        [
            Reply::calls("d-1", "TeamDelete", json!({ "name": "crew" })),
            Reply::text("deleted"),
        ],
    );
    run.turn(&session, "delete the team").await?;

    stub.script_when(
        LEAD,
        [
            message_worker("w-2", "WORKER-SECOND-MARK again"),
            Reply::text("sent again"),
        ],
    );
    stub.script_when("WORKER-SECOND-MARK", [Reply::text("second done")]);
    run.turn(&session, "message the worker again").await?;
    run.wait_tasks_settled(&session, Duration::from_secs(30))
        .await?;

    let second = member_calls(&stub, "WORKER-SECOND-MARK");
    anyhow::ensure!(!second.is_empty(), "the second message never ran");
    assert!(
        second
            .iter()
            .all(|c| !c.messages_contain("WORKER-FIRST-MARK")),
        "TeamDelete left the first worker running, and the second message reached it"
    );
    let listed = created
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "TeamList")
        .expect("TeamList answered");
    assert!(
        listed.1.contains("\"crew\" (2 members)") && listed.1.contains("worker"),
        "the batch wrote over the persistent member's team: {listed:?}"
    );
    run.finish().await;
    Ok(())
}

/// A stage whose outputs are merged into one still reports the member that
/// failed, in its `team_progress` and in the team's own event log.
#[tokio::test(flavor = "multi_thread")]
async fn a_merged_stage_still_reports_its_failed_member() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("team-merged-failure", &stub).start().await?;
    let session = run.session().await?;
    stub.script_when(
        LEAD,
        [
            Reply::calls(
                "c-1",
                "TeamCreate",
                json!({ "name": "crew", "stages": [{
                    "name": "only",
                    "aggregate": "aggregate",
                    "agents": [
                        { "label": "good", "prompt": "GOOD-MARK work" },
                        { "label": "bad", "prompt": "BAD-MARK work" }
                    ]
                }] }),
            ),
            Reply::text("done"),
        ],
    );
    stub.script_when("GOOD-MARK", [Reply::text("good result")]);
    stub.script_when(
        "BAD-MARK",
        [Reply::status(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"refused"}}"#,
        )],
    );
    let events = run.turn(&session, &format!("{LEAD} run the team")).await?;

    let completed: Vec<_> = events
        .events
        .iter()
        .filter(|e| e.to_string().contains("team_progress") && e.to_string().contains("completed"))
        .collect();
    anyhow::ensure!(
        !completed.is_empty(),
        "no stage completed: {:?}",
        events.events
    );
    for event in &completed {
        assert!(
            event.to_string().contains(r#""failed":["bad"]"#),
            "the merged stage lost its failed member: {event}"
        );
    }
    let log = std::fs::read_dir(run.project.join(".agix/state/teams"))?
        .flatten()
        .map(|e| std::fs::read_to_string(e.path().join("events.jsonl")))
        .collect::<Result<String, _>>()?;
    let stage_completed: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("stage_completed"))
        .collect();
    assert!(
        !stage_completed.is_empty() && stage_completed.iter().all(|l| l.contains(r#""bad""#)),
        "{log}"
    );
    run.finish().await;
    Ok(())
}

fn turns_with(stub: &ProviderStub, mark: &str, not: &[&str]) -> Vec<SeenRequest> {
    stub.calls()
        .into_iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .filter(|c| c.messages_contain(mark) && not.iter().all(|n| !c.messages_contain(n)))
        .collect()
}

async fn wait_until(what: &str, timeout: Duration, done: impl Fn() -> bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while !done() {
        anyhow::ensure!(Instant::now() < deadline, "{what} never happened");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

async fn interrupt(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    let resp = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    Ok(resp.result.unwrap_or_default())
}

fn team_dirs(project: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(project.join(".agix/state/teams"))
        .map(|dir| dir.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// A persistent member stopped with a message still queued for it ends that
/// message's task too, so nothing is left running to hold off a rebuild.
#[tokio::test(flavor = "multi_thread")]
async fn a_member_stopped_with_a_queued_message_leaves_no_task_running() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("member-queue", &stub)
        .settings(json!({
            "team": { "enabled": true },
            "permission_rules": [
                { "tool": "Agent", "action": "allow" },
                { "tool": "TeamDelete", "action": "allow" }
            ]
        }))
        .start()
        .await?;
    let session = run.session().await?;
    let member = |id: &str, prompt: &str| {
        Reply::calls(
            id,
            "Agent",
            json!({ "prompt": prompt, "description": "member", "team_name": "crew", "name": "worker" }),
        )
    };
    stub.script_when(
        "LEAD-MARK",
        [
            member("a-1", "FIRST-MESSAGE-MARK work"),
            member("a-2", "SECOND-MESSAGE-MARK then this"),
            Reply::text("queued"),
        ],
    );
    stub.script_when(
        "FIRST-MESSAGE-MARK",
        [Reply::delayed(
            Duration::from_secs(120),
            Reply::text("first done"),
        )],
    );
    run.turn(&session, "LEAD-MARK give the worker two messages")
        .await?;
    wait_until("the member's first turn", Duration::from_secs(30), || {
        !turns_with(&stub, "FIRST-MESSAGE-MARK", &["LEAD-MARK"]).is_empty()
    })
    .await?;
    stub.script_when(
        "STOP-MARK",
        [
            Reply::calls("d-1", "TeamDelete", json!({ "name": "crew" })),
            Reply::text("stopped"),
        ],
    );
    run.turn(&session, "STOP-MARK stop the team").await?;
    run.wait_tasks_settled(&session, Duration::from_secs(30))
        .await?;
    let set = run
        .client
        .call(
            "session.setPrompt",
            json!({ "session_id": session, "append": "AFTER-THE-TEAM" }),
        )
        .await?;
    assert!(set.error.is_none(), "{set:?}");
    run.finish().await;
    Ok(())
}

/// The lead is interrupted while its first stage's member is still waiting
/// on the model: the team ends there, as cancelled, and its second stage is
/// never started.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_team_starts_no_later_stage_and_ends_cancelled() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("team-interrupted", &stub)
        .settings(json!({ "team": { "enabled": true } }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script_when(
        "LEAD-MARK",
        [
            Reply::calls(
                "t1",
                "TeamCreate",
                json!({ "name": "crew", "stages": [
                    { "name": "one", "agents": [{ "label": "first", "prompt": "STAGE-ONE-MARK work" }] },
                    { "name": "two", "agents": [{ "label": "second", "prompt": "STAGE-TWO-MARK work" }] }
                ] }),
            ),
            Reply::text("LEAD-DONE"),
        ],
    );
    stub.script_when(
        "STAGE-ONE-MARK",
        [Reply::delayed(
            Duration::from_secs(120),
            Reply::text("one done"),
        )],
    );
    stub.script_when("STAGE-TWO-MARK", [Reply::text("two done")]);
    let handle = run
        .spawn_turn(&session, "LEAD-MARK run the team", json!({}))
        .await?;
    wait_until(
        "the first stage's member calling the model",
        Duration::from_secs(30),
        || !turns_with(&stub, "STAGE-ONE-MARK", &["LEAD-MARK"]).is_empty(),
    )
    .await?;
    let interrupted = interrupt(&mut run, &session).await?;
    assert_eq!(interrupted["interrupted"], json!(true), "{interrupted}");
    let events = tokio::time::timeout(Duration::from_secs(60), handle).await???;
    let stop = events
        .turn_complete_event
        .as_ref()
        .map(|e| e["stop_reason"].clone())
        .unwrap_or_default();
    assert_eq!(stop, "cancelled", "{:?}", events.events);

    let team = team_dirs(&run.project)
        .into_iter()
        .next()
        .expect("the team's directory");
    let state: Value = serde_json::from_str(&std::fs::read_to_string(team.join("state.json"))?)?;
    assert_eq!(state["status"], "cancelled", "{state}");
    let log = std::fs::read_to_string(team.join("events.jsonl"))?;
    let later_stage = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|e| e["kind"] == "stage_started" && e["index"] == 1);
    assert!(
        later_stage.is_none(),
        "the second stage was started: {later_stage:?}"
    );
    run.finish().await;
    Ok(())
}
