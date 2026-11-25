//! `mcp.callTool` / `mcp.tools` at the edges a host or a hostile project
//! reaches: a scene the daemon makes no session in, a server that dies
//! mid-call, a task past its time, the server's own tool timeout, an
//! untrusted project's hooks and servers, a managed ask nobody can answer,
//! scripts that answer for the server or rewrite what it said, and whether a
//! refused call had reached the server. Only what the RPC and the toy
//! server's call log show.

use std::path::Path;
use std::time::Duration;

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, McpToy, ProviderStub};
use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};
use test_runner::plugin_fixture::{file_url, package_dir};

const SCENE_NOT_FOUND: i32 = -32004;

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

fn toy(name: &str, calls: &Path, extra: &[&str]) -> anyhow::Result<Value> {
    let calls = calls.to_string_lossy();
    let mut args = vec!["--name", name, "--calls", &calls];
    args.extend_from_slice(extra);
    McpToy::stdio_config(&args)
}

async fn call_tool(client: &mut DaemonRpcClient, params: Value) -> anyhow::Result<Value> {
    let resp = client.call("mcp.callTool", params).await?;
    anyhow::ensure!(resp.error.is_none(), "mcp.callTool: {:?}", resp.error);
    Ok(resp.result.unwrap_or_default())
}

fn lines_of(path: &Path, prefix: &str) -> usize {
    McpToy::calls(path)
        .iter()
        .filter(|(_, t)| t.starts_with(prefix))
        .count()
}

fn error_code(result: &Value) -> &str {
    result["error"]["code"].as_str().unwrap_or_default()
}

/// A registered scene that is not active gets no session: `session.create`
/// on it is `SCENE_NOT_FOUND`. A host call on it must not connect that pair
/// and call its servers either.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_on_a_scene_no_session_could_be_made_in_is_refused() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = stubbed("mcp-dormant-scene", &stub)
        .settings(json!({ "mcp": { "servers": { "toy": toy("toy", &calls, &[])? } } }))
        .before_start(|world, _| {
            world.write_declarative_scene("dormant", &["mcp__toy__ping"], "dormant")?;
            Ok(())
        })
        .start()
        .await?;
    let project = run.project.to_string_lossy().to_string();
    let created = run
        .client
        .call(
            "session.create",
            json!({ "scene": "dormant", "project_root": project }),
        )
        .await?;
    let refused = created.error.expect("a session on an inactive scene");
    assert_eq!(refused.code, SCENE_NOT_FOUND, "{}", refused.message);

    let resp = run
        .client
        .call(
            "mcp.callTool",
            json!({ "server": "toy", "tool": "ping", "arguments": { "message": "x" },
                    "scene": "dormant" }),
        )
        .await?;
    assert_eq!(
        lines_of(&calls, "ping"),
        0,
        "a host call reached a server of a scene no session can be made in: {resp:?}"
    );
    let error = resp.error.expect("refused like session.create");
    assert_eq!(error.code, SCENE_NOT_FOUND, "{}", error.message);
    let listed = run
        .client
        .call("mcp.tools", json!({ "scene": "dormant" }))
        .await?;
    let error = listed.error.expect("listing refused like session.create");
    assert_eq!(error.code, SCENE_NOT_FOUND, "{}", error.message);
    run.finish_allowing_refusals().await;
    Ok(())
}

/// The server dies while answering: the call is `failed`, had reached the
/// server, and is not sent again; the next call reconnects and answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_dies_mid_call_fails_it_once_and_the_next_call_reconnects(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = stubbed("mcp-crash", &stub)
        .settings(json!({ "mcp": { "servers": { "toy": toy("toy", &calls, &[])? } } }))
        .start()
        .await?;
    run.wait_mcp_connected(&["toy"]).await?;
    let crashed = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "crash", "timeout_ms": 30000 }),
    )
    .await?;
    assert_eq!(crashed["ok"], false, "{crashed}");
    assert_eq!(error_code(&crashed), "failed", "{crashed}");
    assert_eq!(crashed["reached_server"], true, "{crashed}");
    assert_eq!(lines_of(&calls, "crash"), 1, "the call was sent again");

    let status = run.client.call("mcp.status", json!({})).await?;
    let again = run
        .client
        .call(
            "mcp.callTool",
            json!({ "server": "toy", "tool": "ping", "arguments": { "message": "back" },
                    "timeout_ms": 30000 }),
        )
        .await?;
    let again = again
        .result
        .unwrap_or_else(|| panic!("after the crash ({status:?}): {:?}", again.error));
    assert_eq!(again["text"], "toy: back", "{again}");
    run.finish_allowing_refusals().await;
    Ok(())
}

