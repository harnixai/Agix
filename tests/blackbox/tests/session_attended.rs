//! `options.attended`, `options.auto_name` and the `output_schema` rule of
//! `session.create`: an unattended scene that is told someone is there pushes
//! its permission asks to the host, an attended scene told nobody is there
//! refuses them and drops the ask-the-user tools, sub-agents ask on the root
//! session, the naming call can be turned off and stays off across a resume,
//! and `StructuredOutput` reaches a session with a schema even in a scene
//! whose tool list lacks it.

use std::time::Duration;

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply};
use rpc_client::ToolCallReply;
use serde_json::{json, Value};

const TITLE_MARK: &str = "标题生成器";
const WAIT: Duration = Duration::from_secs(30);

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name)
        .settings(json!({ "permissions": {"mode": "default"}, "memory": { "enabled": false } }))
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
}

fn host_echo() -> Value {
    json!({
        "name": "HostEcho",
        "description": "Echoes on the host.",
        "input_schema": { "type": "object", "properties": {} }
    })
}

fn turn_calls(stub: &ProviderStub) -> Vec<blackbox::SeenRequest> {
    stub.calls()
        .into_iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .collect()
}

fn title_calls(stub: &ProviderStub) -> usize {
    stub.calls()
        .iter()
        .filter(|c| c.purpose() == Purpose::Title)
        .count()
}

