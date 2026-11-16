//! `session.calls`, `session.call`, `session.callBlob` and `session.search`
//! from outside, against a scripted model: what a recorded session lists as
//! its model calls (sub-agents and the naming call included), paging, the
//! system prompt a call carries against what the model was sent, oversized
//! parts handed out by handle, a session with no recording, content search
//! over what was typed and answered, and both reads on a session that is not
//! in memory.

use std::time::{Duration, Instant};

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply};
use serde_json::{json, Value};

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    stub.script_when(
        "标题生成器",
        (0..8).map(|i| Reply::text(format!("title {i}"))),
    );
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

fn recorded(run: &LiveRun) -> anyhow::Result<Value> {
    let dir = run.world.root().join("recordings");
    std::fs::create_dir_all(&dir)?;
    Ok(json!({ "options": { "recorder": {
        "mode": "record", "dir": dir.to_string_lossy()
    } } }))
}

fn delegation(sub_answer: &str, parent_answer: &str) -> Vec<Reply> {
    vec![
        Reply::calls(
            "a1",
            "Agent",
            json!({ "description": "helper", "prompt": "Answer with the helper word." }),
        ),
        Reply::text(sub_answer),
        Reply::text(parent_answer),
    ]
}

fn ok(response: daemon::rpc::RpcResponse) -> anyhow::Result<Value> {
    if let Some(error) = response.error {
        anyhow::bail!("{} {} {:?}", error.code, error.message, error.data);
    }
    Ok(response.result.unwrap_or_default())
}

async fn refused_code(response: daemon::rpc::RpcResponse) -> anyhow::Result<i64> {
    let error = response
        .error
        .ok_or_else(|| anyhow::anyhow!("not refused: {:?}", response.result))?;
    Ok(i64::from(error.code))
}

async fn calls_of(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    ok(run.client.session_calls(session, 0, None).await?)
}

async fn history(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    ok(run
        .client
        .call(
            "session.history",
            json!({ "session_id": session, "limit": 500 }),
        )
        .await?)
}

fn rows(list: &Value) -> Vec<Value> {
    list["calls"].as_array().cloned().unwrap_or_default()
}

async fn until_calls(
    run: &mut LiveRun,
    session: &str,
    ready: impl Fn(&[Value]) -> bool,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = calls_of(run, session).await?;
        if ready(&rows(&list)) {
            return Ok(list);
        }
        anyhow::ensure!(Instant::now() < deadline, "calls never settled: {list}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn conversation_rows(list: &Value, session: &str) -> Vec<Value> {
    rows(list)
        .into_iter()
        .filter(|r| r["purpose"].is_null() && r["session_id"] == session)
        .collect()
}

/// One recording holds the parent's calls, its sub-agent's calls tagged with the parent, and the naming call.
#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_session_lists_its_own_sub_agent_and_title_calls() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("calls-list", &stub).start().await?;
    stub.script(delegation("SUB-WORD", "PARENT-DONE"));
    let extra = recorded(&run)?;
    let session = run.scene_session("harnix", extra).await?;
    run.turn(&session, "delegate it").await?;
    run.wait_tasks_settled(&session, Duration::from_secs(10))
        .await?;

    let list = until_calls(&mut run, &session, |r| {
        r.iter().any(|c| c["purpose"] == "title")
    })
    .await?;
    assert_eq!(list["recording"]["name"], session.as_str(), "{list}");
    let all = rows(&list);
    assert!(all.iter().all(|c| c["status"] == "ok"), "{list}");
    let conversation = conversation_rows(&list, &session);
    assert_eq!(conversation.len(), 2, "parent's two calls: {list}");
    assert!(
        conversation.iter().all(|c| !c["turn_id"].is_null()),
        "{list}"
    );
    let sub: Vec<_> = all
        .iter()
        .filter(|c| c["session_id"] != session.as_str())
        .collect();
    assert!(!sub.is_empty(), "no sub-agent rows: {list}");
    assert!(
        sub.iter()
            .all(|c| c["parent_session_id"] == session.as_str()),
        "{list}"
    );
    assert_eq!(
        all.iter().filter(|c| c["purpose"] == "title").count(),
        1,
        "{list}"
    );
    run.finish().await;
    Ok(())
}

