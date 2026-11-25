//! A host calling MCP tools with no session: `mcp.tools` lists what a pair's
//! connected servers offer, `mcp.callTool` calls one over the connection the
//! daemon already has and hands back what the server answered, `mcp.cancel`
//! stops a named call. Rules, plan mode, connection hooks, the MCP policy and
//! project trust hold as in a session; every call that has a result is told
//! to `daemon.subscribeEvents` without its arguments or result. The toy
//! server's call log says whether a call reached it. Only what the RPC shows.

use std::path::{Path, PathBuf};
use std::time::Duration;

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, McpToy, ProviderStub};
use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};

const INVALID_PARAMS: i32 = -32602;
const MCP_SERVER_UNAVAILABLE: i32 = -32029;

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

/// The toy over stdio as `name`, logging its calls to `calls`, with `report`.
fn toy(name: &str, calls: &Path) -> anyhow::Result<Value> {
    McpToy::stdio_config(&[
        "--name",
        name,
        "--calls",
        &calls.to_string_lossy(),
        "--report",
    ])
}

async fn with_toy(
    name: &str,
    stub: &ProviderStub,
    extra: Value,
) -> anyhow::Result<(LiveRun, tempfile::TempDir, PathBuf)> {
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut settings = json!({ "mcp": { "servers": { "toy": toy("toy", &calls)? } } });
    if let (Some(s), Some(e)) = (settings.as_object_mut(), extra.as_object()) {
        s.extend(e.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    let mut run = stubbed(name, stub).settings(settings).start().await?;
    run.wait_mcp_connected(&["toy"]).await?;
    Ok((run, scratch, calls))
}

async fn call_tool(client: &mut DaemonRpcClient, params: Value) -> anyhow::Result<Value> {
    let resp = client.call("mcp.callTool", params).await?;
    anyhow::ensure!(resp.error.is_none(), "mcp.callTool: {:?}", resp.error);
    Ok(resp.result.unwrap_or_default())
}

async fn refused_with(
    client: &mut DaemonRpcClient,
    method: &str,
    params: Value,
) -> anyhow::Result<daemon::rpc::RpcError> {
    let resp = client.call(method, params.clone()).await?;
    resp.error
        .ok_or_else(|| anyhow::anyhow!("{method} {params} answered {:?}", resp.result))
}

fn calls_of(path: &Path, tool: &str) -> usize {
    McpToy::calls(path)
        .iter()
        .filter(|(_, t)| t == tool)
        .count()
}

/// Waits until the server has logged more than `seen` calls of `tool`.
async fn wait_for_more(path: &Path, tool: &str, seen: usize) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while calls_of(path, tool) <= seen {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the server never logged another {tool}: {:?}",
            McpToy::calls(path)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

fn error_code(result: &Value) -> &str {
    result["error"]["code"].as_str().unwrap_or_default()
}

/// The `mcp_tool_called` events about `tool` so far.
fn audits(events: &blackbox::DaemonEvents, tool: &str) -> Vec<Value> {
    events
        .all()
        .into_iter()
        .filter(|e| e["kind"] == "mcp_tool_called" && e["tool"] == tool)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tools_of_a_connected_server_are_listed_with_their_schemas() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let (mut run, _scratch, _calls) = with_toy("mcp-tools", &stub, json!({})).await?;

    let ping = run.client.call("daemon.ping", json!({})).await?;
    let caps = ping.result.unwrap_or_default()["capabilities"].clone();
    assert!(
        caps.as_array()
            .is_some_and(|c| c.iter().any(|c| c == "mcp_call")),
        "{caps}"
    );

    let listed = run.client.call("mcp.tools", json!({})).await?;
    let listed = listed.result.unwrap_or_default();
    let servers = listed["servers"].as_array().cloned().unwrap_or_default();
    let toy = servers
        .iter()
        .find(|s| s["name"] == "toy")
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(toy["state"], "connected", "{toy}");
    let tools = toy["tools"].as_array().cloned().unwrap_or_default();
    let tool = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} in {toy}"))
    };
    let report = tool("report");
    assert_eq!(report["tool_name"], "mcp__toy__report", "{report}");
    assert_eq!(
        report["annotations"],
        json!({ "read_only": true, "idempotent": false, "open_world": false }),
        "{report}"
    );
    assert_eq!(
        report["output_schema"]["required"],
        json!(["value"]),
        "{report}"
    );
    assert_eq!(report["input_schema"]["type"], "object", "{report}");
    assert!(
        report["input_schema"]["properties"]["value"].is_object(),
        "{report}"
    );
    let ping = tool("ping");
    assert_eq!(ping["annotations"], json!({}), "{ping}");
    assert!(ping.get("output_schema").is_none(), "{ping}");
    assert!(
        ping["description"]
            .as_str()
            .is_some_and(|d| d.contains("Echo")),
        "{ping}"
    );

    let one = run
        .client
        .call("mcp.tools", json!({ "server": "toy" }))
        .await?;
    let one = one.result.unwrap_or_default();
    assert_eq!(one["servers"].as_array().map(Vec::len), Some(1), "{one}");

    let unknown = refused_with(&mut run.client, "mcp.tools", json!({ "server": "nope" })).await?;
    assert_eq!(unknown.code, INVALID_PARAMS, "{}", unknown.message);
    assert!(unknown.message.contains("toy"), "{}", unknown.message);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_hands_back_what_the_server_answered_and_is_audited() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let (mut run, _scratch, calls) = with_toy("mcp-call", &stub, json!({})).await?;
    let events = run.daemon_events().await?;

    let report = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "report", "arguments": { "value": { "a": "SECRET-ARG" } },
                "run_id": "r-1" }),
    )
    .await?;
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(report["is_error"], false, "{report}");
    assert_eq!(
        report["structured"],
        json!({ "value": { "a": "SECRET-ARG" } }),
        "{report}"
    );
    assert_eq!(report["text"], r#"report: {"a":"SECRET-ARG"}"#, "{report}");
    assert_eq!(
        report["content"],
        json!([{ "type": "text", "text": r#"report: {"a":"SECRET-ARG"}"# }]),
        "{report}"
    );
    assert!(report.get("error").is_none(), "{report}");

    let failed = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "report", "arguments": { "value": 1, "fail": true } }),
    )
    .await?;
    assert_eq!(failed["ok"], true, "{failed}");
    assert_eq!(failed["is_error"], true, "{failed}");

    let ping = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "ping", "arguments": { "message": "hi" } }),
    )
    .await?;
    assert_eq!(ping["text"], "toy: hi", "{ping}");
    assert!(ping.get("structured").is_none(), "{ping}");
    assert_eq!(calls_of(&calls, "report"), 2);
    assert_eq!(calls_of(&calls, "ping"), 1);

    for (params, what) in [
        (json!({ "server": "toy", "tool": "nope" }), "no tool"),
        (
            json!({ "server": "ghost", "tool": "ping" }),
            "there is no MCP server",
        ),
        (
            json!({ "server": "toy", "tool": "ping", "arguments": "x" }),
            "",
        ),
        (
            json!({ "server": "toy", "tool": "ping", "timeout_ms": 0 }),
            "more than 0 ms",
        ),
        (
            json!({ "server": "toy", "tool": "ping", "timeout_ms": 600001 }),
            "at most 600000 ms",
        ),
        (
            json!({ "server": "toy", "tool": "ping", "session_id": "s", "scene": "code" }),
            "not both",
        ),
    ] {
        let error = refused_with(&mut run.client, "mcp.callTool", params.clone()).await?;
        assert_eq!(error.code, INVALID_PARAMS, "{params}: {}", error.message);
        assert!(error.message.contains(what), "{params}: {}", error.message);
    }
    assert_eq!(
        calls_of(&calls, "ping"),
        1,
        "a refused request called nothing"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while audits(&events, "ping").is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let reported = audits(&events, "report");
    let outcomes: Vec<&Value> = reported.iter().map(|e| &e["outcome"]).collect();
    assert_eq!(
        outcomes,
        [&json!("ok"), &json!("tool_error")],
        "{reported:?}"
    );
    let first = &reported[0];
    assert_eq!(first["server"], "toy", "{first}");
    assert_eq!(first["tool_name"], "mcp__toy__report", "{first}");
    assert_eq!(first["run_id"], "r-1", "{first}");
    assert_eq!(first["scene"], "code", "{first}");
    assert!(first["duration_ms"].is_u64(), "{first}");
    assert!(first.get("session_id").is_none(), "{first}");
    for event in events.all() {
        assert!(
            !event.to_string().contains("SECRET-ARG"),
            "an event carried the arguments or the result: {event}"
        );
    }
    assert_eq!(audits(&events, "ping").len(), 1);
    assert!(
        audits(&events, "nope").is_empty(),
        "a refused request is not a call"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_in_no_state_to_be_called_says_which() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let policy_path = scratch.path().join("mcp-policy.json");
    std::fs::write(
        &policy_path,
        serde_json::to_vec(&json!({ "denied_servers": [ { "name": "blocked" } ] }))?,
    )?;
    let mut off = toy("off", &calls)?;
    off["enabled"] = json!(false);
    let servers = json!({
        "toy": toy("toy", &calls)?,
        "off": off,
        "blocked": toy("blocked", &calls)?,
        "broken": { "type": "stdio", "command": scratch.path().join("absent-binary").to_string_lossy() },
    });
    let policy = policy_path.clone();
    let mut run = stubbed("mcp-unavailable", &stub)
        .settings(json!({ "mcp": { "servers": servers } }))
        .options(move |o| o.mcp_policy(policy.clone()))
        .start()
        .await?;
    run.wait_mcp_connected(&["toy"]).await?;

    for (server, state) in [
        ("off", "disabled"),
        ("blocked", "denied"),
        ("broken", "failed"),
    ] {
        let error = refused_with(
            &mut run.client,
            "mcp.callTool",
            json!({ "server": server, "tool": "ping", "arguments": { "message": "x" } }),
        )
        .await?;
        assert_eq!(
            error.code, MCP_SERVER_UNAVAILABLE,
            "{server}: {}",
            error.message
        );
        let data = error.data.unwrap_or_default();
        assert_eq!(data["server"], server, "{data}");
        assert_eq!(data["state"], state, "{server}: {data}");
    }
    let listed = run.client.call("mcp.tools", json!({})).await?;
    let listed = listed.result.unwrap_or_default();
    for (server, state) in [
        ("off", "disabled"),
        ("blocked", "denied"),
        ("broken", "failed"),
    ] {
        let entry = listed["servers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["name"] == server)
            .unwrap_or_else(|| panic!("no {server} in {listed}"));
        assert_eq!(entry["state"], state, "{entry}");
        assert_eq!(entry["tools"], json!([]), "{entry}");
    }
    let callers: Vec<String> = McpToy::calls(&calls).into_iter().map(|(n, _)| n).collect();
    assert!(callers.iter().all(|n| n == "toy"), "{callers:?}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_projects_own_server_is_neither_listed_with_tools_nor_called(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = stubbed("mcp-untrusted", &stub)
        .settings(json!({ "mcp": { "servers": { "own": toy("own", &calls)? } } }))
        .untrusted()
        .start()
        .await?;
    let listed = run.client.call("mcp.tools", json!({})).await?;
    let listed = listed.result.unwrap_or_default();
    let own = listed["servers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["name"] == "own")
        .cloned()
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(own["state"], "untrusted", "{own}");
    assert_eq!(own["tools"], json!([]), "{own}");
    assert!(
        own["error"].as_str().is_some_and(|e| e.contains("trust")),
        "{own}"
    );
    let error = refused_with(
        &mut run.client,
        "mcp.callTool",
        json!({ "server": "own", "tool": "ping", "arguments": { "message": "x" } }),
    )
    .await?;
    assert_eq!(error.code, MCP_SERVER_UNAVAILABLE, "{}", error.message);
    assert_eq!(error.data.unwrap_or_default()["state"], "untrusted");
    assert!(McpToy::calls(&calls).is_empty(), "the server ran");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn deny_rules_and_plan_mode_refuse_a_call_before_the_server_hears_of_it() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let servers = json!({ "toy": toy("toy", &calls)?, "other": toy("other", &calls)? });
    let mut run = stubbed("mcp-deny", &stub)
        .settings(json!({
            "mcp": { "servers": servers },
            "permissions": { "deny": ["mcp__toy__ping", "mcp__other"] }
        }))
        .start()
        .await?;
    run.wait_mcp_connected(&["toy", "other"]).await?;
    for (server, tool) in [("toy", "ping"), ("other", "echo")] {
        let result = call_tool(
            &mut run.client,
            json!({ "server": server, "tool": tool,
                    "arguments": { "message": "x", "text": "x" } }),
        )
        .await?;
        assert_eq!(result["ok"], false, "{result}");
        assert_eq!(error_code(&result), "refused", "{result}");
    }
    let echoed = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "echo", "arguments": { "text": "fine" } }),
    )
    .await?;
    assert_eq!(echoed["text"], "fine", "{echoed}");
    let heard: Vec<(String, String)> = McpToy::calls(&calls);
    assert_eq!(
        heard,
        [("toy".to_string(), "echo".to_string())],
        "{heard:?}"
    );
    run.finish_allowing_refusals().await;

    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = stubbed("mcp-plan", &stub)
        .settings(json!({
            "mcp": { "servers": { "toy": toy("toy", &calls)? } },
            "permissions": { "mode": "plan" }
        }))
        .start()
        .await?;
    run.wait_mcp_connected(&["toy"]).await?;
    let writes = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "ping", "arguments": { "message": "x" } }),
    )
    .await?;
    assert_eq!(error_code(&writes), "refused", "{writes}");
    let reads = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "report", "arguments": { "value": 1 } }),
    )
    .await?;
    assert_eq!(
        reads["ok"], true,
        "a read-only tool runs in plan mode: {reads}"
    );
    assert_eq!(calls_of(&calls, "ping"), 0);
    run.finish_allowing_refusals().await;
    Ok(())
}

