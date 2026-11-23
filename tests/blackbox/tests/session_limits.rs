//! What a session spends over its turns: `session.get`'s `usage` counts its
//! delegates' calls as they are made, and a failed turn's, and survives a
//! restart.

use std::time::Duration;

use blackbox::{Block, LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Reply};
use rpc_client::ToolCallReply;
use serde_json::{json, Value};

const ROOT: &str = "ROOT-MARK";
const WAIT: Duration = Duration::from_secs(30);

fn stubbed(name: &str, stub: &ProviderStub, settings: Value) -> LiveRunBuilder {
    let mut merged = json!({ "permissions": { "allow": ["Agent", "Glob", "HostGate"] } });
    if let (Some(base), Some(extra)) = (merged.as_object_mut(), settings.as_object()) {
        base.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    LiveRun::builder(name)
        .settings(merged)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
}

async fn session(run: &mut LiveRun, options: Value) -> anyhow::Result<String> {
    let mut options = options;
    options["auto_name"] = json!(false);
    run.session_with(json!({ "options": options })).await
}

fn delegate(prompt: &str, background: bool) -> Reply {
    Reply::calls(
        "a1",
        "Agent",
        json!({ "description": "helper", "prompt": prompt, "background": background }),
    )
}

fn gate_tool() -> Value {
    json!({
        "name": "HostGate",
        "description": "Waits for the host.",
        "input_schema": { "type": "object", "properties": {} },
        "read_only": true,
        "subagents": true
    })
}

/// `session.get`'s `usage`, as input and output tokens.
async fn usage(run: &mut LiveRun, s: &str) -> anyhow::Result<(u64, u64)> {
    let got = run
        .client
        .call("session.get", json!({ "session_id": s }))
        .await?
        .result
        .unwrap_or_default();
    let figure = |k: &str| got["usage"][k].as_u64().unwrap_or_default();
    Ok((figure("input_tokens"), figure("output_tokens")))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sessions_usage_counts_a_background_sub_agent_as_it_spends() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-usage-live", &stub, json!({}))
        .start()
        .await?;
    let s = session(&mut run, json!({ "host_tools": [gate_tool()] })).await?;
    let host = run.watch(&s, true).await?;
    stub.script_when(
        ROOT,
        [
            Reply::spent(1000, 100, delegate("wait at the gate", true)),
            Reply::spent(1000, 100, Reply::text("started it")),
            Reply::spent(10, 1, Reply::text("noted")),
        ],
    );
    stub.script([
        Reply::spent(100, 50, Reply::calls("h1", "HostGate", json!({}))),
        Reply::spent(400, 40, Reply::text("through the gate")),
    ]);
    run.turn(&s, &format!("{ROOT} start it")).await?;

    let call = host
        .wait_for("the gate", WAIT, |e| e["kind"] == "host_tool_call")
        .await?;
    assert_eq!(
        usage(&mut run, &s).await?,
        (2100, 250),
        "the running sub-agent's first call is not in the session's usage"
    );
    let answered = run
        .client
        .session_respond_tool_call(
            &s,
            call["event"]["call_id"].as_str().unwrap_or_default(),
            ToolCallReply::Result {
                content: json!([{ "type": "text", "text": "open" }]),
                is_error: false,
            },
        )
        .await?;
    assert!(answered.error.is_none(), "{answered:?}");
    run.wait_tasks_settled(&s, WAIT).await?;
    assert_eq!(usage(&mut run, &s).await?, (2500, 290));

    run.turn(&s, &format!("{ROOT} anything else?")).await?;
    assert_eq!(usage(&mut run, &s).await?, (2510, 291));
    drop(host);

    run.restart().await?;
    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": s }))
        .await?;
    assert!(resumed.error.is_none(), "{resumed:?}");
    assert_eq!(
        usage(&mut run, &s).await?,
        (2510, 291),
        "a restart lost what the session had spent"
    );
    run.finish().await;
    Ok(())
}

/// The stream opens, reports its input, then fails with an error nothing
/// retries: the turn fails, and what the provider reported is spent.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_turns_call_is_in_the_sessions_usage() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-usage-failed", &stub, json!({}))
        .start()
        .await?;
    let s = session(&mut run, json!({})).await?;
    stub.script([Reply::errored(
        vec![Block::text("half an answer")],
        "invalid_request_error",
        "this request is refused",
    )]);
    let failed = run.turn(&s, "go").await;
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(stub.call_count(), 1);
    let (input, _) = usage(&mut run, &s).await?;
    assert_eq!(
        input,
        blackbox::provider::INPUT_TOKENS,
        "the failed call's input is not in the session's usage"
    );
    run.finish_allowing_refusals().await;
    Ok(())
}