/// Reading from `next` returns only the calls made since.
#[tokio::test(flavor = "multi_thread")]
async fn reading_from_next_returns_only_the_calls_made_since() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("calls-paging", &stub).start().await?;
    stub.script([Reply::text("first"), Reply::text("second")]);
    let extra = recorded(&run)?;
    let session = run.session_with(extra).await?;
    run.turn(&session, "one").await?;
    let first = until_calls(&mut run, &session, |r| !r.is_empty()).await?;
    let next = first["next"].as_u64().expect("next");
    let total = first["total"].as_u64().expect("total");

    run.turn(&session, "two").await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let later = loop {
        let later = ok(run.client.session_calls(&session, next, None).await?)?;
        if conversation_rows(&later, &session).len() == 1 {
            break later;
        }
        anyhow::ensure!(Instant::now() < deadline, "no new rows: {later}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        rows(&later)
            .iter()
            .all(|c| c["index"].as_u64().unwrap() >= next),
        "{later}"
    );
    assert!(later["total"].as_u64().unwrap() > total, "{later}");
    run.finish().await;
    Ok(())
}

/// A call's inline system blocks carry text and origin, and that text is what the model was sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_calls_system_blocks_are_what_the_model_was_sent() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("calls-system", &stub).start().await?;
    stub.script([Reply::text("hello")]);
    let extra = recorded(&run)?;
    let session = run.session_with(extra).await?;
    run.turn(&session, "hi").await?;
    let list = until_calls(&mut run, &session, |r| !r.is_empty()).await?;
    let index = conversation_rows(&list, &session)[0]["index"]
        .as_u64()
        .expect("index");

    let detail = ok(run.client.session_call(&session, index).await?)?;
    let blocks = detail["request"]["system"].as_array().expect("system");
    assert!(!blocks.is_empty(), "{detail}");
    let sent = stub
        .calls()
        .into_iter()
        .find(|c| c.purpose() == Purpose::Turn)
        .expect("the turn's request")
        .system_text();
    let mut inline = 0;
    for block in blocks.iter().filter(|b| b.get("blob").is_none()) {
        let text = block["text"].as_str().unwrap_or_default();
        assert!(!text.is_empty(), "{block}");
        assert!(
            block["origin"].is_string() || block["origin"].is_object(),
            "{block}"
        );
        assert!(sent.contains(text), "not what was sent: {block}");
        inline += 1;
    }
    assert!(inline > 0, "no inline block: {detail}");
    run.finish().await;
    Ok(())
}

/// Between two turns only the prompt append changes: the rows' digests tell which system block moved and that the tool table did not.
#[tokio::test(flavor = "multi_thread")]
async fn digests_tell_which_fixed_part_changed_between_two_turns() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("calls-digest", &stub).start().await?;
    stub.script([Reply::text("one"), Reply::text("two")]);
    let extra = recorded(&run)?;
    let session = run.session_with(extra).await?;
    run.turn(&session, "first").await?;
    let set = run
        .client
        .call(
            "session.setPrompt",
            json!({ "session_id": session, "append": "CHANGED-APPEND" }),
        )
        .await?;
    anyhow::ensure!(set.error.is_none(), "{set:?}");
    run.turn(&session, "second").await?;
    let list = until_calls(&mut run, &session, |r| {
        r.iter().filter(|c| c["purpose"].is_null()).count() == 2
    })
    .await?;
    let turns = conversation_rows(&list, &session);
    let digests = |row: &Value| -> Vec<(Value, Value)> {
        row["input"]["system"]
            .as_array()
            .expect("system")
            .iter()
            .map(|b| (b["source"].clone(), b["digest"].clone()))
            .collect()
    };
    let (first, second) = (digests(&turns[0]), digests(&turns[1]));
    assert!(first.iter().all(|(_, d)| d.is_string()), "{list}");
    assert_eq!(
        turns[0]["input"]["tools_digest"], turns[1]["input"]["tools_digest"],
        "{list}"
    );
    assert!(turns[0]["input"]["tools_digest"].is_string(), "{list}");
    let changed: Vec<_> = second.iter().filter(|b| !first.contains(b)).collect();
    let kept = second.iter().filter(|b| first.contains(b)).count();
    assert_eq!(changed.len(), 1, "only the append moved: {list}");
    assert!(kept > 0, "{list}");
    run.finish().await;
    Ok(())
}

