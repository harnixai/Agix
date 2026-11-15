//! An MCP server asking the user something in the middle of a tool call,
//! against a scripted model and the toy server: the question reaches the host
//! as a `prompt` frame of `prompt_type:"mcp_elicitation"`, and what the host
//! answers — or that nobody did — reaches the server. Both ways a server asks
//! are covered: a 2026-07-28 server's `input_required` round and an older
//! server's own `elicitation/create` (`--max-protocol 2025-11-25`).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use blackbox::{Answer, LiveConfig, LiveRun, LiveRunBuilder, McpToy, ProviderStub, Reply, Watcher};
use serde_json::{json, Value};

const OLD_SERVER: &[&str] = &["--max-protocol", "2025-11-25"];

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

/// A run whose `demo` server is the toy started with `toy_args`, recording
/// into `calls`, its tools allowed by rule.
fn toy_run(
    name: &str,
    stub: &ProviderStub,
    calls: &Path,
    toy_args: &[&str],
    settings: Value,
) -> anyhow::Result<LiveRunBuilder> {
    let mut args = vec!["--calls", calls.to_str().unwrap_or_default()];
    args.extend_from_slice(toy_args);
    let mut merged = json!({
        "mcp_servers": { "demo": McpToy::stdio_config(&args)? },
        "permission_rules": [
            { "tool": "mcp__demo", "action": "allow" },
            { "tool": "Agent", "action": "allow" }
        ],
    });
    if let (Some(m), Some(extra)) = (merged.as_object_mut(), settings.as_object()) {
        m.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    Ok(stubbed(name, stub).settings(merged))
}

async fn asking_session(run: &mut LiveRun) -> anyhow::Result<String> {
    run.session_with(json!({
        "options": { "prompt_types": ["permission", "mcp_elicitation"] }
    }))
    .await
}

/// What the server was answered, in order.
fn answers(calls: &Path) -> Vec<Value> {
    McpToy::calls(calls)
        .into_iter()
        .filter_map(|(_, what)| serde_json::from_str(what.strip_prefix("ask ")?).ok())
        .collect()
}

fn is_mcp_prompt(event: &Value) -> bool {
    event["kind"] == "prompt" && event["prompt_type"] == "mcp_elicitation"
}

fn mcp_prompts(host: &Watcher) -> Vec<Value> {
    host.frames()
        .into_iter()
        .filter(|f| is_mcp_prompt(&f["event"]))
        .collect()
}

fn answering(answer: Value) -> impl Fn(&Value) -> Option<Value> + Send + Sync + 'static {
    move |event| (event["prompt_type"] == "mcp_elicitation").then(|| answer.clone())
}

fn blue() -> Value {
    Answer::accept(Some(json!({ "color": "blue" })))
}

fn result_of<'a>(
    events: &'a rpc_client::TurnEvents,
    tool: &str,
) -> Option<&'a (String, String, bool)> {
    events.tool_results.iter().find(|(name, _, _)| name == tool)
}