async fn name_of(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    let got = run.client.session_get(session).await?;
    Ok(got.result.unwrap_or_default()["name"].clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_attended_harnix_session_puts_a_host_tool_ask_to_the_host() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    stub.script_when(TITLE_MARK, (0..2).map(|_| Reply::text("标题")));
    let mut run = stubbed("attended-harnix", &stub).start().await?;

    let attended = run
        .scene_session(
            "harnix",
            json!({ "options": { "attended": true, "host_tools": [host_echo()] } }),
        )
        .await?;
    let host = run.watch(&attended, true).await?;
    stub.script([
        Reply::calls("h1", "HostEcho", json!({})),
        Reply::text("attended done"),
    ]);
    let turn = run
        .spawn_turn(&attended, "echo on the host", json!({}))
        .await?;
    let call = host
        .wait_for("the host tool call", WAIT, |e| {
            e["kind"] == "host_tool_call"
        })
        .await?;
    let asked = host.asked();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].prompt["event"]["prompt_type"], "permission");
    assert_eq!(asked[0].prompt["event"]["tool_name"], "HostEcho");
    let answered = run
        .client
        .session_respond_tool_call(
            &attended,
            call["event"]["call_id"].as_str().unwrap_or_default(),
            ToolCallReply::Result {
                content: json!([{ "type": "text", "text": "HOST-SAID-HI" }]),
                is_error: false,
            },
        )
        .await?;
    assert!(answered.error.is_none(), "{answered:?}");
    let events = tokio::time::timeout(WAIT, turn).await???;
    assert!(events.text.contains("attended done"), "{events:?}");
    assert!(!events.tool_results[0].2, "{:?}", events.tool_results);

    let control = run
        .scene_session(
            "harnix",
            json!({ "options": { "host_tools": [host_echo()] } }),
        )
        .await?;
    let silent = run.watch(&control, true).await?;
    stub.script([
        Reply::calls("h2", "HostEcho", json!({})),
        Reply::text("control done"),
    ]);
    let events = run.turn(&control, "echo on the host").await?;
    assert!(events.text.contains("control done"), "{events:?}");
    let result = &events.tool_results[0];
    assert!(result.2, "an unattended ask is refused: {result:?}");
    assert!(silent.asked().is_empty(), "{:?}", silent.frames());
    assert!(
        silent.events("host_tool_call").is_empty(),
        "{:?}",
        silent.frames()
    );
    run.finish_allowing_refusals().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn code_with_attended_false_is_unattended() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("code-unattended", &stub).start().await?;
    let prompt_types = json!(["permission", "question"]);
    let outside = tempfile::tempdir()?;

    let unattended = run
        .session_with(json!({ "options": { "attended": false, "prompt_types": prompt_types } }))
        .await?;
    let host = run.watch(&unattended, true).await?;
    stub.script([
        Reply::calls(
            "w1",
            "Write",
            json!({ "file_path": outside.path().join("quiet.txt").to_string_lossy(), "content": "x" }),
        ),
        Reply::text("finished"),
    ]);
    let events = run.turn(&unattended, "write a file").await?;
    host.wait_for("the turn's end", WAIT, |e| e["kind"] == "turn_complete")
        .await?;
    assert!(events.text.contains("finished"), "{events:?}");

    let first = turn_calls(&stub)[0].clone();
    let system = first.system_text();
    assert!(system.contains("Nobody is there to confirm"), "{system}");
    assert!(!system.contains("Check with the user"), "{system}");
    let tools = first.tool_names();
    for absent in ["AskUserQuestion", "EnterPlanMode", "ExitPlanMode"] {
        assert!(!tools.iter().any(|t| t == absent), "{absent}: {tools:?}");
    }
    assert!(host.events("prompt").is_empty(), "{:?}", host.frames());
    assert!(!outside.path().join("quiet.txt").exists());
    assert!(events.tool_results[0].2, "{:?}", events.tool_results);

    let attended = run
        .session_with(json!({ "options": { "prompt_types": prompt_types } }))
        .await?;
    stub.script([Reply::text("hello")]);
    run.turn(&attended, "hi").await?;
    let second = turn_calls(&stub)[2].clone();
    assert!(second.tool_names().iter().any(|t| t == "AskUserQuestion"));
    assert!(second.system_contains("Check with the user"));
    run.finish_allowing_refusals().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_name_false_leaves_the_session_unnamed_across_a_resume() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    stub.script_when(TITLE_MARK, (0..3).map(|_| Reply::text("命名")));
    let mut run = stubbed("auto-name", &stub).start().await?;

    let control = run.scene_session("harnix", json!({})).await?;
    stub.script([Reply::text("one")]);
    run.turn(&control, "hello").await?;
    assert_eq!(title_calls(&stub), 1, "harnix names its sessions");
    assert!(!name_of(&mut run, &control).await?.is_null());

    let quiet = run
        .scene_session("harnix", json!({ "options": { "auto_name": false } }))
        .await?;
    stub.script([Reply::text("two")]);
    run.turn(&quiet, "hello").await?;
    assert_eq!(title_calls(&stub), 1, "no naming call for auto_name false");
    assert!(name_of(&mut run, &quiet).await?.is_null());

    let cold = run
        .scene_session("harnix", json!({ "options": { "auto_name": false } }))
        .await?;
    let closed = run.client.session_close(&cold).await?;
    assert!(closed.error.is_none(), "{closed:?}");
    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": cold }))
        .await?;
    assert!(resumed.error.is_none(), "{resumed:?}");
    stub.script([Reply::text("three")]);
    run.turn(&cold, "hello").await?;
    assert_eq!(title_calls(&stub), 1, "auto_name false survives a resume");
    assert!(name_of(&mut run, &cold).await?.is_null());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn output_schema_reaches_a_whitelisted_scene() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    stub.script_when(TITLE_MARK, [Reply::text("结构化")]);
    let mut run = stubbed("schema-whitelist", &stub).start().await?;

    let session = run
        .scene_session(
            "harnix",
            json!({ "options": { "output_schema": {
                "type": "object",
                "properties": { "answer": { "type": "string" } },
                "required": ["answer"]
            } } }),
        )
        .await?;
    stub.script([
        Reply::calls("so1", "StructuredOutput", json!({ "answer": "42" })),
        Reply::text("handed in"),
    ]);
    let events = run.turn(&session, "what is the answer").await?;
    let first = turn_calls(&stub)[0].clone();
    assert!(
        first.tool_names().iter().any(|t| t == "StructuredOutput"),
        "{:?}",
        first.tool_names()
    );
    let complete = events.turn_complete_event.clone().unwrap_or_default();
    assert_eq!(complete["structured_output"], json!({ "answer": "42" }));

    let registered = run
        .client
        .call(
            "scene.register",
            json!({
                "scene": {
                    "id": "no-schema",
                    "name": "No schema",
                    "tools": ["Read"],
                    "disallowed_tools": ["StructuredOutput"],
                    "requires_project": true
                },
                "prompt": "You read files.",
            }),
        )
        .await?;
    assert!(registered.error.is_none(), "{registered:?}");
    let activated = run
        .client
        .call("scene.activate", json!({ "scene": "no-schema" }))
        .await?;
    assert!(activated.error.is_none(), "{activated:?}");
    let before = run.client.session_list().await?.result.unwrap_or_default()["sessions"]
        .as_array()
        .map_or(0, Vec::len);
    let refused = run
        .client
        .call(
            "session.create",
            json!({
                "scene": "no-schema",
                "project_root": run.project.to_string_lossy(),
                "options": { "output_schema": { "type": "object" } }
            }),
        )
        .await?;
    let error = refused.error.expect("the schema cannot be delivered");
    assert_eq!(error.code, -32602, "{error:?}");
    assert_eq!(
        error
            .data
            .as_ref()
            .map(|d| d["invalid_options"][0]["field"].clone()),
        Some(json!("output_schema")),
        "{error:?}"
    );
    let after = run.client.session_list().await?.result.unwrap_or_default()["sessions"]
        .as_array()
        .map_or(0, Vec::len);
    assert_eq!(before, after, "no session was created");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_of_an_attended_session_asks_on_the_parent() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    stub.script_when(TITLE_MARK, (0..2).map(|_| Reply::text("子代理")));
    let mut run = stubbed("attended-sub-agent", &stub)
        .settings(json!({
            "permissions": {"mode": "default", "allow": ["Agent"]},
            "memory": {"enabled": false},
        }))
        .start()
        .await?;
    let outside = tempfile::tempdir()?;
    let target = outside
        .path()
        .join("sub.txt")
        .to_string_lossy()
        .into_owned();
    let attended = run
        .scene_session("harnix", json!({ "options": { "attended": true } }))
        .await?;
    let host = run
        .watch_with(&attended, |e| {
            (e["prompt_type"] == "permission").then(|| blackbox::Answer::deny("NOT-NOW"))
        })
        .await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls(
                "a1",
                "Agent",
                json!({ "prompt": "write it", "description": "writer" }),
            ),
            Reply::text("parent done"),
        ],
    );
    stub.script([
        Reply::calls(
            "s1",
            "Write",
            json!({ "file_path": target, "content": "x" }),
        ),
        Reply::text("sub done"),
    ]);
    let events = run.turn(&attended, "PARENT-MARK delegate").await?;
    assert!(events.text.contains("parent done"), "{events:?}");
    let asked = host.asked();
    assert_eq!(asked.len(), 1, "{:?}", host.frames());
    let message = asked[0].prompt["event"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.starts_with("[sub-agent "), "{message}");
    assert!(!outside.path().join("sub.txt").exists());

    let control = run.scene_session("harnix", json!({})).await?;
    let silent = run.watch(&control, true).await?;
    stub.script_when(
        "CONTROL-MARK",
        [
            Reply::calls(
                "a2",
                "Agent",
                json!({ "prompt": "write it", "description": "writer" }),
            ),
            Reply::text("control done"),
        ],
    );
    stub.script([
        Reply::calls(
            "s2",
            "Write",
            json!({ "file_path": target, "content": "x" }),
        ),
        Reply::text("sub done"),
    ]);
    let events = run.turn(&control, "CONTROL-MARK delegate").await?;
    assert!(events.text.contains("control done"), "{events:?}");
    assert!(silent.asked().is_empty(), "{:?}", silent.frames());
    assert!(!outside.path().join("sub.txt").exists());
    run.finish_allowing_refusals().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_capability_is_advertised() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("attended-capability", &stub).start().await?;
    let ping = run.client.call("daemon.ping", json!({})).await?;
    let text = ping.result.unwrap_or_default().to_string();
    assert!(text.contains("session_attended"), "{text}");
    run.finish().await;
    Ok(())
}

