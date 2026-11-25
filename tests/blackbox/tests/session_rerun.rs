//! `session.rerunCall` from outside, against a scripted model: a recorded
//! call sent again with one system block replaced reaches the model as
//! edited and answers with the new response and its diff, while the
//! recording's files stay byte for byte as they were and the session's
//! history does not move; a closed session reruns without coming back into
//! memory; bad edits, a session that records nothing and an id shaped like a
//! path are refused.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use blackbox::{LiveConfig, LiveRun, ProviderStub, Purpose, Reply};
use serde_json::{json, Value};

const INVALID_PARAMS: i64 = -32602;

async fn stubbed(name: &str, stub: &ProviderStub) -> anyhow::Result<LiveRun> {
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .start()
        .await
}

fn recordings(run: &LiveRun) -> PathBuf {
    run.world.root().join("recordings")
}

/// A session that records under `recordings(run)` and makes no naming call,
/// so nothing writes to its recording after its turn.
async fn recorded_session(run: &mut LiveRun) -> anyhow::Result<String> {
    let dir = recordings(run);
    std::fs::create_dir_all(&dir)?;
    run.session_with(json!({ "options": {
        "auto_name": false,
        "recorder": { "mode": "record", "dir": dir.to_string_lossy() },
    } }))
    .await
}

fn ok(response: daemon::rpc::RpcResponse) -> anyhow::Result<Value> {
    if let Some(error) = response.error {
        anyhow::bail!("{} {} {:?}", error.code, error.message, error.data);
    }
    Ok(response.result.unwrap_or_default())
}

fn refused(response: daemon::rpc::RpcResponse) -> anyhow::Result<(i64, String)> {
    let error = response
        .error
        .ok_or_else(|| anyhow::anyhow!("not refused: {:?}", response.result))?;
    Ok((i64::from(error.code), error.message))
}

async fn rerun(run: &mut LiveRun, params: Value) -> anyhow::Result<daemon::rpc::RpcResponse> {
    run.client.call("session.rerunCall", params).await
}

/// The session's recorded calls once every one of them has ended.
async fn settled_calls(run: &mut LiveRun, session: &str) -> anyhow::Result<Vec<Value>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = ok(run.client.session_calls(session, 0, None).await?)?;
        let calls = list["calls"].as_array().cloned().unwrap_or_default();
        if !calls.is_empty() && calls.iter().all(|c| c["status"] == "ok") {
            return Ok(calls);
        }
        anyhow::ensure!(Instant::now() < deadline, "calls never settled: {list}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The index of the first conversation call (not naming, memory, …).
fn conversation_call(calls: &[Value]) -> u64 {
    calls
        .iter()
        .find(|c| c["purpose"].is_null())
        .and_then(|c| c["index"].as_u64())
        .unwrap_or_else(|| panic!("no conversation call: {calls:?}"))
}

/// Every file under `dir`, by path, with its bytes.
fn snapshot(dir: &Path) -> anyhow::Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(at) = pending.pop() {
        for entry in std::fs::read_dir(&at)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                out.insert(path.clone(), std::fs::read(&path)?);
            }
        }
    }
    Ok(out)
}

async fn message_count(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    let page = ok(run
        .client
        .call(
            "session.history",
            json!({ "session_id": session, "limit": 500 }),
        )
        .await?)?;
    Ok(page["total"].clone())
}

async fn spent(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    let usage = ok(run.client.session_get(session).await?)?["usage"].clone();
    anyhow::ensure!(usage.is_object(), "no usage: {usage}");
    Ok(usage)
}

