//! `session.setPermissionMode` from the host's side, with a scripted model:
//! a switch reaches the tool calls of the turn already running, is held to
//! the settings and says so, outlives a restart, enters and leaves plan the
//! way the model's own `EnterPlanMode` / `ExitPlanMode` do, wins over an exit
//! still waiting for approval, reaches a session that is not in memory, and
//! is pushed to watchers as `permission_mode_changed` together with the
//! model's own switches.

use std::time::Duration;

use blackbox::{Answer, LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Reply, Watcher};
use serde_json::{json, Value};

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

async fn set_mode(run: &mut LiveRun, session: &str, mode: &str) -> anyhow::Result<Value> {
    let resp = run
        .client
        .call(
            "session.setPermissionMode",
            json!({ "session_id": session, "mode": mode }),
        )
        .await?;
    match resp.result {
        Some(result) => Ok(result),
        None => anyhow::bail!("session.setPermissionMode {mode}: {:?}", resp.error),
    }
}

async fn resumed_mode(run: &mut LiveRun, session: &str, extra: Value) -> anyhow::Result<Value> {
    let mut params = json!({ "session_id": session });
    if let (Some(p), Some(e)) = (params.as_object_mut(), extra.as_object()) {
        p.extend(e.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    let resp = run.client.call("session.resume", params).await?;
    Ok(resp.result.unwrap_or_default()["permission"]["mode"].clone())
}

fn write(id: &str, path: &str, content: &str) -> Reply {
    Reply::calls(
        id,
        "Write",
        json!({ "file_path": path, "content": content }),
    )
}

fn mode_frames(watcher: &Watcher) -> Vec<(String, String)> {
    watcher
        .events("permission_mode_changed")
        .iter()
        .map(|f| {
            (
                f["event"]["mode"].as_str().unwrap_or("?").to_string(),
                f["event"]["source"].as_str().unwrap_or("?").to_string(),
            )
        })
        .collect()
}

fn pair(mode: &str, source: &str) -> (String, String) {
    (mode.to_string(), source.to_string())
}

async fn wait_frames(watcher: &Watcher, count: usize) -> Vec<(String, String)> {
    for _ in 0..100 {
        if mode_frames(watcher).len() >= count {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    mode_frames(watcher)
}

async fn listed_status(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    let resp = run.client.call("session.list", json!({})).await?;
    let sessions = resp.result.unwrap_or_default()["sessions"].clone();
    Ok(sessions
        .as_array()
        .and_then(|all| all.iter().find(|s| s["session_id"] == session))
        .map(|s| s["status"].clone())
        .unwrap_or_default())
}

async fn answer(run: &mut LiveRun, session: &str, prompt: &Value, decision: Value) {
    let resp = run
        .client
        .call(
            "session.respondToPrompt",
            json!({
                "session_id": session,
                "prompt_id": prompt["event"]["prompt_id"],
                "prompt_type": "permission",
                "decision": decision,
            }),
        )
        .await
        .expect("respondToPrompt");
    assert!(resp.error.is_none(), "{:?}", resp.error);
}

/// The host switches to plan while a command waits for approval; the `Write`
/// later in the same turn is judged in plan mode — refused, with no
/// question — and the model is told of the switch.
#[tokio::test(flavor = "multi_thread")]
async fn a_switch_reaches_the_later_calls_of_the_running_turn() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("mode-mid-turn", &stub).start().await?;
    let session = run.session().await?;
    let watcher = run.watch_with(&session, |_| None).await?;
    stub.script([
        Reply::calls("b1", "Bash", json!({ "command": "touch first.txt" })),
        write("w2", "second.txt", "2"),
        Reply::text("done"),
    ]);
    let turn = run
        .spawn_turn(&session, "touch, then write", json!({}))
        .await?;
    let asked = watcher
        .wait_for("the command's prompt", Duration::from_secs(30), |e| {
            e["kind"] == "prompt"
        })
        .await?;

    let switched = set_mode(&mut run, &session, "plan").await?;
    assert_eq!(switched["mode"], "plan", "{switched}");
    assert_eq!(switched["clamped"], false, "{switched}");
    answer(&mut run, &session, &asked, Answer::deny("NOT-THAT")).await;
    let events = turn.await??;

    assert!(
        run.read_file("second.txt").is_none(),
        "{:?}",
        events.tool_results
    );
    let second = &events.tool_results[1];
    assert!(
        second.2 && second.1.contains("plan mode"),
        "the write was not judged in plan mode: {:?}",
        events.tool_results
    );
    assert_eq!(watcher.asked().len(), 1, "only the command asked");
    let told = stub
        .calls()
        .last()
        .is_some_and(|c| c.messages_contain("switched this session to plan mode"));
    assert!(told, "the model was not told of the switch");
    assert_eq!(resumed_mode(&mut run, &session, json!({})).await?, "plan");
    run.finish().await;
    Ok(())
}

/// Wider than the settings is clamped, and the reply says so; with
/// `allow_client_permission_override` the host's mode stands. A mode only the
/// engine sets, a mode that does not exist and a session that does not are
/// refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_switch_is_held_to_the_settings_and_says_when_it_was() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("mode-clamp", &stub).start().await?;
    let session = run.session().await?;
    let clamped = set_mode(&mut run, &session, "bypassPermissions").await?;
    assert_eq!(
        clamped,
        json!({ "session_id": session, "mode": "default",
                "requested": "bypassPermissions", "clamped": true })
    );
    assert_eq!(
        resumed_mode(&mut run, &session, json!({})).await?,
        "default"
    );
    let narrower = set_mode(&mut run, &session, "dontAsk").await?;
    assert_eq!(narrower["mode"], "dontAsk");
    assert_eq!(narrower["clamped"], false);

    for (mode, code) in [("bubble", -32602), ("plann", -32602)] {
        let resp = run
            .client
            .call(
                "session.setPermissionMode",
                json!({ "session_id": session, "mode": mode }),
            )
            .await?;
        let error = resp.error.expect("refused");
        assert_eq!(error.code, code, "{mode}: {error:?}");
    }
    let missing = run
        .client
        .call(
            "session.setPermissionMode",
            json!({ "session_id": "01JZZZZZZZZZZZZZZZZZZZZZZZ", "mode": "plan" }),
        )
        .await?;
    assert!(missing.error.is_some(), "{missing:?}");
    run.finish().await;

    let stub = ProviderStub::start().await?;
    let mut run = stubbed("mode-override", &stub)
        .settings(json!({ "allow_client_permission_override": true }))
        .start()
        .await?;
    let session = run.session().await?;
    // bypass: the case is whether a host may widen the mode when the settings let it.
    let widened = set_mode(&mut run, &session, "bypassPermissions").await?;
    assert_eq!(widened["mode"], "bypassPermissions", "{widened}");
    assert_eq!(widened["clamped"], false, "{widened}");
    run.finish().await;
    Ok(())
}

/// A host's mode is recorded: after a restart the session comes back in it,
/// over the mode it was created with and over a `session.resume`'s
/// `options`; one the host put in plan comes back in plan.
#[tokio::test(flavor = "multi_thread")]
async fn a_switched_mode_outlives_a_restart() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("mode-restart", &stub)
        .settings(json!({ "permission_mode": "acceptEdits" }))
        .start()
        .await?;
    let edits = run
        .session_with(json!({ "options": { "permission_mode": "default" } }))
        .await?;
    let planning = run.session().await?;
    stub.script([Reply::text("one")]);
    run.turn(&edits, "hello").await?;
    stub.script([Reply::text("two")]);
    run.turn(&planning, "hello").await?;
    set_mode(&mut run, &edits, "acceptEdits").await?;
    set_mode(&mut run, &planning, "plan").await?;

    run.restart().await?;
    assert_eq!(
        resumed_mode(
            &mut run,
            &edits,
            json!({ "options": { "permission_mode": "default" } })
        )
        .await?,
        "acceptEdits"
    );
    assert_eq!(resumed_mode(&mut run, &planning, json!({})).await?, "plan");

    stub.script([write("w1", "edited.txt", "x"), Reply::text("done")]);
    let events = run.turn(&edits, "write it").await?;
    assert_eq!(
        run.read_file("edited.txt").as_deref(),
        Some("x"),
        "{:?}",
        events.tool_results
    );
    run.finish().await;
    Ok(())
}

/// Plan entered by the host is left by the model's `ExitPlanMode` back to
/// the mode it was entered from; every switch, the host's and the model's,
/// reaches a watcher as a `permission_mode_changed` frame.
#[tokio::test(flavor = "multi_thread")]
async fn plan_entered_by_the_host_is_left_to_the_mode_before_it() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("mode-host-plan", &stub)
        .settings(json!({ "permission_mode": "acceptEdits" }))
        .start()
        .await?;
    let session = run.session().await?;
    let watcher = run
        .watch_with(&session, |e| {
            (e["tool_name"] == "ExitPlanMode").then(Answer::permit)
        })
        .await?;
    set_mode(&mut run, &session, "plan").await?;
    stub.script([
        write("w1", "during.txt", "x"),
        Reply::calls("p1", "ExitPlanMode", json!({})),
        write("w2", "after.txt", "y"),
        Reply::calls("p2", "EnterPlanMode", json!({})),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "plan, then do").await?;

    assert!(
        run.read_file("during.txt").is_none(),
        "{:?}",
        events.tool_results
    );
    assert_eq!(
        run.read_file("after.txt").as_deref(),
        Some("y"),
        "back in acceptEdits an edit asks no one: {:?}",
        events.tool_results
    );
    assert_eq!(
        wait_frames(&watcher, 3).await,
        [
            pair("plan", "host"),
            pair("acceptEdits", "model"),
            pair("plan", "model"),
        ]
    );
    run.finish().await;
    Ok(())
}

/// The model's `ExitPlanMode` waits for approval while the host leaves plan
/// and enters it again: approving the exit afterwards changes nothing, and
/// the session stays in the plan mode the host put it in.
#[tokio::test(flavor = "multi_thread")]
async fn an_exit_approved_after_the_host_switched_changes_nothing() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("mode-overtaken-exit", &stub)
        .settings(json!({ "permission_mode": "acceptEdits" }))
        .start()
        .await?;
    let session = run.session().await?;
    let watcher = run.watch_with(&session, |_| None).await?;
    stub.script([
        Reply::calls("p1", "EnterPlanMode", json!({})),
        Reply::calls("p2", "ExitPlanMode", json!({})),
        write("w1", "after.txt", "y"),
        Reply::text("done"),
    ]);
    let turn = run.spawn_turn(&session, "plan, then do", json!({})).await?;
    let asked = watcher
        .wait_for("the exit's prompt", Duration::from_secs(30), |e| {
            e["kind"] == "prompt"
        })
        .await?;
    set_mode(&mut run, &session, "default").await?;
    set_mode(&mut run, &session, "plan").await?;
    answer(&mut run, &session, &asked, Answer::permit()).await;
    let events = turn.await??;

    let exit = &events.tool_results[1];
    assert!(exit.2, "the exit went through: {:?}", events.tool_results);
    assert!(
        run.read_file("after.txt").is_none(),
        "{:?}",
        events.tool_results
    );
    assert_eq!(resumed_mode(&mut run, &session, json!({})).await?, "plan");
    assert_eq!(
        wait_frames(&watcher, 3).await,
        [
            pair("plan", "model"),
            pair("default", "host"),
            pair("plan", "host"),
        ]
    );
    run.finish().await;
    Ok(())
}