/// `attended:false` makes a session unattended as an unattended scene is,
/// S15 included: a sandboxed `Bash` command an attended `code` session asks
/// about is, unattended and under a restricted network, run confined without
/// anyone asked.
#[tokio::test(flavor = "multi_thread")]
async fn attended_false_runs_a_confined_command_unasked_as_an_unattended_scene_does(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("code-attended-false-runs", &stub)
        .settings(json!({
            "permissions": {"mode": "default"},
            "memory": { "enabled": false },
            "sandbox": {"network": {"mode": "deny_all"}}
        }))
        .start()
        .await?;
    let prompt_types = json!(["permission", "question"]);
    let command = |file: &str| format!("sh -c 'printf x > {file}'; echo rc=$?");

    let attended = run
        .session_with(json!({ "options": { "prompt_types": prompt_types } }))
        .await?;
    let host = run.watch(&attended, false).await?;
    stub.script([
        Reply::calls("b1", "Bash", json!({ "command": command("asked.txt") })),
        Reply::text("finished"),
    ]);
    run.turn(&attended, "write a file").await?;
    assert!(!host.events("prompt").is_empty(), "{:?}", host.frames());
    assert!(!run.project.join("asked.txt").exists());
    drop(host);

    let unattended = run
        .session_with(json!({ "options": { "attended": false, "prompt_types": prompt_types } }))
        .await?;
    let host = run.watch(&unattended, true).await?;
    stub.script([
        Reply::calls("b2", "Bash", json!({ "command": command("unasked.txt") })),
        Reply::text("finished"),
    ]);
    let events = run.turn(&unattended, "write a file").await?;
    assert!(host.events("prompt").is_empty(), "{:?}", host.frames());
    assert!(
        run.project.join("unasked.txt").exists(),
        "{:?}",
        events.tool_results
    );
    run.finish_allowing_refusals().await;
    Ok(())
}

/// By default an unattended command the sandbox holds still needs a
/// restricted network: under the default `unrestricted` one, nobody is asked
/// and the command is refused.
#[tokio::test(flavor = "multi_thread")]
async fn an_unattended_command_is_refused_under_an_open_network_by_default() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("code-unattended-open-network", &stub)
        .start()
        .await?;
    let unattended = run
        .session_with(json!({ "options": { "attended": false } }))
        .await?;
    let host = run.watch(&unattended, true).await?;
    stub.script([
        Reply::calls(
            "b1",
            "Bash",
            json!({ "command": "sh -c 'printf x > open.txt'; echo rc=$?" }),
        ),
        Reply::text("finished"),
    ]);
    let events = run.turn(&unattended, "write a file").await?;
    assert!(host.events("prompt").is_empty(), "{:?}", host.frames());
    assert!(events.tool_results[0].2, "{:?}", events.tool_results);
    assert!(!run.project.join("open.txt").exists());
    run.finish_allowing_refusals().await;
    Ok(())
}