async fn register_hook(client: &mut DaemonRpcClient, on_error: &str) -> anyhow::Result<()> {
    let set = client
        .daemon_set_hooks(vec![json!({
            "events": ["PreToolUse"], "tools": ["*"], "timeout_ms": 30000, "on_error": on_error
        })])
        .await?;
    anyhow::ensure!(set.error.is_none(), "{set:?}");
    Ok(())
}

/// An `mcp.callTool` on `client`, the connection that registered the hooks,
/// answering each `hook_call` with `block` (a block message, or proceed):
/// the result and the `session.event` params of each `hook_call`.
async fn call_answering(
    client: &mut DaemonRpcClient,
    params: Value,
    block: Option<&str>,
) -> anyhow::Result<(Value, Vec<Value>)> {
    let id = client.send_request("mcp.callTool", params).await?;
    let (mut asked, mut replies) = (Vec::new(), Vec::new());
    loop {
        let frame = client
            .next_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("closed before mcp.callTool answered"))?;
        if frame["id"].as_i64() == Some(id) {
            anyhow::ensure!(frame.get("error").is_none(), "{frame}");
            return Ok((frame["result"].clone(), asked));
        }
        if frame["id"].as_i64().is_some_and(|r| replies.contains(&r)) {
            anyhow::ensure!(frame.get("error").is_none(), "respondHook: {frame}");
            continue;
        }
        if frame["params"]["event"]["kind"] != "hook_call" {
            continue;
        }
        let params = frame["params"].clone();
        let mut respond = json!({
            "session_id": params["session_id"],
            "call_id": params["event"]["call_id"],
        });
        if let Some(message) = block {
            respond["decision"] = "block".into();
            respond["message"] = message.into();
        }
        replies.push(client.send_request("session.respondHook", respond).await?);
        asked.push(params);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_hook_is_asked_with_the_mcp_call_marker_and_can_block() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let (run, _scratch, calls) = with_toy("mcp-hooks", &stub, json!({})).await?;
    let mut host = run.daemon.connect().await?;
    register_hook(&mut host, "block").await?;

    let params = json!({ "server": "toy", "tool": "ping", "arguments": { "message": "a" },
                         "run_id": "gate-1" });
    let (result, asked) = call_answering(&mut host, params, None).await?;
    assert_eq!(result["text"], "toy: a", "{result}");
    assert_eq!(asked.len(), 1, "{asked:?}");
    let event = &asked[0]["event"];
    assert_eq!(event["input"]["tool_name"], "mcp__toy__ping", "{event}");
    assert_eq!(
        event["mcp_call"],
        json!({ "server": "toy", "tool": "ping", "run_id": "gate-1" }),
        "{event}"
    );
    assert!(event.get("plugin_call").is_none(), "{event}");
    assert!(asked[0]["session_id"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));

    let params = json!({ "server": "toy", "tool": "ping", "arguments": { "message": "b" } });
    let (result, asked) = call_answering(&mut host, params, Some("the host says no")).await?;
    assert_eq!(error_code(&result), "refused", "{result}");
    assert!(
        result["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("the host says no")),
        "{result}"
    );
    assert_eq!(
        asked[0]["event"]["mcp_call"],
        json!({ "server": "toy", "tool": "ping" })
    );
    assert_eq!(
        calls_of(&calls, "ping"),
        1,
        "the blocked call reached the server"
    );
    run.finish_allowing_refusals().await;
    Ok(())
}

/// The registrant goes away while it is asked: the ask ends at once by its
/// `on_error`, before the hook's own 30 s and the call's 20 s.
#[tokio::test(flavor = "multi_thread")]
async fn a_hook_registrant_that_goes_away_ends_the_ask_by_its_on_error() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let (run, _scratch, calls) = with_toy("mcp-hook-gone", &stub, json!({})).await?;
    let mut host = run.daemon.connect().await?;
    register_hook(&mut host, "block").await?;
    let mut caller = run.daemon.connect().await?;
    let id = caller
        .send_request(
            "mcp.callTool",
            json!({ "server": "toy", "tool": "ping", "arguments": { "message": "x" },
                    "timeout_ms": 20000 }),
        )
        .await?;
    loop {
        let frame = host
            .next_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("closed"))?;
        if frame["params"]["event"]["kind"] == "hook_call" {
            break;
        }
    }
    drop(host);
    let result = loop {
        let frame = caller
            .next_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("closed"))?;
        if frame["id"].as_i64() == Some(id) {
            break frame["result"].clone();
        }
    };
    assert_eq!(error_code(&result), "refused", "{result}");
    assert!(
        result["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("on_error is block")),
        "{result}"
    );
    assert_eq!(calls_of(&calls, "ping"), 0);
    run.finish_allowing_refusals().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_is_stopped_by_its_time_by_either_cancel_and_by_its_connection_closing(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let (mut run, _scratch, calls) = with_toy("mcp-stop", &stub, json!({})).await?;
    let events = run.daemon_events().await?;
    let slow = |extra: Value| {
        let mut params = json!({ "server": "toy", "tool": "slow", "arguments": { "ms": 600000 } });
        if let (Some(p), Some(e)) = (params.as_object_mut(), extra.as_object()) {
            p.extend(e.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        params
    };

    let timed = call_tool(&mut run.client, slow(json!({ "timeout_ms": 300 }))).await?;
    assert_eq!(error_code(&timed), "timeout", "{timed}");

    for (cancel, run_id) in [("mcp.cancel", "s-mcp"), ("plugin.cancel", "s-plugin")] {
        let before = calls_of(&calls, "slow");
        let id = run
            .client
            .send_request("mcp.callTool", slow(json!({ "run_id": run_id })))
            .await?;
        wait_for_more(&calls, "slow", before).await?;
        let mut other = run.daemon.connect().await?;
        let elsewhere = other.call(cancel, json!({ "run_id": run_id })).await?;
        assert_eq!(
            elsewhere.result.unwrap_or_default()["cancelled"],
            false,
            "another connection cancelled it"
        );
        let cancel_id = run
            .client
            .send_request(cancel, json!({ "run_id": run_id }))
            .await?;
        let (mut stopped, mut cancelled) = (None, None);
        while stopped.is_none() || cancelled.is_none() {
            let frame = run
                .client
                .next_frame()
                .await?
                .ok_or_else(|| anyhow::anyhow!("closed"))?;
            match frame["id"].as_i64() {
                Some(i) if i == id => stopped = Some(frame["result"].clone()),
                Some(i) if i == cancel_id => cancelled = Some(frame["result"].clone()),
                _ => {}
            }
        }
        assert_eq!(cancelled.unwrap_or_default()["cancelled"], true, "{cancel}");
        let stopped = stopped.unwrap_or_default();
        assert_eq!(error_code(&stopped), "cancelled", "{cancel}: {stopped}");
    }

    let mut gone = run.daemon.connect().await?;
    let before = calls_of(&calls, "slow");
    gone.send_request("mcp.callTool", slow(json!({ "run_id": "s-gone" })))
        .await?;
    wait_for_more(&calls, "slow", before).await?;
    drop(gone);
    wait_for_more(&calls, "slow:cancelled", 3).await?;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while audits(&events, "slow").len() < 4 {
        anyhow::ensure!(std::time::Instant::now() < deadline, "{:?}", events.all());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let outcomes: Vec<Value> = audits(&events, "slow")
        .iter()
        .map(|e| e["outcome"].clone())
        .collect();
    assert_eq!(
        outcomes,
        [
            json!("timeout"),
            json!("cancelled"),
            json!("cancelled"),
            json!("cancelled")
        ]
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_servers_question_during_a_call_is_answered_cancel() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let (mut run, _scratch, calls) = with_toy("mcp-ask", &stub, json!({})).await?;
    let asked = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "ask", "arguments": { "mode": "form" } }),
    )
    .await?;
    assert_eq!(asked["ok"], true, "{asked}");
    let text = asked["text"].as_str().unwrap_or_default();
    assert!(text.contains("cancel"), "{asked}");
    assert!(!text.contains("accept"), "{asked}");
    let answers: Vec<String> = McpToy::calls(&calls)
        .into_iter()
        .filter(|(_, line)| line.starts_with("ask {"))
        .map(|(_, line)| line)
        .collect();
    assert!(
        !answers.is_empty() && answers.iter().all(|a| a.contains("cancel")),
        "{answers:?}"
    );
    run.finish().await;
    Ok(())
}

fn install_plugin_server(
    world: &blackbox::World,
    plugin: &str,
    config: &Value,
) -> anyhow::Result<()> {
    let dir = world
        .global_root()
        .join("plugins")
        .join("cache")
        .join(plugin)
        .join("1.0.0");
    std::fs::create_dir_all(dir.join("mcp"))?;
    std::fs::write(dir.join("mcp").join("server.json"), config.to_string())?;
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "[plugin]\nname = \"{plugin}\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\
             description = \"declares an MCP server\"\n\n\
             [[mcp]]\nname = \"tools\"\nkind = \"native\"\nconfig = \"mcp/server.json\"\n"
        ),
    )?;
    Ok(())
}