/// The first conversation call rerun with system block 1 replaced: the model gets the new text, the answer comes back with
/// its diff against the recorded one, and neither the recording nor the session changes.
#[tokio::test(flavor = "multi_thread")]
async fn a_rerun_with_one_block_replaced_sends_it_and_leaves_the_recording_alone(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("rerun-edit", &stub).await?;
    stub.script([Reply::text("RECORDED-ANSWER")]);
    let session = recorded_session(&mut run).await?;
    run.turn(&session, "say something").await?;
    let index = conversation_call(&settled_calls(&mut run, &session).await?);
    let detail = ok(run.client.session_call(&session, index).await?)?;
    let blocks = detail["request"]["system"].as_array().expect("system");
    assert!(blocks.len() >= 2, "{detail}");
    let replaced = blocks[1]["text"].as_str().unwrap_or_default().to_string();

    let before = snapshot(&recordings(&run))?;
    assert!(!before.is_empty(), "nothing was recorded");
    let history_before = message_count(&mut run, &session).await?;
    let spent_before = spent(&mut run, &session).await?;
    let sent_before = stub.call_count();
    stub.script([Reply::text("RERUN-ANSWER")]);
    let result = ok(rerun(
        &mut run,
        json!({ "session_id": session, "index": index,
                "edits": { "system": [ { "index": 1, "text": "EDITED-BLOCK-MARK" } ] } }),
    )
    .await?)?;

    let sent = stub.calls();
    assert_eq!(sent.len(), sent_before + 1, "one request went out");
    let request = sent.last().expect("the rerun's request");
    assert_eq!(request.purpose(), Purpose::Turn, "the tool table went too");
    assert!(
        request.system_contains("EDITED-BLOCK-MARK"),
        "{}",
        request.system_text()
    );
    if replaced.len() > 40 {
        assert!(
            !request.system_contains(&replaced),
            "the old block went out"
        );
    }

    assert_eq!(result["index"], index, "{result}");
    assert_eq!(result["generation"], detail["generation"], "{result}");
    assert!(result["provider"].is_string(), "{result}");
    let response = &result["response"];
    assert_eq!(response["status"], "ok", "{result}");
    assert_eq!(response["text"], "RERUN-ANSWER", "{result}");
    assert!(response["usage"].is_object(), "{result}");
    let diff = &result["diff"];
    assert_eq!(diff["same"], false, "{result}");
    assert_eq!(diff["text"]["same"], false, "{result}");
    assert_eq!(diff["text"]["recorded"], "RECORDED-ANSWER", "{result}");
    assert_eq!(diff["text"]["live"], "RERUN-ANSWER", "{result}");
    assert_eq!(diff["tool_uses"]["same"], true, "{result}");
    assert_eq!(diff["stop_reason"]["same"], true, "{result}");
    assert!(diff["usage"]["recorded"].is_object(), "{result}");
    assert!(diff["usage"]["live"].is_object(), "{result}");

    assert!(
        before == snapshot(&recordings(&run))?,
        "the recording's files changed"
    );
    assert_eq!(message_count(&mut run, &session).await?, history_before);
    assert_eq!(
        spent(&mut run, &session).await?,
        spent_before,
        "counted as the session's"
    );
    run.finish().await;
    Ok(())
}

/// A closed session's call reruns, and the session stays out of memory.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_session_reruns_without_being_loaded() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("rerun-cold", &stub).await?;
    stub.script([Reply::text("WARM")]);
    let session = recorded_session(&mut run).await?;
    run.turn(&session, "hello").await?;
    let index = conversation_call(&settled_calls(&mut run, &session).await?);
    ok(run.client.session_close(&session).await?)?;

    stub.script([Reply::text("WARM")]);
    let result = ok(rerun(&mut run, json!({ "session_id": session, "index": index })).await?)?;
    assert_eq!(result["response"]["text"], "WARM", "{result}");
    assert_eq!(result["diff"]["same"], true, "{result}");
    let page = ok(run
        .client
        .call("session.history", json!({ "session_id": session }))
        .await?)?;
    assert_eq!(page["active"], false, "{page}");
    run.finish().await;
    Ok(())
}

/// Edits that cannot apply, a session with no recording and an id shaped like a path are all
/// INVALID_PARAMS, and none of them reaches the model.
#[tokio::test(flavor = "multi_thread")]
async fn bad_reruns_are_refused_without_calling_the_model() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("rerun-refused", &stub).await?;
    stub.script([Reply::text("ONE"), Reply::text("TWO")]);
    let session = recorded_session(&mut run).await?;
    run.turn(&session, "hello").await?;
    let calls = settled_calls(&mut run, &session).await?;
    let index = conversation_call(&calls);
    let messages = ok(run.client.session_call(&session, index).await?)?["request"]["messages"]
        .as_array()
        .map_or(0, Vec::len);
    let plain = run.session().await?;
    run.turn(&plain, "unrecorded").await?;
    let sent = stub.call_count();

    let edits = |edits: Value| json!({ "session_id": session, "index": index, "edits": edits });
    let cases = [
        (edits(json!({ "drop_messages": [messages] })), "past"),
        (
            edits(json!({ "drop_messages": (0..messages).collect::<Vec<_>>() })),
            "nothing to send",
        ),
        (
            edits(json!({ "system": [ { "index": 0, "source": "identity", "text": "x" } ] })),
            "not both",
        ),
        (
            edits(json!({ "system": [ { "source": "no-such-block", "text": "x" } ] })),
            "no system block named",
        ),
        (
            edits(json!({ "system": [ { "index": 0 } ] })),
            "`text` is required",
        ),
        (
            edits(json!({ "system": [ { "index": 999, "text": null } ] })),
            "past",
        ),
        (
            json!({ "session_id": session, "index": calls.len() }),
            "no call at index",
        ),
        (
            json!({ "session_id": plain, "index": 0 }),
            "has no recording",
        ),
        (
            json!({ "session_id": "../../../../etc/passwd", "index": 0 }),
            "invalid session_id",
        ),
    ];
    for (params, says) in cases {
        let (code, message) = refused(rerun(&mut run, params.clone()).await?)?;
        assert_eq!(code, INVALID_PARAMS, "{params}: {message}");
        assert!(message.contains(says), "{params}: {message}");
    }
    assert_eq!(stub.call_count(), sent, "a refused rerun reached the model");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_advertises_session_rerun() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("rerun-ping", &stub).await?;
    let ping = ok(run.client.call("daemon.ping", Value::Null).await?)?;
    assert!(
        ping["capabilities"]
            .as_array()
            .is_some_and(|c| c.iter().any(|c| c == "session_rerun")),
        "{ping}"
    );
    run.finish().await;
    Ok(())
}