/// A call the server turned into an MCP task is stopped at the server when
/// its time runs out: `tasks/cancel` goes out.
#[tokio::test(flavor = "multi_thread")]
async fn a_task_past_its_time_is_cancelled_at_the_server() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = stubbed("mcp-task-timeout", &stub)
        .settings(json!({ "mcp": { "servers": {
            "toy": toy("toy", &calls, &["--task"])?
        } } }))
        .start()
        .await?;
    run.wait_mcp_connected(&["toy"]).await?;
    let timed = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "work", "arguments": { "steps": 600, "step_ms": 500 },
                "timeout_ms": 1500 }),
    )
    .await?;
    assert_eq!(error_code(&timed), "timeout", "{timed}");
    assert_eq!(timed["reached_server"], true, "{timed}");
    assert_eq!(
        lines_of(&calls, "work:task"),
        1,
        "{:?}",
        McpToy::calls(&calls)
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while lines_of(&calls, "work:cancel-requested") == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "no tasks/cancel reached the server: {:?}",
            McpToy::calls(&calls)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    run.finish_allowing_refusals().await;
    Ok(())
}

/// The server's own `tool_timeout_ms`, shorter than the call's, still ends
/// the call.
#[tokio::test(flavor = "multi_thread")]
async fn the_servers_shorter_tool_timeout_ends_the_call() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut server = toy("toy", &calls, &[])?;
    server["tool_timeout_ms"] = json!(500);
    let mut run = stubbed("mcp-tool-timeout", &stub)
        .settings(json!({ "mcp": { "servers": { "toy": server } } }))
        .start()
        .await?;
    run.wait_mcp_connected(&["toy"]).await?;
    let timed = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "slow", "arguments": { "ms": 600000 },
                "timeout_ms": 120000 }),
    )
    .await?;
    assert_eq!(error_code(&timed), "timeout", "{timed}");
    assert_eq!(timed["reached_server"], true, "{timed}");
    let message = timed["error"]["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains("120000 ms"),
        "ended by the call's time, not the server's: {timed}"
    );
    run.finish_allowing_refusals().await;
    Ok(())
}

/// An untrusted project's own hooks never run and its own servers never
/// start, also when a host lists and calls tools on that project: only the
/// global server is there to call.
#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_projects_hooks_and_servers_stay_off_for_host_calls() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let hooked = scratch.path().join("hook-ran");
    let started = scratch.path().join("own-started");
    let hook = scratch.path().join("hook.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncat > /dev/null\ntouch '{}'\necho '{{}}'\n",
            hooked.display()
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    let own = json!({
        "type": "stdio", "command": "/bin/sh",
        "args": ["-c", format!("touch '{}'; exec '{}' --name own", started.display(),
                               McpToy::binary()?.display())],
    });
    let global = toy("global", &calls, &[])?;
    let mut run = stubbed("mcp-untrusted-hooks", &stub)
        .settings(json!({
            "mcp": { "servers": { "own": own } },
            "hooks": { "PreToolUse": [{ "type": "command", "command": hook, "timeout": 30000 }] }
        }))
        .untrusted()
        .before_start(move |world, _| {
            world.write_global_settings(&json!({ "mcp": { "servers": { "global": global } } }))
        })
        .start()
        .await?;
    run.wait_mcp_connected(&["global"]).await?;
    let listed = run.client.call("mcp.tools", json!({})).await?;
    let answered = call_tool(
        &mut run.client,
        json!({ "server": "global", "tool": "ping", "arguments": { "message": "x" } }),
    )
    .await?;
    assert_eq!(answered["text"], "toy: x", "{answered} {listed:?}");
    assert!(!hooked.exists(), "the untrusted project's hook ran");
    assert!(!started.exists(), "the untrusted project's server started");
    run.finish_allowing_refusals().await;
    Ok(())
}

/// A managed ask is one only a person may answer; a host's call is not a
/// person, so the call is refused before the server hears of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_managed_ask_refuses_a_host_call() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let managed = scratch.path().join("managed-settings.json");
    std::fs::write(
        &managed,
        json!({ "permissions": { "ask": ["mcp__toy__ping"] } }).to_string(),
    )?;
    let managed = managed.to_string_lossy().to_string();
    let mut run = stubbed("mcp-managed-ask", &stub)
        .settings(json!({ "mcp": { "servers": { "toy": toy("toy", &calls, &[])? } } }))
        .mode(blackbox::Mode::Spawned)
        .options(move |o| o.arg("--managed-settings").arg(managed.clone()))
        .start()
        .await?;
    let project = run.project.to_string_lossy().to_string();
    let asked = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "ping", "arguments": { "message": "x" },
                "project_root": project }),
    )
    .await?;
    assert_eq!(error_code(&asked), "refused", "{asked}");
    assert_eq!(asked["reached_server"], false, "{asked}");
    let echoed = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "echo", "arguments": { "text": "fine" },
                "project_root": project }),
    )
    .await?;
    assert_eq!(echoed["text"], "fine", "{echoed}");
    assert_eq!(lines_of(&calls, "ping"), 0);
    run.finish_allowing_refusals().await;
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
  if (call.tool === "mcp__toy__ping") {
    return { action: "respond", text: "toy: made up" };
  }
  return null;
}
function onResult(result) {
  if (result.tool === "mcp__toy__echo") {
    return "rewritten";
  }
  return null;
}
"#;

