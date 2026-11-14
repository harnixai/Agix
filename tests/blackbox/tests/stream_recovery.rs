//! A model call that fails after its stream has started — the connection
//! drops, or the provider reports an overload inside the stream: the host is
//! told with `model_retry` and a second `step_start` for the same step, the
//! call is sent again, and the answer that finally stands is in the
//! transcript once, without what the broken attempt had drawn.

use std::time::Duration;

use blackbox::{Block, LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Reply, Watcher};
use serde_json::{json, Value};

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

async fn retried_turn(broken: Reply) -> anyhow::Result<(LiveRun, ProviderStub, Watcher)> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("stream-retry", &stub).start().await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    stub.script([broken, Reply::text("WHOLE-ANSWER-ONCE")]);
    run.turn(&session, "first").await?;
    stub.script([Reply::text("second answer")]);
    run.turn(&session, "second").await?;
    Ok((run, stub, watcher))
}

fn assert_recovered(stub: &ProviderStub, watcher: &Watcher, partial: &str) {
    let retries = watcher.events("model_retry");
    assert_eq!(retries.len(), 1, "{retries:?}");
    let retry = &retries[0]["event"];
    assert_eq!(retry["attempt"], 1);
    assert_eq!(retry["delay_ms"], 2000);
    assert!(retry["reason"].as_str().is_some_and(|r| !r.is_empty()));

    let step = &retry["step"];
    let first_turn: Vec<Value> = watcher
        .frames()
        .into_iter()
        .take_while(|f| f["event"]["kind"] != "turn_complete")
        .collect();
    let starts = first_turn
        .iter()
        .filter(|f| f["event"]["kind"] == "step_start" && &f["event"]["step"] == step)
        .count();
    assert_eq!(starts, 2, "the step starts over after the retry");

    let streamed: String = first_turn
        .iter()
        .filter(|f| f["event"]["kind"] == "text_delta")
        .filter_map(|f| f["event"]["text"].as_str().map(String::from))
        .collect();
    assert!(
        streamed.contains(partial),
        "precondition: the host drew the broken attempt: {streamed:?}"
    );

    let last = stub.calls().pop().expect("a request");
    let history = last.messages_text();
    assert_eq!(history.matches("WHOLE-ANSWER-ONCE").count(), 1, "{history}");
    assert!(
        !history.contains(partial),
        "the broken attempt reached the transcript: {history}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_that_drops_after_it_started_is_sent_again() -> anyhow::Result<()> {
    let broken =
        Reply::cut_after_first_delta(Reply::text(format!("PARTIAL-ONE {}", "x".repeat(200))));
    let (run, stub, watcher) = retried_turn(broken).await?;
    assert_recovered(&stub, &watcher, "PARTIAL");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_error_event_inside_the_stream_is_sent_again() -> anyhow::Result<()> {
    let broken = Reply::errored(
        vec![Block::text("PARTIAL-TWO and then")],
        "overloaded_error",
        "busy",
    );
    let (run, stub, watcher) = retried_turn(broken).await?;
    assert_recovered(&stub, &watcher, "PARTIAL-TWO");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_waiting_to_resend_is_ended_by_an_interrupt() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("stream-retry-interrupt", &stub).start().await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    stub.script([
        Reply::errored(vec![Block::text("PARTIAL")], "overloaded_error", "busy"),
        Reply::text("never asked for"),
    ]);
    let turn = run.spawn_turn(&session, "go", json!({})).await?;
    watcher
        .wait_for("a model_retry", Duration::from_secs(20), |e: &Value| {
            e["kind"] == "model_retry"
        })
        .await?;
    let resp = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    assert!(resp.error.is_none(), "{:?}", resp.error);
    tokio::time::timeout(Duration::from_secs(10), turn).await???;
    assert_eq!(
        stub.call_count(),
        1,
        "the call was sent again after the interrupt"
    );
    run.finish().await;
    Ok(())
}
