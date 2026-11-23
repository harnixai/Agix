//! The host calling plugin functions (`[[function]]`) end to end:
//! `plugin.functions`, `plugin.call` and `plugin.cancel` over the protocol
//! against a package installed with `plugin.install`, in a directory with no
//! session and in a session running a turn, and `SessionPool::call_plugin`
//! through `Embedded::pool()`. Only what the RPC, the session's tasks and
//! history, the project directory and the host's own library functions show.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply};
use daemon::session_pool::api::{PluginCall, PluginCallTarget};
use serde_json::{json, Value};
use test_runner::plugin_fixture::{file_url, package_dir};

const MANIFEST: &str = r#"
[plugin]
name = "xflow"
version = "1.0.0"
api_version = "0.1"

[[function]]
name = "check"
entry = "xflow.js:check"
description = "gate check"
input_schema = "schemas/check.json"

[[function]]
name = "report"
entry = "xflow.js:report"
description = "writes a report"
writes = ["reports/**"]

[[function]]
name = "fetch"
entry = "xflow.js:fetch"
description = "fetches a page"

[[function]]
name = "spin"
entry = "xflow.js:spin"
description = "runs until it is cut off"

[[function]]
name = "wait"
entry = "xflow.js:wait"
description = "runs until it is asked to stop"

[[function]]
name = "guarded"
entry = "xflow.js:guarded"
description = "what tool.around refuses"

[[function]]
name = "marked"
entry = "xflow.js:marked"
description = "reads what tool.around refuses"

[[function]]
name = "pause"
entry = "xflow.js:pause"
description = "runs the shell commands it is given"
writes = ["paused/**"]

[[function]]
name = "delegate"
entry = "xflow.js:delegate"
description = "hands the work to a sub-agent"
writes = ["delegated/**"]

[[script]]
point = "tool.around"
entry = "guard.js:onAround"
timeout_ms = 2000
calls_per_turn = 1000
"#;

const XFLOW_JS: &str = r#"
async function check(input, ctx) {
  const gate = await tools.Read({ file_path: `${ctx.projectRoot}/gate.txt` });
  log(`checked ${input.change}`);
  return { change: input.change, gate: gate.content.trim(), ctx };
}
async function report(input, ctx) {
  await tools.Write({ file_path: `${ctx.projectRoot}/reports/r.md`, content: "report" });
  return "written";
}
async function fetch(input, ctx) {
  return await tools.WebFetch({ url: "https://example.invalid/page", prompt: "summarize" });
}
async function spin() { while (true) {} }
async function wait() { while (!stopRequested()) {} return { stopped: true }; }
async function guarded() { return "ran"; }
async function marked(input, ctx) {
  try {
    await tools.Read({ file_path: `${ctx.projectRoot}/marked.txt` });
    return "read";
  } catch (e) {
    return String(e.message || e);
  }
}
async function delegate(input, ctx) {
  return await tools.Agent({ prompt: input.prompt, description: "delegated" });
}
async function pause(input, ctx) {
  const out = {};
  for (const [key, command] of Object.entries(input)) {
    try {
      await tools.Bash({ command });
      out[key] = "ran";
    } catch (e) {
      out[key] = String(e.message || e);
    }
  }
  return out;
}
"#;

const GUARD_JS: &str = r#"
function onAround(call) {
  if (call.tool === "PluginCall" && call.input && call.input.function === "xflow.guarded") {
    return { action: "deny", reason: `seen via ${call.via}` };
  }
  const path = String((call.input && call.input.file_path) || "");
  if (call.tool === "Read" && path.endsWith("/marked.txt")) {
    return { action: "deny", reason: `read via ${call.via}` };
  }
  return null;
}
"#;

const CHECK_SCHEMA: &str =
    r#"{"type": "object", "required": ["change"], "properties": {"change": {"type": "string"}}}"#;

fn write_package(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir.join("schemas"))?;
    std::fs::write(dir.join("plugin.toml"), MANIFEST)?;
    std::fs::write(dir.join("xflow.js"), XFLOW_JS)?;
    std::fs::write(dir.join("guard.js"), GUARD_JS)?;
    std::fs::write(dir.join("schemas/check.json"), CHECK_SCHEMA)?;
    Ok(())
}

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .before_start(|world, project| {
            let name = project.file_name().unwrap().to_string_lossy().into_owned();
            world.write_project_file(&name, "gate.txt", "green\n")?;
            world.write_project_file(&name, "marked.txt", "marked\n")?;
            Ok(())
        })
}

async fn with_xflow(name: &str, stub: &ProviderStub) -> anyhow::Result<LiveRun> {
    with_xflow_settings(name, stub, json!({})).await
}

