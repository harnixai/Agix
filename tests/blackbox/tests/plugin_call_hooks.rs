//! Hooks a connection registered with `daemon.setHooks` asked about a
//! `plugin.call` in no session: the `hook_call` goes to the registering
//! connection under the call's own session id, marked `plugin_call`, and is
//! answered with `session.respondHook`; a registrant that goes away mid-call
//! ends the ask at once by its `on_error`. Only what the RPC shows.

use blackbox::ProviderStub;
use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::plugin_calls::{error_of, in_dir, with_xflow};

async fn register(client: &mut DaemonRpcClient, on_error: &str) -> anyhow::Result<()> {
    let set = client
        .daemon_set_hooks(vec![json!({
            "events": ["PreToolUse"], "tools": ["*"], "timeout_ms": 30000, "on_error": on_error
        })])
        .await?;
    anyhow::ensure!(set.error.is_none(), "{set:?}");
    anyhow::ensure!(
        set.result
            .as_ref()
            .is_some_and(|r| r["refused"] == json!([])),
        "{set:?}"
    );
    Ok(())
}

fn respond_params(asked: &Value, block: Option<String>) -> Value {
    let mut params = json!({
        "session_id": asked["session_id"],
        "call_id": asked["event"]["call_id"],
    });
    if let Some(message) = block {
        params["decision"] = "block".into();
        params["message"] = message.into();
    }
    params
}

/// What one `plugin.call` made on the connection that registered the hooks
/// came to: its result, the `session.event` params of each `hook_call`
/// asked meanwhile, and the call ids whose `hook_call_resolved` said
/// `answered` before the result.
struct Answered {
    result: Value,
    asked: Vec<Value>,
    answered: Vec<String>,
}

/// Make `params` a `plugin.call` on `client`, answering each `hook_call` it
/// is handed with `block` (a block message, or proceed).
async fn call_answering(
    client: &mut DaemonRpcClient,
    params: Value,
    block: impl Fn(&Value) -> Option<String>,
) -> anyhow::Result<Answered> {
    let id = client.send_request("plugin.call", params).await?;
    let (mut asked, mut answered, mut replies) = (Vec::new(), Vec::new(), Vec::new());
    loop {
        let frame = client
            .next_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("closed before the plugin.call answered"))?;
        if frame["id"].as_i64() == Some(id) {
            anyhow::ensure!(frame.get("error").is_none(), "{frame}");
            return Ok(Answered {
                result: frame["result"].clone(),
                asked,
                answered,
            });
        }
        if let Some(reply) = frame["id"].as_i64().filter(|r| replies.contains(r)) {
            anyhow::ensure!(
                frame.get("error").is_none(),
                "respondHook #{reply}: {frame}"
            );
            continue;
        }
        let event = &frame["params"]["event"];
        match event["kind"].as_str() {
            Some("hook_call") => {
                let params = frame["params"].clone();
                let respond = respond_params(&params, block(&params));
                replies.push(client.send_request("session.respondHook", respond).await?);
                asked.push(params);
            }
            Some("hook_call_resolved") if event["resolution"] == "answered" => {
                answered.push(event["call_id"].as_str().unwrap_or_default().to_string());
            }
            _ => {}
        }
    }
}

fn tool_of(asked: &Value) -> &str {
    asked["event"]["input"]["tool_name"]
        .as_str()
        .unwrap_or_default()
}

/// The acceptance case of the hang: the connection that registered a
/// `PreToolUse` hook on every tool calls a function in no session three
/// times in a row, is asked about each call under that call's own session
/// id, answers, and each call ends normally. In a session nothing changes:
/// the frame is the session's, with no marker.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_in_no_session_asks_the_registrant_who_answers_it() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_xflow("hook-dir", &stub).await?;
    let mut host = run.daemon.connect().await?;
    register(&mut host, "block").await?;

    let mut temp_ids = Vec::new();
    for n in 0..3 {
        let mut params = in_dir(&run, "xflow.check", json!({ "input": { "change": "c1" } }));
        if n == 2 {
            params["run_id"] = "gate-3".into();
        }
        let call = call_answering(&mut host, params, |_| None).await?;
        assert_eq!(call.result["ok"], true, "call {n}: {}", call.result);
        assert_eq!(call.result["value"]["gate"], "green", "{}", call.result);
        let tools: Vec<&str> = call.asked.iter().map(tool_of).collect();
        assert_eq!(tools, ["PluginCall", "Read"], "call {n}");
        let sid = call.asked[0]["session_id"].as_str().unwrap_or_default();
        assert!(!sid.is_empty(), "{:?}", call.asked);
        let want = match n {
            2 => json!({ "function": "xflow.check", "run_id": "gate-3" }),
            _ => json!({ "function": "xflow.check" }),
        };
        for asked in &call.asked {
            assert_eq!(asked["session_id"], sid, "one call, one session: {asked}");
            assert_eq!(asked["event"]["input"]["session_id"], sid, "{asked}");
            assert_eq!(asked["event"]["plugin_call"], want, "{asked}");
        }
        let ids: Vec<&str> = call
            .asked
            .iter()
            .map(|a| a["event"]["call_id"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            call.answered, ids,
            "each ask was answered before the result"
        );
        temp_ids.push(sid.to_string());
    }
    temp_ids.sort();
    temp_ids.dedup();
    assert_eq!(temp_ids.len(), 3, "each call has a session of its own");
    let late = host
        .call(
            "session.respondHook",
            json!({ "session_id": temp_ids[0], "call_id": "hk_late" }),
        )
        .await?;
    assert!(
        late.error.is_some(),
        "the session of an ended call is gone: {late:?}"
    );
    let sessions = run.client.session_list().await?;
    assert_eq!(sessions.result.unwrap_or_default()["sessions"], json!([]));

    let session = run.session().await?;
    let params =
        json!({ "function": "xflow.check", "session_id": session, "input": { "change": "c2" } });
    let call = call_answering(&mut host, params, |_| None).await?;
    assert_eq!(call.result["ok"], true, "{}", call.result);
    assert!(!call.asked.is_empty());
    for asked in &call.asked {
        assert_eq!(asked["session_id"], session.as_str(), "{asked}");
        assert!(asked["event"].get("plugin_call").is_none(), "{asked}");
    }
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_registrant_that_blocks_the_call_refuses_it() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let run = with_xflow("hook-block", &stub).await?;
    let mut host = run.daemon.connect().await?;
    register(&mut host, "allow").await?;
    let params = in_dir(&run, "xflow.check", json!({ "input": { "change": "c1" } }));
    let call = call_answering(&mut host, params, |asked| {
        (tool_of(asked) == "PluginCall").then(|| "the host forbids this gate".to_string())
    })
    .await?;
    let (code, message) = error_of(&call.result);
    assert_eq!(code, "refused", "{}", call.result);
    assert!(message.contains("the host forbids this gate"), "{message}");
    let tools: Vec<&str> = call.asked.iter().map(tool_of).collect();
    assert_eq!(tools, ["PluginCall"], "the blocked call made no calls");
    run.finish().await;
    Ok(())
}

