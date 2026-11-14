//! A team's life over JSON-RPC against a model stub: what creating a team
//! under a name already in use does, what deleting one stops, and what a
//! stage reports about its members.

use std::time::Duration;

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply, SeenRequest};
use serde_json::json;

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
