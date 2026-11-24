//! An agent type the session does not have, named by the model's `Agent`
//! call, a team member or the host's `session.startAgent`, is refused with
//! the names there are; no sub-agent starts. Leaving the type out, or naming
//! `general-purpose`, still runs a general-purpose one.

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply};
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
            "permissions": {"allow": ["Agent", "TeamCreate"]}
        }))
        .before_start(|_world, project| {
            let dir = project.join(".agix/agents");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join("auditor.md"),
                "---\ndescription: audits\n---\nAudit it.\n",
            )?;
            Ok(())
        })
}

/// The sub-agents' own calls carrying `mark`.
fn child_calls(stub: &ProviderStub, mark: &str) -> usize {
    stub.calls()
        .into_iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .filter(|c| c.messages_contain(mark) && !c.messages_contain(LEAD))
        .count()
}

fn answers(turn: &rpc_client::TurnEvents, tool: &str) -> Vec<(String, bool)> {
    turn.tool_results
        .iter()
        .filter(|(name, _, _)| name == tool)
        .map(|(_, text, error)| (text.clone(), *error))
        .collect()
}

fn names_the_types(text: &str, unknown: &str) {
    assert!(text.contains(&format!("`{unknown}`")), "{text}");
    for known in ["auditor", "explore", "general-purpose"] {
        assert!(text.contains(known), "{known} missing: {text}");
    }
}

fn agent(id: &str, prompt: &str, extra: Value) -> Reply {
    let mut input = json!({ "prompt": prompt, "description": "helper" });
    if let (Some(i), Some(e)) = (input.as_object_mut(), extra.as_object()) {
        i.extend(e.clone());
    }
    Reply::calls(id, "Agent", input)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_models_agent_call_naming_no_such_type_is_refused_with_the_types() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("unknown-type-agent-tool", &stub).start().await?;
    let session = run
        .session_with(json!({"options": {"auto_name": false}}))
        .await?;
    stub.script_when(
        LEAD,
        [
            agent(
                "a-1",
                "KID-NONESUCH look",
                json!({"subagent_type": "nonesuch"}),
            ),
            agent(
                "a-2",
                "KID-MEMBER look",
                json!({"subagent_type": "nonesuch", "team_name": "crew", "name": "w"}),
            ),
            agent("a-3", "KID-PLAIN look", json!({})),
            agent(
                "a-4",
                "KID-GENERAL look",
                json!({"subagent_type": "general-purpose"}),
            ),
            Reply::text("lead done"),
        ],
    );
    stub.script_when("KID-PLAIN", [Reply::text("plain done")]);
    stub.script_when("KID-GENERAL", [Reply::text("general done")]);
    let turn = run.turn(&session, &format!("{LEAD} delegate")).await?;

    let got = answers(&turn, "Agent");
    assert_eq!(got.len(), 4, "{got:?}");
    for (text, is_error) in &got[..2] {
        assert!(is_error, "{text}");
        names_the_types(text, "nonesuch");
    }
    assert!(!got[2].1 && got[2].0.contains("plain done"), "{got:?}");
    assert!(!got[3].1 && got[3].0.contains("general done"), "{got:?}");
    assert_eq!(child_calls(&stub, "KID-NONESUCH"), 0);
    assert_eq!(child_calls(&stub, "KID-MEMBER"), 0);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn start_agent_naming_no_such_type_is_invalid_params_and_starts_no_task() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("unknown-type-start-agent", &stub).start().await?;
    let session = run
        .session_with(json!({"options": {"auto_name": false}}))
        .await?;
    let response = run
        .client
        .call(
            "session.startAgent",
            json!({"session_id": session, "prompt": "KID-HOST look", "subagent_type": "nonesuch"}),
        )
        .await?;
    let error = response.error.expect("refused");
    assert_eq!(error.code, -32602, "{error:?}");
    names_the_types(&error.message, "nonesuch");
    let listed = run
        .client
        .call("session.tasks", json!({ "session_id": session }))
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(listed["tasks"], json!([]), "{listed}");
    assert_eq!(child_calls(&stub, "KID-HOST"), 0);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_team_naming_no_such_type_starts_none_of_its_members() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("unknown-type-team", &stub).start().await?;
    let session = run
        .session_with(json!({"options": {"auto_name": false}}))
        .await?;
    stub.script_when(
        LEAD,
        [
            Reply::calls(
                "t-1",
                "TeamCreate",
                json!({ "name": "crew", "stages": [
                    { "name": "first", "agents": [{ "label": "a", "prompt": "KID-FIRST work" }] },
                    { "name": "second", "agents": [
                        { "label": "b", "prompt": "KID-SECOND work", "agent_type": "nonesuch" }
                    ] }
                ] }),
            ),
            Reply::text("lead done"),
        ],
    );
    stub.script_when("KID-FIRST", [Reply::text("first done")]);
    let turn = run.turn(&session, &format!("{LEAD} run a team")).await?;

    let got = answers(&turn, "TeamCreate");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].1, "{got:?}");
    names_the_types(&got[0].0, "nonesuch");
    assert_eq!(child_calls(&stub, "KID-FIRST"), 0);
    assert_eq!(child_calls(&stub, "KID-SECOND"), 0);
    run.finish().await;
    Ok(())
}