/// A session not in memory — pushed out of a full pool — has its log
/// changed, and the turn that brings it back runs in the mode set: into plan
/// and out of it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_not_in_memory_takes_the_mode_up_when_it_comes_back() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("mode-cold", &stub)
        .settings(json!({ "permission_mode": "acceptEdits" }))
        .options(|o| o.session_cap(1))
        .start()
        .await?;
    let session = run
        .session_with(json!({ "options": { "permission_mode": "default" } }))
        .await?;
    stub.script([Reply::text("one")]);
    run.turn(&session, "hello").await?;
    let other = run.session().await?;

    stub.script([Reply::text("other")]);
    run.turn(&other, "something else").await?;
    assert_eq!(listed_status(&mut run, &session).await?, "inactive");
    let planned = set_mode(&mut run, &session, "plan").await?;
    assert_eq!(planned["mode"], "plan", "{planned}");
    stub.script([write("w1", "planned.txt", "x"), Reply::text("done")]);
    let events = run.turn(&session, "write it").await?;
    assert!(
        run.read_file("planned.txt").is_none(),
        "{:?}",
        events.tool_results
    );

    stub.script([Reply::text("other")]);
    run.turn(&other, "something else").await?;
    assert_eq!(listed_status(&mut run, &session).await?, "inactive");
    let edits = set_mode(&mut run, &session, "acceptEdits").await?;
    assert_eq!(edits["mode"], "acceptEdits", "{edits}");
    stub.script([write("w2", "edited.txt", "y"), Reply::text("done")]);
    let events = run.turn(&session, "write it now").await?;
    assert_eq!(
        run.read_file("edited.txt").as_deref(),
        Some("y"),
        "{:?}",
        events.tool_results
    );
    run.finish().await;
    Ok(())
}

/// What a switch was granted is what is kept: a request the settings held
/// narrower stays as narrow once they would allow it.
#[tokio::test(flavor = "multi_thread")]
async fn a_clamped_switch_stays_clamped_when_the_settings_loosen() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("mode-clamp-kept", &stub).start().await?;
    let session = run.session().await?;
    stub.script([Reply::text("one")]);
    run.turn(&session, "hello").await?;
    let clamped = set_mode(&mut run, &session, "bypassPermissions").await?;
    assert_eq!(clamped["mode"], "default", "{clamped}");

    let mut loosened = LiveRun::base_settings();
    loosened["allow_client_permission_override"] = json!(true);
    run.world
        .write_project_settings("mode-clamp-kept", &loosened)?;
    run.restart().await?;
    assert_eq!(
        resumed_mode(&mut run, &session, json!({})).await?,
        "default"
    );
    run.finish().await;
    Ok(())
}