/// Hooks are the daemon's: one connection's hook is asked about another
/// connection's call, and only it may answer — the caller's answer to it is
/// refused, as in a session.
#[tokio::test(flavor = "multi_thread")]
async fn another_connections_hook_is_asked_and_only_it_answers() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let run = with_xflow("hook-other", &stub).await?;
    let mut host = run.daemon.connect().await?;
    register(&mut host, "block").await?;
    let (asked_tx, mut asked_rx) = mpsc::unbounded_channel::<Value>();
    let (go_tx, mut go_rx) = mpsc::unbounded_channel::<()>();
    let answering = tokio::spawn(async move {
        let mut released = false;
        while let Ok(Some(frame)) = host.next_frame().await {
            if frame["params"]["event"]["kind"] != "hook_call" {
                continue;
            }
            let params = frame["params"].clone();
            let _ = asked_tx.send(params.clone());
            if !released {
                released = go_rx.recv().await.is_some();
            }
            let _ = host
                .send_request("session.respondHook", respond_params(&params, None))
                .await;
        }
    });

    let mut caller = run.daemon.connect().await?;
    let params = in_dir(&run, "xflow.check", json!({ "input": { "change": "c1" } }));
    let id = caller.send_request("plugin.call", params).await?;
    let first = asked_rx
        .recv()
        .await
        .ok_or_else(|| anyhow::anyhow!("the hook was never asked"))?;
    assert_eq!(
        first["event"]["plugin_call"]["function"], "xflow.check",
        "{first}"
    );
    let wrong = caller
        .call("session.respondHook", respond_params(&first, None))
        .await?;
    let error = wrong
        .error
        .ok_or_else(|| anyhow::anyhow!("the caller answered another connection's hook"))?;
    assert_eq!(error.code, -32602, "{}", error.message);
    assert!(
        error
            .message
            .contains("answered by the connection that provides it"),
        "{}",
        error.message
    );
    go_tx.send(())?;
    let result = loop {
        let frame = caller
            .next_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("closed"))?;
        if frame["id"].as_i64() == Some(id) {
            break frame["result"].clone();
        }
    };
    assert_eq!(result["ok"], true, "{result}");
    answering.abort();
    run.finish().await;
    Ok(())
}

/// A registrant whose connection closes while it is asked ends the ask at
/// once by its `on_error`: `block` refuses the call, `allow` lets it run.
/// The call's own time is shorter than the hook's, so a call left waiting
/// on the hook would answer `timeout` instead.
#[tokio::test(flavor = "multi_thread")]
async fn a_registrant_that_goes_away_ends_the_ask_by_its_on_error() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let run = with_xflow("hook-gone", &stub).await?;
    for (on_error, ok) in [("block", false), ("allow", true)] {
        let mut host = run.daemon.connect().await?;
        register(&mut host, on_error).await?;
        let mut caller = run.daemon.connect().await?;
        let params = in_dir(
            &run,
            "xflow.check",
            json!({ "input": { "change": "c1" }, "timeout_ms": 20000 }),
        );
        let id = caller.send_request("plugin.call", params).await?;
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
        assert_eq!(result["ok"], ok, "{on_error}: {result}");
        if !ok {
            let (code, message) = error_of(&result);
            assert_eq!(code, "refused", "{result}");
            assert!(message.contains("on_error is block"), "{message}");
        }
    }
    run.finish().await;
    Ok(())
}