/// Scripts can answer in the server's place or rewrite what it said; the
/// host then gets `answered_by_middleware`, never the made-up or rewritten
/// answer as the server's — and `reached_server` says whether the server
/// was called at all.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_a_script_gave_or_rewrote_is_not_handed_over_as_the_servers() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = stubbed("mcp-shim", &stub)
        .settings(json!({ "mcp": { "servers": { "toy": toy("toy", &calls, &[])? } } }))
        .start()
        .await?;
    run.wait_mcp_connected(&["toy"]).await?;
    let source = tempfile::tempdir()?;
    std::fs::write(source.path().join("plugin.toml"), SHIM_MANIFEST)?;
    std::fs::write(source.path().join("shim.js"), SHIM_JS)?;
    let (archive, _) = package_dir(source.path(), run.world.root(), "shim")?;
    let installed = run
        .client
        .plugin_install_and_enable(
            json!({ "name": "shim", "version": "1.0.0", "download_url": file_url(&archive) }),
        )
        .await?;
    let installed = installed.result.unwrap_or_default();
    assert_eq!(installed["success"], true, "{installed}");

    let made_up = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "ping", "arguments": { "message": "x" } }),
    )
    .await?;
    assert_eq!(error_code(&made_up), "answered_by_middleware", "{made_up}");
    assert_eq!(made_up["reached_server"], false, "{made_up}");
    assert!(made_up.get("text").is_none(), "{made_up}");
    assert_eq!(lines_of(&calls, "ping"), 0);

    let rewritten = call_tool(
        &mut run.client,
        json!({ "server": "toy", "tool": "echo", "arguments": { "text": "said" } }),
    )
    .await?;
    assert_eq!(
        error_code(&rewritten),
        "answered_by_middleware",
        "{rewritten}"
    );
    assert_eq!(rewritten["reached_server"], true, "{rewritten}");
    assert!(
        !rewritten.to_string().contains("said") && !rewritten.to_string().contains("rewritten"),
        "{rewritten}"
    );
    assert_eq!(lines_of(&calls, "echo"), 1);
    run.finish_allowing_refusals().await;
    Ok(())
}

/// A `PostToolUse` block refuses a call the server already answered, which
/// `reached_server` says; a deny rule refuses one it never heard of.
#[tokio::test(flavor = "multi_thread")]
async fn reached_server_tells_a_block_after_the_answer_from_a_rule_before_it() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = stubbed("mcp-post-block", &stub)
        .settings(json!({
            "mcp": { "servers": { "toy": toy("toy", &calls, &[])? } },
            "permissions": { "deny": ["mcp__toy__ping"] }
        }))
        .start()
        .await?;
    let mut host = run.daemon.connect().await?;
    let set = host
        .daemon_set_hooks(vec![json!({
            "events": ["PostToolUse"], "tools": ["*"], "timeout_ms": 30000, "on_error": "block"
        })])
        .await?;
    anyhow::ensure!(set.error.is_none(), "{set:?}");
    run.wait_mcp_connected(&["toy"]).await?;

    let denied = call_tool(
        &mut host,
        json!({ "server": "toy", "tool": "ping", "arguments": { "message": "x" } }),
    )
    .await?;
    assert_eq!(error_code(&denied), "refused", "{denied}");
    assert_eq!(denied["reached_server"], false, "{denied}");
    assert_eq!(lines_of(&calls, "ping"), 0);

    let id = host
        .send_request(
            "mcp.callTool",
            json!({ "server": "toy", "tool": "echo", "arguments": { "text": "seen" } }),
        )
        .await?;
    let blocked = loop {
        let frame = host
            .next_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("closed before mcp.callTool answered"))?;
        if frame["id"].as_i64() == Some(id) {
            anyhow::ensure!(frame.get("error").is_none(), "{frame}");
            break frame["result"].clone();
        }
        if frame["params"]["event"]["kind"] != "hook_call" {
            continue;
        }
        let params = &frame["params"];
        host.send_request(
            "session.respondHook",
            json!({ "session_id": params["session_id"], "call_id": params["event"]["call_id"],
                    "decision": "block", "message": "not after all" }),
        )
        .await?;
    };
    assert_eq!(error_code(&blocked), "refused", "{blocked}");
    assert_eq!(blocked["reached_server"], true, "{blocked}");
    assert!(!blocked.to_string().contains("seen"), "{blocked}");
    assert_eq!(lines_of(&calls, "echo"), 1);
    run.finish_allowing_refusals().await;
    Ok(())
}