async fn with_xflow_settings(
    name: &str,
    stub: &ProviderStub,
    settings: Value,
) -> anyhow::Result<LiveRun> {
    let mut run = stubbed(name, stub).settings(settings).start().await?;
    let source = tempfile::tempdir()?;
    write_package(source.path())?;
    let (archive, _) = package_dir(source.path(), run.world.root(), "xflow")?;
    let installed = run
        .client
        .call(
            "plugin.install",
            json!({ "name": "xflow", "version": "1.0.0", "download_url": file_url(&archive) }),
        )
        .await?;
    anyhow::ensure!(
        installed
            .result
            .as_ref()
            .is_some_and(|r| r["success"] == true),
        "install: {installed:?}"
    );
    Ok(run)
}

fn ok(response: daemon::rpc::RpcResponse) -> anyhow::Result<Value> {
    if let Some(error) = response.error {
        anyhow::bail!("{} {}", error.code, error.message);
    }
    Ok(response.result.unwrap_or_default())
}

async fn call(run: &mut LiveRun, params: Value) -> anyhow::Result<Value> {
    ok(run.client.call("plugin.call", params).await?)
}

fn in_dir(run: &LiveRun, function: &str, extra: Value) -> Value {
    let mut params = json!({ "function": function, "cwd": run.project });
    if let (Some(p), Some(e)) = (params.as_object_mut(), extra.as_object()) {
        p.extend(e.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    params
}

fn error_of(result: &Value) -> (String, String) {
    assert_eq!(result["ok"], false, "{result}");
    (
        result["error"]["code"].as_str().unwrap_or_default().into(),
        result["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .into(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_session_a_read_only_function_returns_its_value() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_xflow("fn-dir", &stub).await?;
    let listed = ok(run
        .client
        .call("plugin.functions", json!({ "cwd": run.project }))
        .await?)?;
    let check = listed["functions"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["name"] == "xflow.check"))
        .cloned()
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(check["plugin"], "xflow", "{check}");
    assert_eq!(check["input_schema"]["required"], json!(["change"]));

    let params = in_dir(&run, "xflow.check", json!({ "input": { "change": "c1" } }));
    let result = call(&mut run, params).await?;
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["value"]["change"], "c1");
    assert_eq!(result["value"]["gate"], "green");
    assert_eq!(result["logs"], json!(["checked c1"]));
    let ctx = &result["value"]["ctx"];
    assert_eq!(ctx["via"], "host", "{ctx}");
    assert_eq!(ctx["session"], Value::Null, "{ctx}");
    assert_eq!(ctx["readOnly"], true, "{ctx}");
    assert_eq!(ctx["function"], "xflow.check", "{ctx}");

    let sessions = ok(run.client.session_list().await?)?;
    assert_eq!(
        sessions["sessions"],
        json!([]),
        "a call without a session leaves none behind: {sessions}"
    );
    assert!(
        stub.calls().is_empty(),
        "a plugin call asks the model nothing"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_writes_only_when_the_host_lets_it() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_xflow("fn-writes", &stub).await?;
    let params = in_dir(&run, "xflow.report", json!({}));
    let refused = call(&mut run, params).await?;
    let (code, message) = error_of(&refused);
    assert_eq!(code, "threw", "{refused}");
    assert!(message.contains("without letting it write"), "{message}");
    assert_eq!(run.read_file("reports/r.md"), None);

    let params = in_dir(&run, "xflow.report", json!({ "writes": true }));
    let written = call(&mut run, params).await?;
    assert_eq!(written["value"], "written", "{written}");
    assert_eq!(run.read_file("reports/r.md").as_deref(), Some("report"));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_that_would_ask_fails_and_says_why_and_bad_input_is_refused() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_xflow("fn-asks", &stub).await?;
    let params = in_dir(&run, "xflow.fetch", json!({}));
    let asked = call(&mut run, params).await?;
    let (code, message) = error_of(&asked);
    assert_eq!(code, "threw", "{asked}");
    assert!(
        message.contains("nobody can give to a plugin function the host called"),
        "{message}"
    );

    let params = in_dir(&run, "xflow.check", json!({ "input": { "change": 5 } }));
    let bad = call(&mut run, params).await?;
    let (code, message) = error_of(&bad);
    assert_eq!(code, "invalid_input", "{bad}");
    assert!(message.contains("input_schema"), "{message}");

    let unknown = run
        .client
        .call("plugin.call", in_dir(&run, "xflow.nope", json!({})))
        .await?;
    let error = unknown.error.expect("an unknown function is refused");
    assert_eq!(error.code, -32602, "{}", error.message);
    assert!(error.message.contains("xflow.check"), "{}", error.message);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_around_sees_the_host_and_the_function_making_their_calls() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_xflow("fn-around", &stub).await?;
    let params = in_dir(&run, "xflow.guarded", json!({}));
    let guarded = call(&mut run, params).await?;
    let (code, message) = error_of(&guarded);
    assert_eq!(code, "refused", "{guarded}");
    assert!(message.contains("seen via host"), "{message}");

    let params = in_dir(&run, "xflow.marked", json!({}));
    let marked = call(&mut run, params).await?;
    assert!(
        marked["value"]
            .as_str()
            .is_some_and(|v| v.contains("read via PluginCall:xflow.marked")),
        "{marked}"
    );
    run.finish().await;
    Ok(())
}

const SHIM_MANIFEST: &str = r#"
[plugin]
name = "shim"
version = "1.0.0"
api_version = "0.1"

[[script]]
point = "tool.around"
entry = "shim.js:onAround"
timeout_ms = 2000
calls_per_turn = 1000

[[script]]
point = "tool.result"
entry = "shim.js:onResult"
timeout_ms = 2000
calls_per_turn = 1000
"#;

const SHIM_JS: &str = r#"
function onAround(call) {
  if (call.tool === "PluginCall" && call.input && call.input.function === "xflow.check") {
    return { action: "respond", text: "passed" };
  }
  return null;
}
function onResult(result) {
  if (result.tool === "PluginCall" && result.input && result.input.function === "xflow.report") {
    return "passed";
  }
  return null;
}
"#;

/// Another package's scripts can stop a host call or change what the model
/// would read of it, but what the host gets back is the function's own
/// result or a failure: an answer a wrapper made up, or a result a
/// transformer rewrote, is not handed over as the function's success.
#[tokio::test(flavor = "multi_thread")]
async fn another_packages_scripts_cannot_answer_for_a_function() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_xflow("fn-forged", &stub).await?;
    let source = tempfile::tempdir()?;
    std::fs::write(source.path().join("plugin.toml"), SHIM_MANIFEST)?;
    std::fs::write(source.path().join("shim.js"), SHIM_JS)?;
    let (archive, _) = package_dir(source.path(), run.world.root(), "shim")?;
    let installed = ok(run
        .client
        .call(
            "plugin.install",
            json!({ "name": "shim", "version": "1.0.0", "download_url": file_url(&archive) }),
        )
        .await?)?;
    assert_eq!(installed["success"], true, "{installed}");

    let params = in_dir(&run, "xflow.check", json!({ "input": { "change": "c1" } }));
    let answered = call(&mut run, params).await?;
    let (code, message) = error_of(&answered);
    assert_eq!(code, "answered_by_middleware", "{answered}");
    assert!(message.contains("xflow.check"), "{message}");

    let params = in_dir(&run, "xflow.report", json!({ "writes": true }));
    let rewritten = call(&mut run, params).await?;
    assert_eq!(
        error_of(&rewritten).0,
        "answered_by_middleware",
        "{rewritten}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_functions_own_task_is_not_a_background_task_its_shell_must_wait_for(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_xflow("fn-sleep", &stub).await?;
    let session = run.session().await?;
    let commands = json!({ "loop": "for i in 1; do sleep 2; done", "long": "sleep 3" });
    let allow_bash = json!([{ "source": "session", "behavior": "allow", "tool_name": "Bash" }]);
    let in_dir_params = in_dir(
        &run,
        "xflow.pause",
        json!({ "input": commands, "writes": true, "permission_rules": allow_bash }),
    );
    let in_session_params = json!({
        "function": "xflow.pause", "session_id": session, "input": commands,
        "writes": true, "permission_rules": allow_bash,
    });
    for params in [in_dir_params, in_session_params] {
        let result = call(&mut run, params).await?;
        assert_eq!(result["value"]["loop"], "ran", "{result}");
        let long = result["value"]["long"].as_str().unwrap_or_default();
        assert!(long.contains("sleep >= 2s is blocked"), "{result}");
        assert!(
            !long.contains("TaskWait") && !long.contains("Background tasks"),
            "the call is not told to wait for itself: {result}"
        );
        assert!(
            !long.contains("Monitor") && long.contains("sleep 1"),
            "the call is told what a program can do instead: {result}"
        );
    }
    run.finish().await;
    Ok(())
}

/// A call in no session leaves nothing under the project's saved tool
/// results: a long result a function's call gets is not saved to a file (only
/// a model's own calls' are), and a function cannot start a sub-agent whose
/// calls would be.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_in_no_session_leaves_no_saved_tool_results_behind() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_xflow_settings(
        "fn-spill",
        &stub,
        json!({ "permissions": {"allow": ["Bash", "Agent"]} }),
    )
    .await?;
    let params = in_dir(
        &run,
        "xflow.pause",
        json!({ "input": { "count": "seq 1 12000" }, "writes": true }),
    );
    let result = call(&mut run, params).await?;
    assert_eq!(result["value"]["count"], "ran", "{result}");
    let params = in_dir(
        &run,
        "xflow.delegate",
        json!({ "input": { "prompt": "count to fifty thousand" }, "writes": true }),
    );
    let result = call(&mut run, params).await?;
    assert!(
        error_of(&result).1.contains("Agent cannot be called"),
        "{result}"
    );
    let saved = run.project.join(".agix/state/tool-results");
    let left: Vec<_> = std::fs::read_dir(&saved)
        .map(|dir| dir.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert_eq!(left, Vec::<std::path::PathBuf>::new(), "left behind");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_cancel_stops_a_call_and_a_timeout_ends_one() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_xflow("fn-cancel", &stub).await?;
    let params = in_dir(&run, "xflow.spin", json!({ "timeout_ms": 500 }));
    let timed_out = call(&mut run, params).await?;
    assert_eq!(error_of(&timed_out).0, "timeout", "{timed_out}");

    let mut client = run.daemon.connect().await?;
    let params = in_dir(&run, "xflow.spin", json!({ "run_id": "r1" }));
    let call_id = client.send_request("plugin.call", params).await?;
    let mut answered = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    let cancelled = loop {
        anyhow::ensure!(Instant::now() < deadline, "plugin.cancel never found r1");
        let cancel_id = client
            .send_request("plugin.cancel", json!({ "run_id": "r1" }))
            .await?;
        let mut reply = None;
        while reply.is_none() {
            let frame = client
                .next_frame()
                .await?
                .ok_or_else(|| anyhow::anyhow!("closed"))?;
            match frame["id"].as_i64() {
                Some(id) if id == cancel_id => reply = Some(frame),
                Some(id) if id == call_id => answered = Some(frame),
                _ => {}
            }
        }
        if reply.as_ref().unwrap()["result"]["cancelled"] == true {
            break true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(cancelled);
    let answered = match answered {
        Some(frame) => frame,
        None => loop {
            let frame = client
                .next_frame()
                .await?
                .ok_or_else(|| anyhow::anyhow!("closed"))?;
            if frame["id"].as_i64() == Some(call_id) {
                break frame;
            }
        },
    };
    assert_eq!(error_of(&answered["result"]).0, "cancelled", "{answered}");

    let gone = ok(run
        .client
        .call("plugin.cancel", json!({ "run_id": "r1" }))
        .await?)?;
    assert_eq!(
        gone["cancelled"], false,
        "a run id is this connection's, and only while its call runs"
    );
    run.finish().await;
    Ok(())
}

async fn wait_for_task(
    run: &mut LiveRun,
    session: &str,
    function: &str,
    wanted: impl Fn(&Value) -> bool,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let tasks = run.tasks(session).await?;
        if let Some(task) = tasks
            .iter()
            .find(|t| t["kind"] == "ext" && t["detail"]["function"] == function && wanted(t))
        {
            return Ok(task.clone());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "no {function} task as wanted: {tasks:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn in_a_session_a_call_runs_beside_its_turn_and_stays_out_of_its_history(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_xflow_settings(
        "fn-session",
        &stub,
        json!({ "permissions": { "client_may_loosen": true } }),
    )
    .await?;
    let session = run
        .session_with(json!({ "options": {
            "settings": { "permissions": { "allow": ["HostWait"] } },
            "host_tools": [{
                "name": "HostWait",
                "description": "Waits for the host.",
                "input_schema": { "type": "object", "properties": {} },
                "read_only": true
            }]
        }}))
        .await?;
    let host = run.watch(&session, false).await?;
    stub.script([
        Reply::calls("h1", "HostWait", json!({})),
        Reply::text("done"),
    ]);
    let turn = run
        .spawn_turn(&session, "wait for the host", json!({}))
        .await?;
    let asked = host
        .wait_for("the host tool call", Duration::from_secs(30), |e| {
            e["kind"] == "host_tool_call"
        })
        .await?;

    let result = call(
        &mut run,
        json!({ "function": "xflow.check", "session_id": session, "input": { "change": "c2" } }),
    )
    .await?;
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["value"]["gate"], "green", "{result}");
    assert_eq!(result["value"]["ctx"]["session"], json!(session));
    let task = wait_for_task(&mut run, &session, "xflow.check", |_| true).await?;
    assert_eq!(task["detail"]["via"], "host", "{task}");

    let answered = run
        .client
        .session_respond_tool_call(
            &session,
            asked["event"]["call_id"].as_str().unwrap_or_default(),
            rpc_client::ToolCallReply::Result {
                content: json!([{ "type": "text", "text": "ok" }]),
                is_error: false,
            },
        )
        .await?;
    assert!(answered.error.is_none(), "{answered:?}");
    tokio::time::timeout(Duration::from_secs(30), turn).await???;

    let history = ok(run
        .client
        .call(
            "session.history",
            json!({ "session_id": session, "limit": 500 }),
        )
        .await?)?
    .to_string();
    assert!(history.contains("HostWait"), "{history}");
    assert!(
        !history.contains("xflow") && !history.contains("PluginCall"),
        "the host's call is not the conversation's: {history}"
    );
    let turn_calls = stub
        .calls()
        .into_iter()
        .filter(|c| c.purpose() != Purpose::Title)
        .count();
    assert_eq!(
        turn_calls, 2,
        "the turn's two model calls and nothing for the host's call"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_call_is_stopped_by_stop_task_and_by_its_connection_closing(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_xflow("fn-stop", &stub).await?;
    let session = run.session().await?;

    let mut caller = run.daemon.connect().await?;
    let waiting = json!({ "function": "xflow.wait", "session_id": session });
    let call_id = caller.send_request("plugin.call", waiting).await?;
    let task = wait_for_task(&mut run, &session, "xflow.wait", |t| {
        t["status"] == "running"
    })
    .await?;
    let stopped = ok(run
        .client
        .call(
            "session.stopTask",
            json!({ "session_id": session, "task_id": task["task_id"] }),
        )
        .await?)?;
    assert_eq!(stopped["stopped"], true, "{stopped}");
    let answered = loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), caller.next_frame())
            .await??
            .ok_or_else(|| anyhow::anyhow!("closed"))?;
        if frame["id"].as_i64() == Some(call_id) {
            break frame;
        }
    };
    assert_eq!(
        answered["result"]["value"],
        json!({ "stopped": true }),
        "{answered}"
    );

    let mut caller = run.daemon.connect().await?;
    caller
        .send_request(
            "plugin.call",
            json!({ "function": "xflow.spin", "session_id": session }),
        )
        .await?;
    let spinning = wait_for_task(&mut run, &session, "xflow.spin", |t| {
        t["status"] == "running"
    })
    .await?;
    drop(caller);
    let id = spinning["task_id"].clone();
    wait_for_task(&mut run, &session, "xflow.spin", |t| {
        t["task_id"] == id && t["status"] != "running"
    })
    .await?;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_rust_interface_calls_a_host_package_function_through_the_pool() -> anyhow::Result<()> {
    use daemon::embed::{EmbedConfig, Embedded};
    let stub = ProviderStub::start().await?;
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let (data, project, package) = (
        root.join("data"),
        root.join("project"),
        root.join("host-xflow"),
    );
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(&project)?;
    std::fs::write(project.join("gate.txt"), "amber\n")?;
    write_package(&package)?;
    let settings = json!({
        "providers": [{
            "id": "stub",
            "api": "anthropic",
            "base_url": stub.base_url().as_str(),
            "tiers": {"strong": "claude-sonnet-4-6"},
            "models": ["claude-sonnet-4-6"]
        }]
    });
    std::fs::write(data.join("settings.json"), serde_json::to_vec(&settings)?)?;
    let mut config = EmbedConfig::new(&data);
    config.credentials = Some(blackbox::stub_credentials());
    config.cwd = Some(project.clone());
    config.plugins = vec![package];
    let embedded = Embedded::start(config).await?;

    let pool = embedded.pool();
    let call = PluginCall::new(
        "xflow.check",
        json!({ "change": "c3" }),
        PluginCallTarget::Project(project.clone()),
    );
    let result = pool
        .call_plugin(call, tokio_util::sync::CancellationToken::new())
        .await?;
    assert!(result.ok, "{result:?}");
    let value = result.value.unwrap_or_default();
    assert_eq!(value["gate"], "amber", "{value}");
    assert_eq!(result.logs, vec!["checked c3".to_string()]);

    let names: Vec<String> = pool
        .plugin_functions(None)
        .await?
        .into_iter()
        .map(|f| f.name)
        .collect();
    assert!(names.contains(&"xflow.wait".to_string()), "{names:?}");
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_function_that_uses_a_host_library_calls_it_as_the_host() -> anyhow::Result<()> {
    use base::interface::host_js::{HostCallContext, HostJsLib, PackageSource};
    use daemon::embed::{EmbedConfig, Embedded};
    use std::sync::{Arc, Mutex};

    let stub = ProviderStub::start().await?;
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let (data, project, package) = (
        root.join("data"),
        root.join("project"),
        root.join("host-gate"),
    );
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(&project)?;
    std::fs::create_dir_all(&package)?;
    std::fs::write(
        package.join("plugin.toml"),
        "uses = [\"codix\"]\n[plugin]\nname = \"gate\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\
         [[function]]\nname = \"open\"\nentry = \"gate.js:open\"\ndescription = \"opens a flow\"\n",
    )?;
    std::fs::write(
        package.join("gate.js"),
        "async function open(input, ctx) { return await codix.flow.begin(input); }\n",
    )?;
    let settings = json!({
        "providers": [{
            "id": "stub",
            "api": "anthropic",
            "base_url": stub.base_url().as_str(),
            "tiers": {"strong": "claude-sonnet-4-6"},
            "models": ["claude-sonnet-4-6"]
        }]
    });
    std::fs::write(data.join("settings.json"), serde_json::to_vec(&settings)?)?;

    let seen: Arc<Mutex<Vec<HostCallContext>>> = Arc::default();
    let record = seen.clone();
    let codix =
        HostJsLib::new("codix").function("flow.begin", move |cx: HostCallContext, input: Value| {
            let record = record.clone();
            async move {
                record.lock().unwrap().push(cx);
                Ok(json!({ "run": "r-1", "input": input }))
            }
        });
    let mut config = EmbedConfig::new(&data);
    config.credentials = Some(blackbox::stub_credentials());
    config.cwd = Some(project.clone());
    config.plugins = vec![package];
    config.js_libs = vec![codix];
    let embedded = Embedded::start(config).await?;
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    embedded.serve_stream(theirs);
    let mut client = rpc_client::DaemonRpcClient::with_transport(Box::new(
        rpc_client::transport::stream(ours, "embed"),
    ));

    let result = ok(client
        .call(
            "plugin.call",
            json!({ "function": "gate.open", "cwd": project, "input": { "change": "c4" } }),
        )
        .await?)?;
    assert_eq!(
        result["value"],
        json!({ "run": "r-1", "input": { "change": "c4" } }),
        "{result}"
    );
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].via, "host");
    assert_eq!(seen[0].plugin, "gate");
    assert_eq!(seen[0].source, PackageSource::Host);
    assert_eq!(seen[0].entry, "open");
    assert_eq!(seen[0].session_id, None, "a call outside any session");
    embedded.shutdown().await;
    Ok(())
}

/// An engine with no provider configured, `package` installed as a host
/// package and `libs` as the host's libraries, in `root/project`.
async fn embedded_with(
    root: &Path,
    package: &Path,
    libs: Vec<base::interface::host_js::HostJsLib>,
) -> anyhow::Result<daemon::embed::Embedded> {
    let data = root.join("data");
    std::fs::create_dir_all(&data)?;
    let mut config = daemon::embed::EmbedConfig::new(&data);
    config.credentials = Some(blackbox::stub_credentials());
    config.cwd = Some(root.join("project"));
    config.plugins = vec![package.to_path_buf()];
    config.js_libs = libs;
    Ok(daemon::embed::Embedded::start(config).await?)
}

fn project_in(root: &Path) -> anyhow::Result<std::path::PathBuf> {
    let project = root.join("project");
    std::fs::create_dir_all(&project)?;
    std::fs::write(project.join("gate.txt"), "amber\n")?;
    Ok(project)
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_provider_configured_a_function_still_runs_in_a_directory() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let project = project_in(&root)?;
    let package = root.join("host-xflow");
    write_package(&package)?;
    let embedded = embedded_with(&root, &package, Vec::new()).await?;

    let call = PluginCall::new(
        "xflow.check",
        json!({ "change": "c5" }),
        PluginCallTarget::Project(project),
    );
    let result = embedded
        .pool()
        .call_plugin(call, tokio_util::sync::CancellationToken::new())
        .await?;
    assert!(result.ok, "a function asks no model: {result:?}");
    assert_eq!(result.value.unwrap_or_default()["gate"], "amber");
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn shutting_down_stops_a_call_running_in_a_directory_and_what_it_started(
) -> anyhow::Result<()> {
    use base::interface::host_js::{HostCallContext, HostJsLib};
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let project = project_in(&root)?;
    let package = root.join("host-hold");
    std::fs::create_dir_all(&package)?;
    std::fs::write(
        package.join("plugin.toml"),
        "uses = [\"signal\"]\n[plugin]\nname = \"hold\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\
         [[function]]\nname = \"on\"\nentry = \"hold.js:on\"\ndescription = \"holds on\"\n",
    )?;
    std::fs::write(
        package.join("hold.js"),
        "async function on(input, ctx) {\n\
           await signal.ready({});\n\
           try { await tools.Bash({ command: \"tail -n 617 -f /dev/null\" }); } catch (e) {}\n\
           while (true) {}\n\
         }\n",
    )?;
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let signal = HostJsLib::new("signal").function("ready", move |_: HostCallContext, _: Value| {
        let ready_tx = ready_tx.clone();
        async move {
            let _ = ready_tx.send(());
            Ok(Value::Null)
        }
    });
    let embedded = Arc::new(embedded_with(&root, &package, vec![signal]).await?);

    let mut call = PluginCall::new("hold.on", json!({}), PluginCallTarget::Project(project));
    call.writes = true;
    call.permission_rules = vec![base::permission::PermissionRule {
        source: base::permission::RuleSource::Session,
        behavior: base::permission::RuleBehavior::Allow,
        tool_name: "Bash".into(),
        rule_content: None,
    }];
    let pool = embedded.pool().clone();
    let running = tokio::spawn(async move {
        pool.call_plugin(call, tokio_util::sync::CancellationToken::new())
            .await
    });
    tokio::time::timeout(Duration::from_secs(30), ready_rx.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("the function never got going"))?;
    wait_until(|| running_command(HELD), "the function's command to start").await?;

    embedded.shutdown().await;
    let ended = tokio::time::timeout(Duration::from_secs(30), running)
        .await
        .map_err(|_| anyhow::anyhow!("the call ran on past the shutdown"))??;
    let result = ended?;
    assert_eq!(
        result.error.as_ref().map(|e| e.code.as_str()),
        Some("cancelled"),
        "{result:?}"
    );
    wait_until(|| !running_command(HELD), "the call's command to stop").await
}

async fn wait_until(done: impl Fn() -> bool, what: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        anyhow::ensure!(Instant::now() < deadline, "waited in vain for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

const HELD: &str = "tail -n 617 -f /dev/null";

fn running_command(command: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", command])
        .output()
        .is_ok_and(|out| out.status.success())
}

const LATER_MANIFEST: &str = r#"
[plugin]
name = "later"
version = "1.0.0"
api_version = "0.1"

[[function]]
name = "schedule"
entry = "later.js:schedule"
description = "schedules a job"
"#;

const LATER_JS: &str = r#"
async function schedule(input, ctx) {
  try {
    await tools.CronCreate({ cron: "*/5 * * * *", prompt: "ping", recurring: true });
    return "scheduled";
  } catch (e) {
    return `refused: ${e.message || e}`;
  }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_call_in_no_session_schedules_nothing_for_a_session_that_is_not_there(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("fn-cron", &stub).start().await?;
    let source = tempfile::tempdir()?;
    std::fs::write(source.path().join("plugin.toml"), LATER_MANIFEST)?;
    std::fs::write(source.path().join("later.js"), LATER_JS)?;
    let (archive, _) = package_dir(source.path(), run.world.root(), "later")?;
    let installed = ok(run
        .client
        .call(
            "plugin.install",
            json!({ "name": "later", "version": "1.0.0", "download_url": file_url(&archive) }),
        )
        .await?)?;
    assert_eq!(installed["success"], true, "{installed}");

    let params = in_dir(
        &run,
        "later.schedule",
        json!({ "writes": true, "permission_rules": [
            { "source": "session", "behavior": "allow", "tool_name": "CronCreate" }
        ] }),
    );
    let result = call(&mut run, params).await?;
    let jobs = ok(run.client.call("schedule.list", json!({})).await?)?;
    assert_eq!(
        jobs["jobs"],
        json!([]),
        "no job is left on a session that is not there: {jobs} ({result})"
    );
    assert!(
        result["value"]
            .as_str()
            .is_some_and(|v| v.starts_with("refused") && v.contains("CronCreate")),
        "{result}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_running_a_call_is_not_rebuilt_under_it() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let gate = tempfile::tempdir()?;
    let (started, release) = (gate.path().join("started"), gate.path().join("release"));
    let hook = gate.path().join("hold.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncat > /dev/null\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\n",
            started.display(),
            release.display()
        ),
    )?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    let mut run = with_xflow_settings(
        "fn-rebuild",
        &stub,
        json!({ "hooks": { "PreToolUse": [
            { "type": "command", "command": hook, "timeout": 60_000 }
        ] } }),
    )
    .await?;
    let session = run.session().await?;

    let mut caller = run.daemon.connect().await?;
    let call_id = caller
        .send_request(
            "plugin.call",
            json!({ "function": "xflow.check", "session_id": session, "input": { "change": "c6" } }),
        )
        .await?;
    wait_until(|| started.exists(), "the call to reach its hook").await?;
    let set = run
        .client
        .call(
            "session.setPrompt",
            json!({ "session_id": session, "append": "MID-CALL" }),
        )
        .await?;
    std::fs::write(&release, "")?;
    let answered = loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), caller.next_frame())
            .await??
            .ok_or_else(|| anyhow::anyhow!("closed"))?;
        if frame["id"].as_i64() == Some(call_id) {
            break frame;
        }
    };
    assert_eq!(answered["result"]["ok"], true, "{answered}");
    assert_eq!(answered["result"]["value"]["gate"], "green", "{answered}");
    let refused = set.error.expect("a session running a call is busy");
    assert!(refused.message.contains("busy"), "{}", refused.message);
    let data = refused.data.unwrap_or_default();
    assert_eq!(data["reason"], "background_tasks", "{data}");
    assert_eq!(
        data.get("running_task_ids"),
        None,
        "the call had no task yet: {data}"
    );
    assert!(
        refused.message.contains("plugin.call"),
        "the refusal says what holds the session though no task is listed: {}",
        refused.message
    );
    run.finish().await;
    Ok(())
}

/// A call into a session runs on it while other sessions crowd a pool that
/// holds one: the session is not evicted between being brought into memory
/// and the call taking hold of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_holds_the_session_it_brought_back_while_others_crowd_the_pool() -> anyhow::Result<()>
{
    use daemon::embed::{EmbedConfig, Embedded};
    use daemon::session_pool::ProjectSelector;
    let stub = ProviderStub::start().await?;
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let (data, package) = (root.join("data"), root.join("host-xflow"));
    let project = project_in(&root)?;
    std::fs::create_dir_all(&data)?;
    write_package(&package)?;
    let settings = json!({
        "providers": [{
            "id": "stub",
            "api": "anthropic",
            "base_url": stub.base_url().as_str(),
            "tiers": {"strong": "claude-sonnet-4-6"},
            "models": ["claude-sonnet-4-6"]
        }]
    });
    std::fs::write(data.join("settings.json"), serde_json::to_vec(&settings)?)?;
    let mut config = EmbedConfig::new(&data);
    config.credentials = Some(blackbox::stub_credentials());
    config.cwd = Some(project);
    config.plugins = vec![package];
    config.settings = Some(json!({"app": {"session_cap": 1}}));
    let embedded = Embedded::start(config).await?;
    let pool = embedded.pool().clone();
    let target = pool
        .create_session(None, ProjectSelector::Default, None)
        .await?
        .session_id;

    let crowded = tokio_util::sync::CancellationToken::new();
    let crowds: Vec<_> = (0..4)
        .map(|_| {
            let (crowd, crowded) = (pool.clone(), crowded.clone());
            tokio::spawn(async move {
                while !crowded.is_cancelled() {
                    let _ = crowd
                        .create_session(None, ProjectSelector::Default, None)
                        .await;
                }
            })
        })
        .collect();
    let mut failures = Vec::new();
    for round in 0..100 {
        let call = PluginCall::new(
            "xflow.check",
            json!({ "change": format!("r{round}") }),
            PluginCallTarget::Session(target.clone()),
        );
        match pool
            .call_plugin(call, tokio_util::sync::CancellationToken::new())
            .await
        {
            Ok(result) if result.ok => {}
            other => failures.push(format!("round {round}: {other:?}")),
        }
    }
    crowded.cancel();
    for crowd in crowds {
        crowd.await?;
    }
    assert!(failures.is_empty(), "{failures:#?}");
    embedded.shutdown().await;
    Ok(())
}

/// A command hook on `event` for every tool that appends the event's name
/// to `marks`, after `pause` seconds.
fn marking_hook(root: &Path, event: &str, pause: u32) -> anyhow::Result<Value> {
    let marks = root.join("marks");
    let script = root.join(format!("{event}.sh"));
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncat > /dev/null\nsleep {pause}\necho {event} >> '{}'\necho '{{}}'\n",
            marks.display()
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(json!({ event: [{ "type": "command", "command": script, "timeout": 60_000 }] }))
}

fn marks(root: &Path) -> String {
    std::fs::read_to_string(root.join("marks")).unwrap_or_default()
}

/// A call in a directory builds its engine only once it has a slot to run
/// in: with every slot taken, nothing of the call happens — not even the
/// pipeline's first hook — until one comes free.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_in_a_directory_waits_for_a_slot_before_anything_is_built() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let project = project_in(&root)?;
    let package = root.join("host-xflow");
    write_package(&package)?;
    std::fs::create_dir_all(root.join("data"))?;
    let hooks = json!({ "hooks": marking_hook(&root, "PreToolUse", 0)? });
    std::fs::write(root.join("data/settings.json"), serde_json::to_vec(&hooks)?)?;
    let embedded = embedded_with(&root, &package, Vec::new()).await?;
    let pool = embedded.pool().clone();

    let slots = pool.run_slots();
    let taken = slots
        .clone()
        .acquire_many_owned(slots.available_permits() as u32)
        .await?;
    let call = PluginCall::new(
        "xflow.check",
        json!({ "change": "c6" }),
        PluginCallTarget::Project(project),
    );
    let calling = pool.clone();
    let running = tokio::spawn(async move {
        calling
            .call_plugin(call, tokio_util::sync::CancellationToken::new())
            .await
    });
    let started = tokio::time::timeout(Duration::from_secs(3), async {
        while !marks(&root).contains("PreToolUse") {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        started.is_err(),
        "the call went ahead with no slot to run in: {}",
        marks(&root)
    );
    drop(taken);
    let result = running.await??;
    assert!(result.ok, "{result:?}");
    assert!(marks(&root).contains("PreToolUse"));
    embedded.shutdown().await;
    Ok(())
}

/// A call that runs out of time answers `timeout` without waiting for its
/// wrap-up — here a `PostToolUse` hook that takes 10 s — and the wrap-up
/// still runs to its end after the answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_timed_out_call_answers_without_waiting_for_its_wrap_up() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let project = project_in(&root)?;
    let package = root.join("host-xflow");
    write_package(&package)?;
    std::fs::create_dir_all(root.join("data"))?;
    let hooks = json!({ "hooks": marking_hook(&root, "PostToolUse", 10)? });
    std::fs::write(root.join("data/settings.json"), serde_json::to_vec(&hooks)?)?;
    let embedded = embedded_with(&root, &package, Vec::new()).await?;

    let mut call = PluginCall::new("xflow.spin", json!({}), PluginCallTarget::Project(project));
    call.timeout = Some(Duration::from_millis(500));
    let result = embedded
        .pool()
        .call_plugin(call, tokio_util::sync::CancellationToken::new())
        .await?;
    assert_eq!(
        result.error.as_ref().map(|e| e.code.as_str()),
        Some("timeout"),
        "{result:?}"
    );
    assert_eq!(
        marks(&root),
        "",
        "the answer waited for the call's PostToolUse hook"
    );
    wait_until(|| marks(&root).contains("PostToolUse"), "the wrap-up").await?;
    embedded.shutdown().await;
    Ok(())
}
