//! The host calling plugin functions (`[[function]]`) end to end:
//! `plugin.functions`, `plugin.call` and `plugin.cancel` over the protocol
//! against a package installed with `plugin.install`, in a directory with no
//! session and in a session running a turn, and `SessionPool::call_plugin`
//! through `Embedded::pool()`. Only what the RPC, the session's tasks and
//! history, and the project directory show.

use std::path::Path;
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
    let mut run = stubbed(name, stub).start().await?;
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
    let mut run = with_xflow("fn-session", &stub).await?;
    let session = run
        .session_with(json!({ "options": {
            "permission_rules": [
                { "source": "session", "behavior": "allow", "tool_name": "HostWait" }
            ],
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
        "providers": { "stub": {
            "api_type": "anthropic",
            "base_url": stub.base_url().as_str(),
            "api_key": "stub-key",
            "default_model": "claude-sonnet-4-6",
            "models": ["claude-sonnet-4-6"]
        }},
        "default_provider": "stub"
    });
    std::fs::write(data.join("settings.json"), serde_json::to_vec(&settings)?)?;
    let mut config = EmbedConfig::new(&data);
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