/// A multi-step turn lists, in one read, what each step said, what it called with and whether the call failed.
#[tokio::test(flavor = "multi_thread")]
async fn rows_carry_the_answer_text_the_arguments_and_whether_each_tool_failed(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("calls-preview", &stub).start().await?;
    run.write_file("present.txt", "PRESENT-BODY\n")?;
    let present = run
        .project
        .join("present.txt")
        .to_string_lossy()
        .to_string();
    stub.script([
        Reply::calls("r1", "Read", json!({ "offset": "not-a-number-marker" })),
        Reply::calls("r2", "Read", json!({ "file_path": present })),
        Reply::text("FINAL-ANSWER"),
    ]);
    let extra = recorded(&run)?;
    let session = run.session_with(extra).await?;
    run.turn(&session, "read both").await?;
    let list = until_calls(&mut run, &session, |r| {
        r.iter().filter(|c| c["purpose"].is_null()).count() == 3
    })
    .await?;
    let steps = conversation_rows(&list, &session);
    let use_of = |step: usize| steps[step]["output"]["tool_uses"][0].clone();
    assert_eq!(use_of(0)["name"], "Read", "{list}");
    assert!(
        use_of(0)["input_preview"]
            .as_str()
            .is_some_and(|p| p.contains("not-a-number-marker")),
        "{list}"
    );
    assert_eq!(use_of(0)["result_is_error"], true, "{list}");
    assert_eq!(use_of(1)["result_is_error"], false, "{list}");
    assert_eq!(steps[2]["output"]["text_preview"], "FINAL-ANSWER", "{list}");
    assert_eq!(steps[2]["output"]["tool_uses"], json!([]), "{list}");
    run.finish().await;
    Ok(())
}

/// A part over 64 KiB comes as a handle, and `session.callBlob` returns it whole.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_message_is_a_blob_that_call_blob_returns_whole() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("calls-blob", &stub).start().await?;
    stub.script([Reply::text("read it")]);
    let extra = recorded(&run)?;
    let session = run.session_with(extra).await?;
    let prompt = "0123456789".repeat(7_000);
    run.turn(&session, &prompt).await?;
    let list = until_calls(&mut run, &session, |r| !r.is_empty()).await?;
    let index = conversation_rows(&list, &session)[0]["index"]
        .as_u64()
        .expect("index");

    let detail = ok(run.client.session_call(&session, index).await?)?;
    let messages = detail["request"]["messages"].as_array().expect("messages");
    let handle = messages
        .iter()
        .find(|m| m.get("blob").is_some())
        .unwrap_or_else(|| panic!("no blob among the messages: {}", detail["request"]));
    let name = handle["blob"].as_str().expect("handle");
    assert!(handle["bytes"].as_u64().unwrap() > 65_536, "{handle}");

    let part = ok(run.client.session_call_blob(&session, name).await?)?;
    assert_eq!(part["blob"], name);
    assert!(
        part["value"].to_string().contains(&prompt),
        "value lacks the prompt"
    );

    let bad = run.client.session_call_blob(&session, "s.../../x").await?;
    assert_eq!(refused_code(bad).await?, -32602);
    run.finish().await;
    Ok(())
}

/// A session with no recorder answers an empty list, not an error.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_without_a_recorder_has_no_calls() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("calls-none", &stub).start().await?;
    stub.script([Reply::text("plain")]);
    let session = run.session().await?;
    run.turn(&session, "hi").await?;
    let list = calls_of(&mut run, &session).await?;
    assert!(list["recording"].is_null(), "{list}");
    assert_eq!(list["calls"], json!([]), "{list}");
    run.finish().await;
    Ok(())
}

async fn search(run: &mut LiveRun, params: Value) -> anyhow::Result<Value> {
    ok(run.client.session_search(params).await?)
}