/// `rounds`: how many times the server saw the call — two when it asked in
/// an `input_required` round, one when it sent its own request.
async fn a_form_question_is_answered(
    name: &str,
    toy_args: &[&str],
    rounds: usize,
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run(name, &stub, &calls, toy_args, json!({}))?
        .start()
        .await?;
    let session = asking_session(&mut run).await?;
    let host = run.watch_with(&session, answering(blue())).await?;
    stub.script([
        Reply::calls("a1", "mcp__demo__ask", json!({ "mode": "form" })),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "ask me").await?;

    let asked = mcp_prompts(&host);
    assert_eq!(asked.len(), 1, "{asked:?}");
    let event = &asked[0]["event"];
    assert_eq!(event["mode"], "form", "{event}");
    assert_eq!(event["server"], "demo", "{event}");
    assert_eq!(event["message"], "Pick a color", "{event}");
    assert_eq!(event["tool_use_id"], "a1", "{event}");
    assert_eq!(
        event["requested_schema"]["properties"]["color"]["enum"],
        json!(["red", "blue"]),
        "the server's schema as it sent it: {event}"
    );
    assert_eq!(
        answers(&calls),
        [json!({ "action": "accept", "content": { "color": "blue" } })]
    );
    let calls_seen = McpToy::calls(&calls);
    assert_eq!(
        calls_seen.iter().filter(|(_, what)| what == "ask").count(),
        rounds,
        "{calls_seen:?}"
    );
    let result = result_of(&events, "mcp__demo__ask").expect("the call's result");
    assert!(
        !result.2 && result.1.contains("answer:") && result.1.contains("blue"),
        "{result:?}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_form_question_from_a_2026_server_reaches_the_host_and_its_answer_reaches_the_server(
) -> anyhow::Result<()> {
    a_form_question_is_answered("ask-2026", &[], 2).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_from_a_2025_server_sent_as_a_request_is_answered_the_same_way(
) -> anyhow::Result<()> {
    a_form_question_is_answered("ask-2025", OLD_SERVER, 1).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_url_question_shows_the_url_and_its_host_and_never_carries_content() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("ask-url", &stub, &calls, &[], json!({}))?
        .start()
        .await?;
    let session = asking_session(&mut run).await?;
    let host = run
        .watch_with(&session, answering(Answer::accept(None)))
        .await?;
    stub.script([
        Reply::calls("u1", "mcp__demo__ask", json!({ "mode": "url" })),
        Reply::text("done"),
    ]);
    run.turn(&session, "sign me in").await?;

    let asked = mcp_prompts(&host);
    assert_eq!(asked.len(), 1, "{asked:?}");
    let event = &asked[0]["event"];
    assert_eq!(event["mode"], "url", "{event}");
    assert_eq!(
        event["url"], "https://auth.example.com/toy/sign-in",
        "{event}"
    );
    assert_eq!(event["url_host"], "auth.example.com", "{event}");
    assert!(event.get("requested_schema").is_none(), "{event}");
    assert_eq!(answers(&calls), [json!({ "action": "accept" })]);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declined_question_tells_the_server_decline_and_an_unanswered_one_cancel(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("ask-no", &stub, &calls, &[], json!({}))?
        .options(|o| o.prompt_timeout(Duration::from_secs(2)))
        .start()
        .await?;
    let session = asking_session(&mut run).await?;
    let seen = Arc::new(AtomicUsize::new(0));
    let first_only = seen.clone();
    let host = run
        .watch_with(&session, move |event| {
            let first = event["prompt_type"] == "mcp_elicitation"
                && first_only.fetch_add(1, Ordering::SeqCst) == 0;
            first.then(|| Answer::mcp("decline"))
        })
        .await?;
    stub.script([
        Reply::calls("n1", "mcp__demo__ask", json!({ "mode": "form" })),
        Reply::calls("n2", "mcp__demo__ask", json!({ "mode": "form" })),
        Reply::text("done"),
    ]);
    run.turn(&session, "ask me twice").await?;

    assert_eq!(
        answers(&calls),
        [
            json!({ "action": "decline" }),
            json!({ "action": "cancel" })
        ]
    );
    let second = mcp_prompts(&host)
        .get(1)
        .map(|f| f["event"]["prompt_id"].clone())
        .expect("the second question went out");
    assert!(
        host.events("prompt_resolved").iter().any(|f| {
            f["event"]["prompt_id"] == second
                && f["event"]["resolution"] == "withdrawn"
                && f["event"]["reason"] == "timed_out"
        }),
        "{:?}",
        host.events("prompt_resolved")
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_breaks_the_servers_schema_is_refused_and_can_be_given_again(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("ask-schema", &stub, &calls, &[], json!({}))?
        .options(|o| o.prompt_timeout(Duration::from_secs(60)))
        .start()
        .await?;
    let session = asking_session(&mut run).await?;
    let host = run.watch_with(&session, |_| None).await?;
    stub.script([
        Reply::calls("s1", "mcp__demo__ask", json!({ "mode": "form" })),
        Reply::text("done"),
    ]);
    let turn = run.spawn_turn(&session, "ask me", json!({})).await?;
    let frame = host
        .wait_for("the question", Duration::from_secs(30), is_mcp_prompt)
        .await?;
    let prompt_id = frame["event"]["prompt_id"].clone();
    let respond = |answer: Value| {
        json!({
            "session_id": session,
            "prompt_id": prompt_id,
            "prompt_type": "mcp_elicitation",
            "answer": answer,
        })
    };

    let refused = run
        .client
        .call(
            "session.respondToPrompt",
            respond(Answer::accept(Some(json!({ "color": "green" })))),
        )
        .await?;
    let error = refused
        .error
        .expect("an answer outside the schema is refused");
    assert_eq!(error.code, -32602, "{error:?}");
    assert!(
        error
            .data
            .as_ref()
            .is_some_and(|d| d["schema_errors"].as_array().is_some_and(|e| !e.is_empty())),
        "{error:?}"
    );
    let mut other = run.daemon.connect().await?;
    let pending = other
        .session_subscribe(&session)
        .await?
        .result
        .unwrap_or_default();
    assert!(
        pending["pending_prompts"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|f| f["params"]["event"]["prompt_id"] == prompt_id),
        "the question is still open: {pending}"
    );

    let taken = run
        .client
        .call("session.respondToPrompt", respond(blue()))
        .await?;
    assert!(taken.error.is_none(), "{taken:?}");
    tokio::time::timeout(Duration::from_secs(30), turn).await???;
    assert_eq!(
        answers(&calls),
        [json!({ "action": "accept", "content": { "color": "blue" } })]
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_that_did_not_ask_for_mcp_questions_gets_none_and_the_server_gets_cancel(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("ask-unlisted", &stub, &calls, &[], json!({}))?
        .start()
        .await?;
    let session = run.session().await?;
    let host = run.watch_with(&session, answering(blue())).await?;
    stub.script([
        Reply::calls("c1", "mcp__demo__ask", json!({ "mode": "form" })),
        Reply::text("done"),
    ]);
    run.turn(&session, "ask me").await?;
    assert!(mcp_prompts(&host).is_empty(), "{:?}", host.frames());
    assert_eq!(answers(&calls), [json!({ "action": "cancel" })]);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_turn_withdraws_the_question() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("ask-interrupt", &stub, &calls, &[], json!({}))?
        .options(|o| o.prompt_timeout(Duration::from_secs(60)))
        .start()
        .await?;
    let session = asking_session(&mut run).await?;
    let host = run.watch_with(&session, |_| None).await?;
    stub.script([
        Reply::calls("i1", "mcp__demo__ask", json!({ "mode": "form" })),
        Reply::text("done"),
    ]);
    let turn = run.spawn_turn(&session, "ask me", json!({})).await?;
    let frame = host
        .wait_for("the question", Duration::from_secs(30), is_mcp_prompt)
        .await?;
    let prompt_id = frame["event"]["prompt_id"].clone();
    let interrupted = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    assert_eq!(
        interrupted.result.unwrap_or_default()["interrupted"],
        json!(true)
    );
    let events = tokio::time::timeout(Duration::from_secs(30), turn).await???;
    let withdrawn = host
        .wait_for("the withdrawal", Duration::from_secs(10), |e| {
            e["kind"] == "prompt_resolved" && e["prompt_id"] == prompt_id
        })
        .await?;
    assert_eq!(withdrawn["event"]["resolution"], "withdrawn", "{withdrawn}");
    assert_eq!(withdrawn["event"]["reason"], "cancelled", "{withdrawn}");
    assert!(
        result_of(&events, "mcp__demo__ask").is_none_or(|(_, text, _)| !text.contains("answer:")),
        "{:?}",
        events.tool_results
    );
    assert!(answers(&calls).is_empty(), "{:?}", McpToy::calls(&calls));
    run.finish_allowing_refusals().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn two_questions_in_one_round_are_asked_one_after_the_other() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("ask-twice", &stub, &calls, &[], json!({}))?
        .start()
        .await?;
    let session = asking_session(&mut run).await?;
    let host = run.watch_with(&session, answering(blue())).await?;
    stub.script([
        Reply::calls("t1", "mcp__demo__ask_twice", json!({})),
        Reply::text("done"),
    ]);
    run.turn(&session, "ask me two things").await?;

    let frames = host.frames();
    let at = |wanted: &dyn Fn(&Value) -> bool| frames.iter().position(|f| wanted(&f["event"]));
    let asked: Vec<usize> = frames
        .iter()
        .enumerate()
        .filter(|(_, f)| is_mcp_prompt(&f["event"]))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(asked.len(), 2, "{frames:?}");
    let first_id = frames[asked[0]]["event"]["prompt_id"].clone();
    let first_resolved = at(&|e| e["kind"] == "prompt_resolved" && e["prompt_id"] == first_id)
        .expect("the first question ended");
    assert!(
        first_resolved < asked[1],
        "the second question came only after the first was answered: {frames:?}"
    );
    assert_eq!(
        answers(&calls),
        [
            json!({ "action": "accept", "content": { "color": "blue" } }),
            json!({ "action": "accept", "content": { "color": "blue" } })
        ]
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agents_mcp_question_is_asked_on_the_parent_session() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("ask-sub", &stub, &calls, &[], json!({}))?
        .start()
        .await?;
    run.write_file(
        ".agix/agents/asker.md",
        "---\ndescription: Asks through the demo server\nmcp_servers: [demo]\n---\nASKER.\n",
    )?;
    let session = asking_session(&mut run).await?;
    let host = run.watch_with(&session, answering(blue())).await?;
    stub.script([
        Reply::calls(
            "g1",
            "Agent",
            json!({ "description": "ask", "prompt": "Ask the user.", "subagent_type": "asker" }),
        ),
        Reply::calls("s1", "mcp__demo__ask", json!({ "mode": "form" })),
        Reply::text("sub done"),
        Reply::text("parent done"),
    ]);
    run.turn(&session, "delegate the question").await?;

    let asked = mcp_prompts(&host);
    assert_eq!(asked.len(), 1, "{:?}", host.frames());
    let message = asked[0]["event"]["message"].as_str().unwrap_or_default();
    assert!(
        message.starts_with("[sub-agent ") && message.ends_with("Pick a color"),
        "{message}"
    );
    assert_eq!(
        answers(&calls),
        [json!({ "action": "accept", "content": { "color": "blue" } })]
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_elicitation_hook_can_refuse_before_the_host_is_asked() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let results = scratch.path().join("results.jsonl");
    let refuse = "cat >/dev/null; echo '{\"decision\": \"block\", \"reason\": \"not here\"}'";
    let record = format!(
        "cat >> '{}'; echo >> '{}'",
        results.display(),
        results.display()
    );
    let hooks = json!({
        "hooks_config": {
            "Elicitation": [{ "type": "command", "command": refuse }],
            "ElicitationResult": [{ "type": "command", "command": record }]
        }
    });
    let mut run = toy_run("ask-hooked", &stub, &calls, &[], hooks)?
        .start()
        .await?;
    let session = asking_session(&mut run).await?;
    let host = run.watch_with(&session, answering(blue())).await?;
    stub.script([
        Reply::calls("h1", "mcp__demo__ask", json!({ "mode": "form" })),
        Reply::text("done"),
    ]);
    run.turn(&session, "ask me").await?;

    assert!(mcp_prompts(&host).is_empty(), "{:?}", host.frames());
    assert_eq!(answers(&calls), [json!({ "action": "decline" })]);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut recorded = String::new();
    while recorded.trim().is_empty() && std::time::Instant::now() < deadline {
        recorded = std::fs::read_to_string(&results).unwrap_or_default();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let payload: Value = serde_json::from_str(recorded.lines().next().unwrap_or("null"))?;
    assert_eq!(
        payload["hook_event_name"], "ElicitationResult",
        "{recorded}"
    );
    assert_eq!(payload["tool_name"], "mcp_elicitation_demo", "{recorded}");
    assert_eq!(payload["tool_input"]["action"], "decline", "{recorded}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_2025_request_with_two_calls_in_flight_is_cancelled_not_guessed() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("ask-two", &stub, &calls, OLD_SERVER, json!({}))?
        .start()
        .await?;
    let first = asking_session(&mut run).await?;
    let second = asking_session(&mut run).await?;
    let hosts = [
        run.watch_with(&first, answering(blue())).await?,
        run.watch_with(&second, answering(blue())).await?,
    ];
    let overlapping = json!({ "mode": "form", "wait_ms": 1500, "hold_ms": 3000 });
    stub.script([
        Reply::calls("p1", "mcp__demo__ask", overlapping.clone()),
        Reply::calls("p2", "mcp__demo__ask", overlapping),
        Reply::text("done"),
        Reply::text("done"),
    ]);
    let turns = [
        run.spawn_turn(&first, "ask", json!({})).await?,
        run.spawn_turn(&second, "ask", json!({})).await?,
    ];
    for turn in turns {
        tokio::time::timeout(Duration::from_secs(60), turn).await???;
    }
    for host in &hosts {
        assert!(mcp_prompts(host).is_empty(), "{:?}", host.frames());
    }
    assert_eq!(
        answers(&calls),
        [json!({ "action": "cancel" }), json!({ "action": "cancel" })]
    );
    run.finish().await;
    Ok(())
}

fn answered_with(action: &str) -> Value {
    json!({ "action": action })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_2026_round_of_ten_questions_is_asked_eight_deep_and_the_rest_cancelled(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("ask-round", &stub, &calls, &[], json!({}))?
        .start()
        .await?;
    let session = asking_session(&mut run).await?;
    let host = run.watch_with(&session, answering(blue())).await?;
    stub.script([
        Reply::calls("f1", "mcp__demo__ask_flood", json!({ "count": 10 })),
        Reply::text("done"),
    ]);
    run.turn(&session, "ask me ten things").await?;

    assert_eq!(mcp_prompts(&host).len(), 8, "{:?}", host.frames());
    let got = answers(&calls);
    assert_eq!(got.len(), 10, "{got:?}");
    assert_eq!(
        got[..8].iter().filter(|a| a["action"] == "accept").count(),
        8
    );
    assert_eq!(got[8..], [answered_with("cancel"), answered_with("cancel")]);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_2025_server_asking_six_at_once_has_four_open_and_two_cancelled() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("ask-flood", &stub, &calls, OLD_SERVER, json!({}))?
        .options(|o| o.prompt_timeout(Duration::from_secs(2)))
        .start()
        .await?;
    let session = asking_session(&mut run).await?;
    let host = run.watch_with(&session, |_| None).await?;
    stub.script([
        Reply::calls("f1", "mcp__demo__ask_flood", json!({ "count": 6 })),
        Reply::text("done"),
    ]);
    run.turn(&session, "ask me six things").await?;

    assert_eq!(mcp_prompts(&host).len(), 4, "{:?}", host.frames());
    assert_eq!(answers(&calls), vec![answered_with("cancel"); 6]);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_2025_server_asking_twelve_in_turn_is_answered_ten_and_the_result_says_so(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("ask-in-turn", &stub, &calls, OLD_SERVER, json!({}))?
        .start()
        .await?;
    let session = asking_session(&mut run).await?;
    let host = run.watch_with(&session, answering(blue())).await?;
    stub.script([
        Reply::calls(
            "f1",
            "mcp__demo__ask_flood",
            json!({ "count": 12, "one_by_one": true }),
        ),
        Reply::text("done"),
    ]);
    run.turn(&session, "ask me twelve things").await?;

    assert_eq!(mcp_prompts(&host).len(), 10, "{:?}", host.frames());
    let got = answers(&calls);
    assert_eq!(
        got[10..],
        [answered_with("cancel"), answered_with("cancel")]
    );
    let sent = stub
        .calls()
        .last()
        .map(|c| c.body.to_string())
        .unwrap_or_default();
    assert!(
        sent.contains("at most 10"),
        "the result does not say questions were cut: {sent}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_over_4_kib_is_cut_and_marked_limited() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("ask-long", &stub, &calls, &[], json!({}))?
        .start()
        .await?;
    let session = asking_session(&mut run).await?;
    let host = run.watch_with(&session, answering(blue())).await?;
    stub.script([
        Reply::calls("l1", "mcp__demo__ask", json!({ "mode": "long" })),
        Reply::text("done"),
    ]);
    run.turn(&session, "ask me").await?;

    let asked = mcp_prompts(&host);
    assert_eq!(asked.len(), 1, "{asked:?}");
    let event = &asked[0]["event"];
    assert!(
        event["message"].as_str().unwrap_or_default().len() <= 4096,
        "{event}"
    );
    assert_eq!(event["limited"], json!(true), "{event}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_form_of_forty_fields_is_declined_without_asking_anyone() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = toy_run("ask-wide", &stub, &calls, &[], json!({}))?
        .start()
        .await?;
    let session = asking_session(&mut run).await?;
    let host = run.watch_with(&session, answering(blue())).await?;
    stub.script([
        Reply::calls("w1", "mcp__demo__ask", json!({ "mode": "wide" })),
        Reply::text("done"),
    ]);
    run.turn(&session, "ask me").await?;

    assert!(mcp_prompts(&host).is_empty(), "{:?}", host.frames());
    assert_eq!(answers(&calls), [answered_with("decline")]);
    run.finish().await;
    Ok(())
}
