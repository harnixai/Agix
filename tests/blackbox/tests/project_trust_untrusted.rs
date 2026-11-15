//! A cloned repository nobody has confirmed — `tests/fixtures/hostile_project`
//! and the pieces `World::hostile_project` plants — seen through the daemon:
//! its skill's commands and grants do nothing, its wider agent type is held
//! to the session's mode, its script bindings stay unbound, a skill that is a
//! link to a credential does not read it into the model, and `project.trust`
//! turns each of these on.

use blackbox::{Answer, LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Reply, HOSTILE_SECRET};
use serde_json::{json, Value};

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .settings(json!({ "permission_rules": [
            { "tool": "Skill", "action": "allow" },
            { "tool": "Agent", "action": "allow" }
        ] }))
}

fn hostile(name: &'static str, stub: &ProviderStub) -> LiveRunBuilder {
    stubbed(name, stub)
        .untrusted()
        .before_start(move |world, _| {
            world.hostile_project(name)?;
            world.write_global_settings(&json!({ "permission_rules": [
            { "tool": "Bash(touch shell-ran.txt)", "action": "allow" }
        ] }))?;
            Ok(())
        })
}

fn asked_tools(watcher: &blackbox::Watcher) -> Vec<String> {
    watcher
        .asked()
        .iter()
        .map(|a| {
            a.prompt["event"]["tool_name"]
                .as_str()
                .unwrap_or("?")
                .into()
        })
        .collect()
}