/// Search finds what was typed and what the model said, one hit per session, and not tool results.
#[tokio::test(flavor = "multi_thread")]
async fn search_covers_typed_and_answered_text_but_not_tool_results() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("search-text", &stub).start().await?;
    let file = run.write_file("notes.txt", "the UNIQUEWORD42 lives here")?;
    let a = run.session().await?;
    stub.script([Reply::text("use pgbouncer in front")]);
    run.turn(&a, "tune the PostgreSQL pool").await?;
    let b = run.session().await?;
    stub.script([
        Reply::calls("r1", "Read", json!({ "file_path": file.to_string_lossy() })),
        Reply::text("nothing else"),
    ]);
    let turn = run.turn(&b, "read the notes").await?;
    assert!(
        turn.tool_results
            .iter()
            .any(|(name, content, _)| name == "Read" && content.contains("UNIQUEWORD42")),
        "{:?}",
        turn.tool_results
    );

    let found = search(&mut run, json!({ "query": "postgresql" })).await?;
    let hits = found["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{found}");
    assert_eq!(hits[0]["session_id"], a.as_str());
    assert_eq!(hits[0]["origin"], "user");
    assert!(hits[0]["snippet"].as_str().unwrap().contains("PostgreSQL"));
    let page = history(&mut run, &a).await?;
    let item = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["seq"] == hits[0]["seq"])
        .expect("the hit's seq in history");
    assert_eq!(item["origin"], "user", "{item}");
    assert!(item.to_string().contains("PostgreSQL"), "{item}");

    let found = search(&mut run, json!({ "query": "PGBOUNCER" })).await?;
    let hits = found["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{found}");
    assert_eq!(hits[0]["session_id"], a.as_str());
    assert_eq!(hits[0]["origin"], "assistant");

    let found = search(&mut run, json!({ "query": "UNIQUEWORD42" })).await?;
    assert_eq!(
        found["hits"],
        json!([]),
        "a tool result was searched: {found}"
    );
    run.finish().await;
    Ok(())
}

/// With more matching sessions than `limit`, the answer says it was cut short.
#[tokio::test(flavor = "multi_thread")]
async fn search_over_its_limit_reports_truncated() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("search-limit", &stub).start().await?;
    for i in 0..3 {
        let session = run.session().await?;
        stub.script([Reply::text(format!("ack {i}"))]);
        run.turn(&session, "the sharedneedle topic").await?;
    }
    let found = search(&mut run, json!({ "query": "sharedneedle", "limit": 1 })).await?;
    assert_eq!(found["hits"].as_array().unwrap().len(), 1, "{found}");
    assert_eq!(found["truncated"], true, "{found}");
    run.finish().await;
    Ok(())
}

/// A closed session is found by search and its recording is listed, without being brought back into memory.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_session_is_searched_and_listed_without_being_loaded() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("calls-cold", &stub).start().await?;
    stub.script([Reply::text("cold answer")]);
    let extra = recorded(&run)?;
    let session = run.session_with(extra).await?;
    run.turn(&session, "remember coldneedle").await?;
    until_calls(&mut run, &session, |r| !r.is_empty()).await?;
    assert_eq!(history(&mut run, &session).await?["active"], true);

    ok(run.client.session_close(&session).await?)?;
    assert_eq!(history(&mut run, &session).await?["active"], false);

    let found = search(&mut run, json!({ "query": "coldneedle" })).await?;
    assert_eq!(found["hits"][0]["session_id"], session.as_str(), "{found}");
    let list = calls_of(&mut run, &session).await?;
    assert_eq!(list["recording"]["name"], session.as_str(), "{list}");
    assert!(!rows(&list).is_empty(), "{list}");
    assert_eq!(history(&mut run, &session).await?["active"], false);
    run.finish().await;
    Ok(())
}

/// `daemon.ping` advertises both capabilities.
#[tokio::test(flavor = "multi_thread")]
async fn ping_advertises_the_calls_and_search_capabilities() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("calls-ping", &stub).start().await?;
    let ping = ok(run.client.call("daemon.ping", Value::Null).await?)?;
    let capabilities = ping["capabilities"].as_array().expect("capabilities");
    for name in [
        "session_calls",
        "session_search",
        "calls_digest",
        "calls_preview",
    ] {
        assert!(capabilities.iter().any(|c| c == name), "{ping}");
    }
    run.finish().await;
    Ok(())
}