/// With `session_id` the call uses that session's servers: a session whose
/// `settings.plugins` leave out the package declaring a server does not get
/// it, which is `INVALID_PARAMS`, while the settings' own server answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_in_a_session_uses_the_servers_that_session_gets() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let kit = toy("kit", &calls)?;
    let mut run = stubbed("mcp-session", &stub)
        .settings(json!({ "mcp": { "servers": { "toy": toy("toy", &calls)? } } }))
        .before_start(move |world, _| install_plugin_server(world, "kit", &kit))
        .start()
        .await?;
    run.wait_mcp_connected(&["toy", "kit-mcp-tools"]).await?;
    let narrowed = run
        .session_with(json!({ "options": { "settings": { "plugins": { "allow": ["ghost"] } } } }))
        .await?;
    let whole = run.session().await?;

    let ping = |server: &str, session: &str| {
        json!({ "server": server, "tool": "ping", "arguments": { "message": session },
                "session_id": session })
    };
    let answered = call_tool(&mut run.client, ping("kit-mcp-tools", &whole)).await?;
    assert_eq!(answered["text"], format!("toy: {whole}"), "{answered}");
    let answered = call_tool(&mut run.client, ping("toy", &narrowed)).await?;
    assert_eq!(answered["text"], format!("toy: {narrowed}"), "{answered}");
    let error = refused_with(
        &mut run.client,
        "mcp.callTool",
        ping("kit-mcp-tools", &narrowed),
    )
    .await?;
    assert_eq!(error.code, INVALID_PARAMS, "{}", error.message);
    assert!(error.message.contains("does not get"), "{}", error.message);

    let listed = run
        .client
        .call("mcp.tools", json!({ "session_id": narrowed }))
        .await?;
    let names: Vec<String> = listed.result.unwrap_or_default()["servers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert_eq!(names, ["toy"], "the session's view");

    let gone = refused_with(
        &mut run.client,
        "mcp.callTool",
        ping("toy", "00000000-0000-0000-0000-000000000000"),
    )
    .await?;
    assert_eq!(gone.code, -32000, "{}", gone.message);
    run.finish().await;
    Ok(())
}