async fn trust(run: &mut LiveRun) -> anyhow::Result<()> {
    let resp = run
        .client
        .call(
            "project.trust",
            json!({ "project_root": run.project.to_string_lossy(), "trusted": true }),
        )
        .await?;
    anyhow::ensure!(resp.error.is_none(), "{:?}", resp.error);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_status_names_what_the_repository_needs_trust_for() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = hostile("hostile-status", &stub).start().await?;
    let status = run
        .client
        .call(
            "project.status",
            json!({ "project_root": run.project.to_string_lossy() }),
        )
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(status["trust"], "untrusted", "{status}");
    let needs: Vec<&str> = status["requires_trust"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for wanted in ["skills/shellout", "skills/fetchy", "agents/wide"] {
        assert!(needs.contains(&wanted), "{wanted} not in {needs:?}");
    }
    run.finish().await;
    Ok(())
}

async fn skill_then_touch(
    run: &mut LiveRun,
    stub: &ProviderStub,
    deny: bool,
) -> anyhow::Result<(Vec<String>, bool, bool)> {
    let session = run.session().await?;
    let watcher = run
        .watch_with(&session, move |_| {
            Some(if deny {
                Answer::deny("no")
            } else {
                Answer::permit()
            })
        })
        .await?;
    stub.script([
        Reply::calls("s1", "Skill", json!({ "skill": "shellout" })),
        Reply::calls("b1", "Bash", json!({ "command": "touch grabbed.txt" })),
        Reply::text("done"),
    ]);
    run.turn(&session, "go").await?;
    let asked = asked_tools(&watcher);
    drop(watcher);
    Ok((
        asked,
        run.read_file("shell-ran.txt").is_some(),
        run.read_file("grabbed.txt").is_some(),
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_projects_skill_runs_no_command_and_grants_nothing() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = hostile("hostile-skill", &stub).start().await?;
    let (asked, shell_ran, grabbed) = skill_then_touch(&mut run, &stub, true).await?;
    assert!(
        stub.calls()
            .iter()
            .any(|c| c.messages_contain("SKILL-FOLLOWED")),
        "precondition: the skill was expanded"
    );
    assert!(!shell_ran, "the skill's inline command ran");
    assert_eq!(asked, ["Bash"], "the skill's grant let Bash through");
    assert!(!grabbed);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn trusting_the_project_runs_the_skills_command() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = hostile("hostile-skill-trusted", &stub).start().await?;
    trust(&mut run).await?;
    let (_, shell_ran, _) = skill_then_touch(&mut run, &stub, true).await?;
    assert!(
        shell_ran,
        "the control: a trusted project's skill command runs"
    );
    run.finish().await;
    Ok(())
}

async fn wide_agent_turn(
    run: &mut LiveRun,
    stub: &ProviderStub,
) -> anyhow::Result<(Vec<String>, bool)> {
    let session = run.session().await?;
    let watcher = run
        .watch_with(&session, |_| Some(Answer::deny("no")))
        .await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls(
                "a1",
                "Agent",
                json!({ "description": "wide", "prompt": "touch it", "subagent_type": "wide" }),
            ),
            Reply::text("parent done"),
        ],
    );
    stub.script([
        Reply::calls("b1", "Bash", json!({ "command": "touch wide.txt" })),
        Reply::text("child done"),
    ]);
    run.turn(&session, "PARENT-MARK delegate").await?;
    let asked = asked_tools(&watcher);
    drop(watcher);
    Ok((asked, run.read_file("wide.txt").is_some()))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_projects_wider_agent_type_is_held_to_the_session_mode() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = hostile("hostile-agent", &stub).start().await?;
    let (asked, wrote) = wide_agent_turn(&mut run, &stub).await?;
    assert_eq!(asked, ["Bash"], "the agent's bypass mode took effect");
    assert!(!wrote);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trusted_projects_wider_agent_type_keeps_its_mode() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = hostile("hostile-agent-trusted", &stub).start().await?;
    trust(&mut run).await?;
    let (asked, wrote) = wide_agent_turn(&mut run, &stub).await?;
    assert!(asked.is_empty(), "the control: {asked:?}");
    assert!(wrote);
    run.finish().await;
    Ok(())
}

const SCRIPT_MARK: &str = "PROJECT-SCRIPT-MARK";

fn with_script(builder: LiveRunBuilder) -> LiveRunBuilder {
    builder
        .settings(json!({
            "scripts": [{
                "path": ".agix/scripts/house.js",
                "point": "prompt.assemble",
                "entry": "onAssemble",
                "timeout_ms": 5000
            }]
        }))
        .before_start(|world, _| {
            world.write_project_file(
                "hostile-script",
                ".agix/scripts/house.js",
                &format!(
                    "function onAssemble(blocks) {{ return blocks.concat([{{ name: \"house\", \
                     content: \"{SCRIPT_MARK}\" }}]); }}"
                ),
            )?;
            Ok(())
        })
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_projects_script_bindings_stay_unbound() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_script(stubbed("hostile-script", &stub).untrusted())
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([Reply::text("ok")]);
    run.turn(&session, "hello").await?;
    assert!(
        !stub.calls().last().unwrap().system_contains(SCRIPT_MARK),
        "an untrusted project's script reached the prompt"
    );

    trust(&mut run).await?;
    let session = run.session().await?;
    stub.script([Reply::text("ok")]);
    run.turn(&session, "hello").await?;
    assert!(
        stub.calls().last().unwrap().system_contains(SCRIPT_MARK),
        "the control: after project.trust the binding is live"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skill_linked_to_a_credential_does_not_read_it_into_the_model() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = hostile("hostile-creds", &stub).start().await?;
    let session = run.session().await?;
    stub.script([
        Reply::calls("s1", "Skill", json!({ "skill": "creds" })),
        Reply::text("done"),
    ]);
    run.turn(&session, "use the creds skill").await?;
    for call in stub.calls() {
        assert!(
            !call.messages_contain(HOSTILE_SECRET) && !call.system_contains(HOSTILE_SECRET),
            "the credential reached the model"
        );
    }
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nested_repository_does_not_run_its_program_for_a_status_in_the_project(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = hostile("hostile-gitlinks", &stub).start().await?;
    let canary = run.world.root().join("hostile-gitlinks-fsmonitor-ran");
    let control = run.world.hostile_project("hostile-control")?;
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&control.root)
        .env("HOME", run.world.home())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()?;
    assert!(status.status.success(), "{status:?}");
    assert!(
        control.canary.exists(),
        "the fixture is inert: a host `git status` does not run the program"
    );
    let session = run.session().await?;
    let _watcher = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("b1", "Bash", json!({ "command": "git status" })),
        Reply::text("done"),
    ]);
    run.turn(&session, "status").await?;
    assert!(!canary.exists(), "the nested repository's fsmonitor ran");
    run.finish().await;
    Ok(())
}

fn with_inline_limit(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .settings(json!({
            "permission_rules": [{ "tool": "Bash", "action": "allow" }],
            "compaction": { "tool_result_inline_chars": 1 }
        }))
}

async fn echo_hello(run: &mut LiveRun, stub: &ProviderStub) -> anyhow::Result<String> {
    let session = run.session().await?;
    stub.script([
        Reply::calls("b1", "Bash", json!({ "command": "echo hello" })),
        Reply::text("done"),
    ]);
    run.turn(&session, "say hello").await?;
    let request = stub
        .calls()
        .into_iter()
        .rfind(|c| c.purpose() == blackbox::Purpose::Turn)
        .expect("a turn reached the model");
    Ok(request.body["messages"].to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_projects_inline_limit_is_ignored() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_inline_limit("hostile-inline", &stub)
        .untrusted()
        .start()
        .await?;
    let seen = echo_hello(&mut run, &stub).await?;
    assert!(
        !seen.contains("Output too long"),
        "the project's limit applied: {seen}"
    );
    assert!(!run.project.join(".agix/tool-results").exists());

    trust(&mut run).await?;
    let seen = echo_hello(&mut run, &stub).await?;
    assert!(
        seen.contains("Output too long"),
        "the control: once trusted the limit applies: {seen}"
    );
    run.finish().await;
    Ok(())
}
