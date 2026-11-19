//! A session's limits, against a scripted model: the per-connection cap on
//! requests in flight, the pool's capacity and idle evictions, a subscriber
//! too slow to keep up (its session subscription dropped, its daemon events
//! lagged), retries on an overloaded endpoint, the endings the engine
//! recovers from (`max_tokens`, `pause_turn`), thinking that streams apart
//! from the answer and goes back with its signature, a stream cut short, and
//! an interrupt while the model has not answered at all or a compaction
//! summary is still being written, and a closed session whose engine has not
//! stopped yet being resumed.
//!
//! The live tier cannot ask for any of these on demand; here the model's side
//! is held fixed so the product's side is all that varies.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use blackbox::{
    Block, LiveConfig, LiveRun, LiveRunBuilder, Mode, ProviderStub, Purpose, Reply, SeenRequest,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

const OVERLOADED: &str =
    "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}";

/// Every `daemon.event` a second connection is pushed, in order.
async fn daemon_events(
    run: &LiveRun,
) -> anyhow::Result<(Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>)> {
    let mut subscriber = run.daemon.connect().await?;
    let subscribed = subscriber
        .call("daemon.subscribeEvents", Value::Null)
        .await?;
    anyhow::ensure!(subscribed.error.is_none(), "{subscribed:?}");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let into = seen.clone();
    let reader = tokio::spawn(async move {
        while let Ok(Some(frame)) = subscriber.next_frame().await {
            if frame["method"] == "daemon.event" {
                into.lock().unwrap().push(frame["params"].clone());
            }
        }
    });
    Ok((seen, reader))
}

async fn wait_for_event(
    seen: &Mutex<Vec<Value>>,
    timeout: Duration,
    wanted: impl Fn(&Value) -> bool,
) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(e) = seen.lock().unwrap().iter().find(|e| wanted(e)) {
            return Some(e.clone());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// A connection spoken to line by line, so a case can have many requests on
/// it at once, or stop reading it altogether.
struct RawConnection {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write: tokio::net::unix::OwnedWriteHalf,
}

impl RawConnection {
    async fn open(run: &LiveRun) -> anyhow::Result<Self> {
        let (read, write) = tokio::net::UnixStream::connect(run.daemon.socket())
            .await?
            .into_split();
        Ok(Self {
            lines: BufReader::new(read).lines(),
            write,
        })
    }

    async fn send(&mut self, requests: &[Value]) -> anyhow::Result<()> {
        let mut batch = String::new();
        for request in requests {
            batch.push_str(&request.to_string());
            batch.push('\n');
        }
        self.write.write_all(batch.as_bytes()).await?;
        Ok(())
    }

    /// Frames until `done` says stop, or nothing arrives for `quiet`.
    async fn read_until(
        &mut self,
        quiet: Duration,
        mut done: impl FnMut(&[Value]) -> bool,
    ) -> anyhow::Result<Vec<Value>> {
        let mut frames = Vec::new();
        while !done(&frames) {
            match tokio::time::timeout(quiet, self.lines.next_line()).await {
                Ok(Ok(Some(line))) => frames.push(serde_json::from_str(&line)?),
                Ok(Ok(None)) | Err(_) => break,
                Ok(Err(e)) => return Err(e.into()),
            }
        }
        Ok(frames)
    }
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn response(frames: &[Value], id: i64) -> Option<&Value> {
    frames
        .iter()
        .find(|f| f["id"] == id && (f.get("result").is_some() || f.get("error").is_some()))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_past_sixty_four_requests_in_flight_is_refused_not_queued(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("in-flight", &stub).start().await?;
    let session = run.session().await?;
    stub.script([Reply::delayed(Duration::from_secs(4), Reply::text("first"))]);
    let first = run.spawn_turn(&session, "hold", json!({})).await?;
    run.wait_turn_state(&session, "running", Duration::from_secs(10))
        .await?;

    let mut raw = RawConnection::open(&run).await?;
    let mut requests: Vec<Value> = (1..=64)
        .map(|id| {
            request(
                id,
                "session.run_turn",
                json!({
                    "session_id": session,
                    "message": format!("queued {id}"),
                    "on_busy": "queue",
                }),
            )
        })
        .collect();
    requests.push(request(65, "daemon.ping", Value::Null));
    raw.send(&requests).await?;
    let frames = raw
        .read_until(Duration::from_secs(3), |f| response(f, 65).is_some())
        .await?;
    let refused = response(&frames, 65).expect("the 65th request was answered");
    assert_eq!(refused["error"]["code"], -32017, "{refused}");
    assert!(
        frames
            .iter()
            .filter(|f| f["id"].as_i64().is_some_and(|id| id <= 64))
            .all(|f| f.get("result").is_none() && f.get("error").is_none()),
        "a queued turn finished while the first was still running"
    );

    first.await??;
    let frames = raw
        .read_until(Duration::from_secs(30), |f| {
            (1..=64).all(|id| response(f, id).is_some())
        })
        .await?;
    for id in 1..=64 {
        let answer = response(&frames, id).unwrap_or(&Value::Null);
        assert!(answer.get("result").is_some(), "queued turn {id}: {answer}");
    }
    raw.send(&[request(66, "daemon.ping", Value::Null)]).await?;
    let frames = raw
        .read_until(Duration::from_secs(5), |f| response(f, 66).is_some())
        .await?;
    let pong = response(&frames, 66).expect("a ping once the queue drained");
    assert!(pong.get("result").is_some(), "{pong}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_pool_evicts_its_least_recent_idle_session_which_comes_back_on_use(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("capacity", &stub)
        .options(|o| o.session_cap(2))
        .start()
        .await?;
    let (seen, _reader) = daemon_events(&run).await?;
    let oldest = run.session().await?;
    stub.script([Reply::text("OLDEST-ANSWER")]);
    run.turn(&oldest, "OLDEST-QUESTION").await?;
    let middle = run.session().await?;
    stub.script([Reply::text("middle")]);
    run.turn(&middle, "middle").await?;
    let newest = run.session().await?;

    let evicted = wait_for_event(&seen, Duration::from_secs(10), |e| {
        e["kind"] == "session_evicted"
    })
    .await
    .expect("a session_evicted event");
    assert_eq!(evicted["session_id"], oldest.as_str(), "{evicted}");
    assert_eq!(evicted["reason"], "capacity");
    let got = run
        .client
        .session_get(&oldest)
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(got["status"], "inactive", "{got}");

    stub.script([Reply::text("back")]);
    run.turn(&oldest, "again").await?;
    let last = stub.calls().last().cloned().expect("a call");
    assert!(
        last.messages_contain("OLDEST-QUESTION") && last.messages_contain("OLDEST-ANSWER"),
        "the evicted session came back without its history"
    );
    assert!(
        wait_for_event(&seen, Duration::from_secs(5), |e| {
            e["kind"] == "session_resumed" && e["session_id"] == oldest.as_str()
        })
        .await
        .is_some(),
        "no session_resumed for the evicted session"
    );
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "session_evicted" || e["session_id"] != newest.as_str()),
        "the newest session was evicted"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn an_idle_session_is_evicted_and_resumes_with_its_history() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("idle", &stub)
        .mode(Mode::Spawned)
        .options(|o| o.arg("--session-idle-timeout").arg("1"))
        .start()
        .await?;
    let (seen, _reader) = daemon_events(&run).await?;
    let session = run.session().await?;
    stub.script([Reply::text("IDLE-ANSWER")]);
    run.turn(&session, "IDLE-QUESTION").await?;
    let evicted = wait_for_event(&seen, Duration::from_secs(70), |e| {
        e["kind"] == "session_evicted" && e["session_id"] == session.as_str()
    })
    .await
    .expect("the idle session was never evicted");
    assert_eq!(evicted["reason"], "idle");
    stub.script([Reply::text("back")]);
    run.turn(&session, "again").await?;
    let last = stub.calls().last().cloned().expect("a call");
    assert!(last.messages_contain("IDLE-QUESTION") && last.messages_contain("IDLE-ANSWER"));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reader_that_stops_reading_loses_its_subscription_and_is_told_events_lagged(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("slow-reader", &stub).start().await?;
    let session = run.session().await?;

    let mut slow = RawConnection::open(&run).await?;
    slow.send(&[
        request(1, "daemon.subscribeEvents", Value::Null),
        request(2, "session.subscribe", json!({ "session_id": session })),
    ])
    .await?;
    let acks = slow
        .read_until(Duration::from_secs(5), |f| {
            response(f, 1).is_some() && response(f, 2).is_some()
        })
        .await?;
    assert!(response(&acks, 2).is_some_and(|r| r.get("result").is_some()));

    let blocks: Vec<Block> = (0..3000)
        .map(|i| Block::text(format!("chunk {i:04} ")))
        .collect();
    stub.script([Reply::Blocks(blocks)]);
    run.turn(&session, "flood").await?;
    for _ in 0..1500 {
        run.turn(&session, "/help").await?;
    }

    let frames = slow.read_until(Duration::from_secs(2), |_| false).await?;
    let session_frame = |f: &Value| f["method"] == "session.event";
    let dropped_at = frames
        .iter()
        .position(|f| session_frame(f) && f["params"]["event"]["kind"] == "subscription_dropped")
        .expect("a subscription_dropped frame");
    assert_eq!(
        frames[dropped_at]["params"]["event"]["reason"], "overflow",
        "{}",
        frames[dropped_at]
    );
    assert!(
        frames[dropped_at + 1..].iter().all(|f| !session_frame(f)),
        "session frames kept coming after the drop"
    );
    let lagged = frames
        .iter()
        .find(|f| f["method"] == "daemon.event" && f["params"]["kind"] == "events_lagged")
        .expect("an events_lagged daemon event");
    assert!(
        lagged["params"]["missed"].as_u64().unwrap_or(0) > 0,
        "{lagged}"
    );

    slow.send(&[request(
        3,
        "session.subscribe",
        json!({ "session_id": session }),
    )])
    .await?;
    let again = slow
        .read_until(Duration::from_secs(5), |f| response(f, 3).is_some())
        .await?;
    assert!(response(&again, 3).is_some_and(|r| r.get("result").is_some()));
    stub.script([Reply::text("AFTER-RESUBSCRIBE")]);
    run.turn(&session, "after").await?;
    let after = slow.read_until(Duration::from_secs(2), |_| false).await?;
    assert!(
        after.iter().any(|f| session_frame(f)
            && f["params"]["event"]["kind"] == "text_delta"
            && f["params"]["event"]["text"]
                .as_str()
                .is_some_and(|t| !t.is_empty())),
        "the resubscribed connection saw nothing of the next turn"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn overloaded_answers_are_retried_with_backoff_and_a_bad_request_is_not() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("retry", &stub).start().await?;
    let session = run.session().await?;

    stub.inject_failures(2, Reply::status(529, OVERLOADED));
    stub.script([Reply::text("THIRD-TIME-LUCKY")]);
    let started = Instant::now();
    let events = run.turn(&session, "one").await?;
    let waited = started.elapsed();
    assert!(events.text.contains("THIRD-TIME-LUCKY"));
    assert_eq!(stub.call_count(), 3, "two failures, one success");
    assert!(
        waited >= Duration::from_millis(2_200),
        "retried without backing off: {waited:?}"
    );

    stub.inject_failures(
        1,
        Reply::status(
            400,
            "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"BAD-REQUEST-MARK\"}}",
        ),
    );
    stub.script([Reply::text("never asked")]);
    let failed = run
        .client
        .run_turn_with(json!({ "session_id": session, "message": "two" }))
        .await?;
    let error = failed.response.and_then(|r| r.error).expect("an error");
    assert_eq!(error.code, -32002, "{error:?}");
    assert_eq!(stub.call_count(), 4, "a 400 was retried");

    let events = run.turn(&session, "three").await?;
    assert!(events.text.contains("never asked"), "the session is usable");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_max_tokens_stop_is_retried_with_a_higher_limit_and_a_user_note() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("max-tokens", &stub).start().await?;
    let session = run.session().await?;
    stub.script([
        Reply::ended(vec![Block::text("cut one")], "max_tokens"),
        Reply::ended(vec![Block::text("cut two")], "max_tokens"),
        Reply::text("WHOLE-ANSWER"),
    ]);
    let events = run.turn(&session, "write a lot").await?;
    let calls = stub.calls();
    assert_eq!(calls.len(), 3, "{:?}", events.events);
    let limit = |i: usize| calls[i].body["max_tokens"].as_u64().unwrap_or(0);
    assert!(limit(0) < 64_000, "first call at {}", limit(0));
    assert!(limit(1) > limit(0), "first escalation at {}", limit(1));
    assert!(
        limit(1) <= 64_000,
        "held to the model's limit: {}",
        limit(1)
    );
    for i in [1, 2] {
        assert!(
            calls[i].messages_contain("cut off at the output token limit"),
            "call {i} carries the user's note that the reply was cut off"
        );
    }
    assert!(!calls[0].messages_contain("cut off at the output token limit"));
    assert!(events.text.contains("WHOLE-ANSWER"));
    let complete = events.turn_complete_event.clone().unwrap_or_default();
    assert_eq!(complete["stop_reason"], "end_turn", "{complete}");
    assert_eq!(complete["api_calls"], 3);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pause_turn_stop_is_continued_in_the_same_turn() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("pause-turn", &stub).start().await?;
    let session = run.session().await?;
    stub.script([
        Reply::ended(vec![Block::text("PAUSED-PART ")], "pause_turn"),
        Reply::text("RESUMED-PART"),
    ]);
    let events = run.turn(&session, "search for a while").await?;
    let calls = stub.calls();
    assert_eq!(calls.len(), 2, "the paused turn was not continued");
    assert!(
        calls[1].messages_contain("PAUSED-PART"),
        "the paused response went back"
    );
    assert!(events.text.contains("RESUMED-PART"));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn thinking_streams_apart_from_the_answer_and_goes_back_with_its_signature(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("thinking", &stub).start().await?;
    let session = run.session().await?;
    stub.script([
        Reply::Blocks(vec![
            Block::thinking("PONDERING-THE-FILE"),
            Block::tool("b1", "Bash", json!({ "command": "echo hi" })),
        ]),
        Reply::Blocks(vec![
            Block::thinking("SECOND-THOUGHT"),
            Block::text("ANSWER-TEXT"),
        ]),
    ]);
    let events = run.turn(&session, "think, then run it").await?;
    let thought: String = events
        .events
        .iter()
        .filter(|e| e["kind"] == "thinking_delta")
        .filter_map(|e| e["text"].as_str())
        .collect();
    assert!(
        thought.contains("PONDERING-THE-FILE") && thought.contains("SECOND-THOUGHT"),
        "{thought}"
    );
    assert!(!events.text.contains("PONDERING") && !events.text.contains("SECOND-THOUGHT"));
    assert!(events.text.contains("ANSWER-TEXT"));

    let second = &stub.calls()[1];
    let thinking_sent: Vec<&Value> = second.body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "assistant")
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter(|b| b["type"] == "thinking")
        .collect();
    assert_eq!(thinking_sent.len(), 1, "{}", second.body["messages"]);
    assert_eq!(thinking_sent[0]["thinking"], "PONDERING-THE-FILE");
    assert_eq!(thinking_sent[0]["signature"], "stub-signature");
    run.finish().await;
    Ok(())
}

/// A stream that breaks off after `message_start` and before any block.
fn cut_stream() -> Reply {
    Reply::cut(420, Reply::text(format!("NEVER-WHOLE {}", "x".repeat(400))))
}

async fn cut_turn(
    run: &mut LiveRun,
    stub: &ProviderStub,
    session: &str,
) -> anyhow::Result<rpc_client::TurnEvents> {
    stub.inject_failures(8, cut_stream());
    let events = tokio::time::timeout(
        Duration::from_secs(90),
        run.client
            .run_turn_with(json!({ "session_id": session, "message": "during" })),
    )
    .await??;
    stub.inject_failures(0, Reply::text("unused"));
    run.wait_turn_state(session, "idle", Duration::from_secs(10))
        .await?;
    Ok(events)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_cut_short_fails_the_turn() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("cut-fails", &stub).start().await?;
    let session = run.session().await?;
    let failed = cut_turn(&mut run, &stub, &session).await?;
    let error = failed.response.and_then(|r| r.error);
    assert!(
        error.as_ref().is_some_and(|e| e.code == -32002),
        "after {} cut streams: {error:?} {:?}",
        stub.call_count(),
        failed.events
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_carries_on_after_a_stream_cut_short() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("cut", &stub).start().await?;
    let session = run.session().await?;
    stub.script([Reply::text("WHOLE-BEFORE")]);
    run.turn(&session, "before").await?;
    let cut = cut_turn(&mut run, &stub, &session).await?;
    assert!(!cut.text.contains("NEVER-WHOLE"), "{}", cut.text);

    stub.script([Reply::text("AFTER-THE-CUT")]);
    let events = run.turn(&session, "after").await?;
    assert!(events.text.contains("AFTER-THE-CUT"), "{}", events.text);
    let last = stub.calls().last().cloned().expect("a call");
    assert!(
        last.messages_contain("WHOLE-BEFORE"),
        "history survived the cut"
    );
    let messages = last.body["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for m in &messages {
        let empty = match &m["content"] {
            Value::String(s) => s.is_empty(),
            Value::Array(blocks) => blocks.is_empty(),
            _ => true,
        };
        assert!(!empty, "an empty message went back to the model: {m}");
    }
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_ends_a_turn_whose_model_has_not_answered_yet() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("slow-model", &stub).start().await?;
    let session = run.session().await?;
    stub.script([Reply::delayed(
        Duration::from_secs(30),
        Reply::text("TOO-LATE"),
    )]);
    let handle = run
        .spawn_turn(&session, "take your time", json!({}))
        .await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while stub.call_count() == 0 {
        anyhow::ensure!(Instant::now() < deadline, "the model was never called");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let interrupted_at = Instant::now();
    let resp = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    assert_eq!(resp.result.unwrap_or_default()["interrupted"], json!(true));
    let events = tokio::time::timeout(Duration::from_secs(40), handle).await???;
    assert!(
        interrupted_at.elapsed() < Duration::from_secs(5),
        "the turn waited out the model: {:?}",
        interrupted_at.elapsed()
    );
    assert!(!events.text.contains("TOO-LATE"));
    run.finish().await;
    Ok(())
}

/// The turn outgrows the threshold and compacts with a model-written summary
/// that is slow to come; an interrupt then ends the turn without waiting for
/// it, and the conversation is left as it was.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_ends_a_turn_whose_compaction_summary_has_not_come_back() -> anyhow::Result<()>
{
    const SUMMARY: &str = "You are compacting an AI coding agent's conversation";
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("slow-summary", &stub)
        .settings(json!({ "compaction": { "threshold_tokens": 22000, "keep_recent": 2 } }))
        .start()
        .await?;
    let mut script = Vec::new();
    for i in 1..=4 {
        let body: String = (0..600)
            .map(|l| format!("part {i} line {l}: routine reading, all values nominal\n"))
            .collect();
        let path = run.write_file(&format!("part{i}.log"), &body)?;
        script.push(Reply::calls(
            &format!("p{i}"),
            "Read",
            json!({ "file_path": path.to_string_lossy() }),
        ));
    }
    script.push(Reply::text("DONE"));
    stub.script(script);
    stub.script_when(
        SUMMARY,
        [Reply::delayed(
            Duration::from_secs(60),
            Reply::text("SUMMARY-TOO-LATE"),
        )],
    );
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    let handle = run
        .spawn_turn(&session, "read the four parts", json!({}))
        .await?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !stub.calls().iter().any(|c| c.purpose() == Purpose::Compact) {
        anyhow::ensure!(
            Instant::now() < deadline,
            "the turn never asked for a summary"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let interrupted_at = Instant::now();
    let resp = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    assert_eq!(resp.result.unwrap_or_default()["interrupted"], json!(true));
    tokio::time::timeout(Duration::from_secs(70), handle).await???;
    assert!(
        interrupted_at.elapsed() < Duration::from_secs(5),
        "the turn waited out the summary: {:?}",
        interrupted_at.elapsed()
    );
    assert_eq!(watcher.events("compact"), Vec::<Value>::new());
    let history = run
        .client
        .call(
            "session.history",
            json!({ "session_id": session, "limit": 100 }),
        )
        .await?
        .result
        .unwrap_or_default()
        .to_string();
    assert!(
        history.contains("part 1 line 0:") && !history.contains("SUMMARY-TOO-LATE"),
        "the conversation was changed: {}",
        &history[..history.len().min(2000)]
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// A `SessionStart` hook that has not returned does not hold the engine once
/// the session is closed: the hook runs on in the background, the engine
/// stops, and the session resumes on a new one straight away.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_session_resumes_while_its_session_start_hook_still_runs() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let (started, release) = (
        scratch.path().join("started"),
        scratch.path().join("release"),
    );
    let hook = scratch.path().join("hold.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncat >/dev/null\nif [ ! -e '{started}' ]; then\n  touch '{started}'\n  \
             while [ ! -e '{release}' ]; do sleep 0.05; done\nfi\necho '{{}}'\n",
            started = started.display(),
            release = release.display(),
        ),
    )?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    let mut run = stubbed("stuck-engine", &stub)
        .settings(json!({
            "hooks_config": {
                "SessionStart": [{ "type": "command", "command": hook, "timeout": 120_000 }]
            }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !started.exists() {
        anyhow::ensure!(Instant::now() < deadline, "the SessionStart hook never ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let closed = run.client.session_close(&session).await?;
    anyhow::ensure!(closed.error.is_none(), "{closed:?}");
    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": session }))
        .await?;
    assert_eq!(
        resumed.result.as_ref().map(|r| r["status"].clone()),
        Some(json!("resumed")),
        "{resumed:?}"
    );
    std::fs::write(&release, "")?;
    run.finish().await;
    Ok(())
}

const SUMMARY: &str = "You are compacting an AI coding agent's conversation";

const TOO_LONG: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 212034 tokens > 200000 maximum"}}"#;

fn turns_with(stub: &ProviderStub, mark: &str, not: &[&str]) -> Vec<SeenRequest> {
    stub.calls()
        .into_iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .filter(|c| c.messages_contain(mark) && not.iter().all(|n| !c.messages_contain(n)))
        .collect()
}

fn executable(dir: &Path, name: &str, body: &str) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

async fn wait_until(what: &str, timeout: Duration, done: impl Fn() -> bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while !done() {
        anyhow::ensure!(Instant::now() < deadline, "{what} never happened");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

async fn interrupt(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    let resp = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    Ok(resp.result.unwrap_or_default())
}

async fn runtime_of(run: &mut LiveRun, session: &str, name: &str) -> anyhow::Result<Value> {
    let archive = run.world.root().join(format!("export-{name}"));
    let exported = run
        .client
        .session_export(session, &archive, Some("dir"), None)
        .await?;
    anyhow::ensure!(exported.error.is_none(), "{exported:?}");
    Ok(serde_json::from_str(&std::fs::read_to_string(
        archive.join("sessions").join(session).join("runtime.json"),
    )?)?)
}

/// `rounds` rounds of file reads, so a compaction has something to summarize.
async fn a_few_rounds(
    run: &mut LiveRun,
    stub: &ProviderStub,
    session: &str,
    rounds: usize,
) -> anyhow::Result<()> {
    let mut script = Vec::new();
    for i in 0..rounds {
        let body: String = (0..200)
            .map(|l| format!("file {i} line {l} of filler\n"))
            .collect();
        let path = run.write_file(&format!("f{i}.txt"), &body)?;
        script.push(Reply::calls(
            &format!("r{i}"),
            "Read",
            json!({ "file_path": path.to_string_lossy() }),
        ));
    }
    script.push(Reply::text("read them"));
    stub.script(script);
    run.turn(session, "read the files").await?;
    Ok(())
}

const WAIT_FOR_GO: &str = "touch waiting; while [ ! -e go ]; do sleep 0.05; done; echo released";

/// The first task of `kind` in `session.tasks`.
async fn task_of_kind(run: &mut LiveRun, session: &str, kind: &str) -> anyhow::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(task) = run
            .tasks(session)
            .await?
            .into_iter()
            .find(|t| t["kind"] == kind)
        {
            return Ok(task);
        }
        anyhow::ensure!(Instant::now() < deadline, "no {kind} task appeared");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A turn submitted while the engine is still between turns (here: re-running
/// an async-rewake hook) and interrupted before it starts is the turn that
/// gets interrupted — not the finished one before it, nor the background
/// command that one started.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_sent_between_turns_reaches_the_turn_it_was_sent_for() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let (go, stalled, release) = (
        scratch.path().join("go"),
        scratch.path().join("stalled"),
        scratch.path().join("release"),
    );
    let hook = executable(
        scratch.path(),
        "rewake.sh",
        &format!(
            "#!/bin/sh\ninput=$(cat)\necho \"$input\" >> '{log}'\ncase \"$input\" in\n  *WakeEvent*)\n    touch '{stalled}'\n    \
             while [ ! -e '{release}' ]; do sleep 0.05; done\n    echo '{{}}' ;;\n  *)\n    \
             echo '{{\"rewake\":true}}' ;;\nesac\n",
            stalled = stalled.display(),
            release = release.display(),
            log = scratch.path().join("stalled.log").display(),
        ),
    )?;
    let mut run = stubbed("rewake-interrupt", &stub)
        .settings(json!({
            "hooks_config": { "SessionStart": [{
                "type": "command", "command": hook, "async_rewake": true, "timeout": 120_000
            }], "TaskCreated": [{ "type": "command", "command": hook }] },
            "permissions": {"allow": ["Monitor"]}
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([
        Reply::Blocks(vec![
            blackbox::Block::tool(
                "m-quick",
                "Monitor",
                json!({
                    "command": format!(
                        "while [ ! -e '{}' ]; do sleep 0.05; done; echo finished",
                        go.display()
                    ),
                    "description": "quick"
                }),
            ),
            blackbox::Block::tool(
                "m-long",
                "Monitor",
                json!({ "command": "sleep 600", "description": "long" }),
            ),
        ]),
        Reply::text("t1 done"),
    ]);
    run.turn(&session, "start two monitors").await?;
    let task_of = |tasks: &[Value], description: &str| {
        tasks
            .iter()
            .find(|t| t.to_string().contains(description))
            .cloned()
    };
    let tasks = run.tasks(&session).await?;
    anyhow::ensure!(
        task_of(&tasks, "long").is_some() && task_of(&tasks, "quick").is_some(),
        "the monitors did not start: {tasks:?}"
    );
    let log = scratch.path().join("stalled.log");
    wait_until(
        "the quick monitor announced",
        Duration::from_secs(20),
        || {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .any(|l| l.contains("TaskCreated") && l.contains("quick"))
        },
    )
    .await?;
    std::fs::write(&go, "")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let tasks = run.tasks(&session).await?;
        if task_of(&tasks, "quick").is_some_and(|t| t["status"] != "running") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the quick monitor never ended");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    stub.script([Reply::text("T2-ANSWER")]);
    let second = run
        .spawn_turn(&session, "T2-MARK filler", json!({}))
        .await?;
    wait_until("the rewake hook stalling", Duration::from_secs(20), || {
        stalled.exists()
    })
    .await?;
    let (handle, mark) = if turns_with(&stub, "T2-MARK", &[]).is_empty() {
        (second, "T2-MARK")
    } else {
        second.await??;
        stub.script([Reply::text("T3-ANSWER")]);
        (
            run.spawn_turn(&session, "T3-MARK the turn to interrupt", json!({}))
                .await?,
            "T3-MARK",
        )
    };
    run.wait_turn_state(&session, "running", Duration::from_secs(10))
        .await?;
    let interrupted = interrupt(&mut run, &session).await?;
    assert_eq!(interrupted["interrupted"], json!(true), "{interrupted}");
    std::fs::write(&release, "")?;
    let events = handle.await??;

    let mut problems = Vec::new();
    let stop = events
        .turn_complete_event
        .as_ref()
        .map(|e| e["stop_reason"].clone())
        .unwrap_or_default();
    if !turns_with(&stub, mark, &[]).is_empty() && stop == "end_turn" {
        problems.push(format!(
            "the interrupted turn ({mark}) still reached the model and ended {stop}: {:?}",
            events.text
        ));
    }
    let long = task_of(&run.tasks(&session).await?, "long");
    if long.as_ref().is_none_or(|t| t["status"] != "running") {
        problems.push(format!(
            "the previous turn's monitor was stopped by this interrupt: {long:?}"
        ));
    }
    assert!(problems.is_empty(), "{problems:#?}");
    run.finish().await;
    Ok(())
}

/// A size refusal sets off a compaction before the retry; a host interrupt
/// while its summary is being written ends the turn the way any interrupt
/// does, not with an engine error.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_during_the_compaction_for_a_size_refusal_is_not_an_engine_error(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("retry-interrupt", &stub).start().await?;
    let session = run.session().await?;
    a_few_rounds(&mut run, &stub, &session, 12).await?;
    stub.script_when(
        SUMMARY,
        [Reply::delayed(
            Duration::from_secs(60),
            Reply::text("SUMMARY-TOO-LATE"),
        )],
    );
    stub.inject_failures(1, Reply::status(400, TOO_LONG));
    stub.script([Reply::text("ANSWERED-AFTER-COMPACTING")]);
    let handle = run.spawn_turn(&session, "and now?", json!({})).await?;
    wait_until("a summary request", Duration::from_secs(30), || {
        stub.calls().iter().any(|c| c.purpose() == Purpose::Compact)
    })
    .await?;
    let interrupted = interrupt(&mut run, &session).await?;
    assert_eq!(interrupted["interrupted"], json!(true), "{interrupted}");
    let events = handle.await??;
    let error = events.response.as_ref().and_then(|r| r.error.as_ref());
    assert!(
        error.is_none(),
        "the interrupted turn failed: {error:?}; turn_complete {:?}",
        events.turn_complete_event
    );
    run.finish().await;
    Ok(())
}

/// `session.compact` is a turn of its own; an interrupt while its summary is
/// being written ends it, and the late summary is never applied.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_stops_session_compact_while_its_summary_is_written() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("compact-interrupt", &stub).start().await?;
    let session = run.session().await?;
    a_few_rounds(&mut run, &stub, &session, 4).await?;
    stub.script_when(
        SUMMARY,
        [Reply::delayed(
            Duration::from_secs(8),
            Reply::text("SUMMARY-WRITTEN-LATE"),
        )],
    );
    let mut compactor = run.daemon.connect().await?;
    let id = session.clone();
    let compacting = tokio::spawn(async move {
        compactor
            .call("session.compact", json!({ "session_id": id }))
            .await
    });
    wait_until("a summary request", Duration::from_secs(30), || {
        stub.calls().iter().any(|c| c.purpose() == Purpose::Compact)
    })
    .await?;
    let interrupted = interrupt(&mut run, &session).await?;
    assert_eq!(interrupted["interrupted"], json!(true), "{interrupted}");
    let compacted = compacting.await??;
    let history = run
        .client
        .call(
            "session.history",
            json!({ "session_id": session, "limit": 100 }),
        )
        .await?
        .result
        .unwrap_or_default()
        .to_string();
    assert!(
        !history.contains("SUMMARY-WRITTEN-LATE"),
        "the summary written after the interrupt was applied; session.compact answered {:?}",
        compacted
            .result
            .or(compacted.error.map(|e| json!(e.message)))
    );
    run.finish().await;
    Ok(())
}

/// A transcript that exists but fails to load is a failed resume, not a
/// session with no history: the turn is refused, the model is never asked,
/// and the log is left exactly as it was — no second `Meta`, no new turn.
/// Two ways to break one: a whole line that is not UTF-8 (a newline-ended
/// line was written in full, so it is not a torn tail the reader may drop),
/// and a file this process may write but not read.
#[tokio::test(flavor = "multi_thread")]
async fn a_transcript_that_fails_to_load_is_not_resumed_as_an_empty_session() -> anyhow::Result<()>
{
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("unloadable", &stub).start().await?;
    let breakages: [(&str, &[u8], bool); 2] = [
        ("a line that is not UTF-8", b"\xff\xfe\n", false),
        ("a write-only file", b"", true),
    ];
    for (n, (how, appended, write_only)) in breakages.into_iter().enumerate() {
        let session = run.session().await?;
        stub.script([Reply::text("first answer")]);
        run.turn(&session, &format!("FIRST-MARK-{n} hello")).await?;
        let closed = run.client.session_close(&session).await?;
        anyhow::ensure!(closed.error.is_none(), "{closed:?}");
        let transcript = run
            .world
            .transcript_dir(&run.project)
            .join(format!("{session}.jsonl"));
        anyhow::ensure!(
            transcript.exists(),
            "no transcript at {}",
            transcript.display()
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)?
            .write_all(appended)?;
        let broken = std::fs::read(&transcript)?;
        if write_only {
            std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o200))?;
            if std::fs::read(&transcript).is_ok() {
                eprintln!("skipping {how}: this process reads files it has no read permission on");
                std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o600))?;
                continue;
            }
        }

        stub.script([Reply::text("second answer")]);
        let second_mark = format!("SECOND-MARK-{n}");
        let resumed = run
            .client
            .run_turn_with(
                json!({ "session_id": session, "message": format!("{second_mark} again") }),
            )
            .await?;
        if write_only {
            std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o600))?;
        }

        let error = resumed.response.as_ref().and_then(|r| r.error.as_ref());
        assert!(
            error.is_some_and(|e| e.message.contains("could not be loaded")),
            "{how}: the turn on a transcript that does not load was not refused: {:?}",
            resumed.response
        );
        assert!(
            turns_with(&stub, &second_mark, &[]).is_empty(),
            "{how}: the model was asked to go on from a transcript that did not load"
        );
        assert!(
            std::fs::read(&transcript)? == broken,
            "{how}: the refused resume wrote to the transcript"
        );
    }
    run.finish().await;
    Ok(())
}

/// A settings change rebuilds an idle session's engine before its next turn;
/// what the session was in — plan mode, its todo list, the deferred tools it
/// loaded — is still what it is in afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn a_rebuilt_session_keeps_plan_mode_todos_and_loaded_tools() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let builds = scratch.path().join("builds.log");
    let hook = executable(
        scratch.path(),
        "start.sh",
        &format!(
            "#!/bin/sh\ncat >/dev/null\necho started >> '{}'\necho '{{}}'\n",
            builds.display()
        ),
    )?;
    let mut run = stubbed("rebuilt", &stub)
        .settings(json!({
            "hooks_config": { "SessionStart": [{ "type": "command", "command": hook }] }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("ts-1", "ToolSearch", json!({ "query": "select:TodoWrite" })),
        Reply::calls(
            "td-1",
            "TodoWrite",
            json!({ "todos": [{ "content": "step one", "status": "pending", "active_form": "doing step one" }] }),
        ),
        Reply::calls("pm-1", "EnterPlanMode", json!({})),
        Reply::text("planned"),
    ]);
    run.turn(&session, "plan it").await?;
    let before = runtime_of(&mut run, &session, "before").await?;
    anyhow::ensure!(
        before["permission_mode"] == "plan"
            && before["todos"].as_array().is_some_and(|t| t.len() == 1),
        "the session never got into the state this case checks: {before}"
    );
    let starts = || {
        std::fs::read_to_string(&builds)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let starts_before = starts();

    let reloaded = run.client.config_reload(&rpc_client::Target::own()).await?;
    anyhow::ensure!(reloaded.error.is_none(), "{reloaded:?}");
    stub.script([Reply::text("after the reload")]);
    run.turn(&session, "carry on").await?;
    anyhow::ensure!(
        starts() > starts_before,
        "the session was not rebuilt ({starts_before} -> {} starts)",
        starts()
    );
    let after = runtime_of(&mut run, &session, "after").await?;

    stub.script_when(SUMMARY, [Reply::text("1. User intent — plan it.")]);
    let compacted = run.client.session_compact(&session, None, None).await?;
    anyhow::ensure!(compacted.error.is_none(), "{compacted:?}");
    stub.script([Reply::text("after the compaction")]);
    run.turn(&session, "LAST-MARK and now").await?;
    let last = turns_with(&stub, "LAST-MARK", &[]);
    let loaded = last.last().is_some_and(|c| {
        c.messages_contain("These deferred tools were loaded earlier")
            && c.messages_contain("TodoWrite")
    });

    let mut problems = Vec::new();
    if after["permission_mode"] != before["permission_mode"] {
        problems.push(format!(
            "permission_mode {} -> {}",
            before["permission_mode"], after["permission_mode"]
        ));
    }
    if after["todos"] != before["todos"] {
        problems.push(format!("todos {} -> {}", before["todos"], after["todos"]));
    }
    if !loaded {
        problems
            .push("the compaction after the rebuild no longer lists TodoWrite as loaded".into());
    }
    assert!(problems.is_empty(), "{problems:#?}");
    run.finish().await;
    Ok(())
}

/// A session the pool evicts at its cap comes back from its log on the next
/// turn. One the model put in plan mode is still in plan mode then: leaving
/// it is `ExitPlanMode`'s question to the user, not the pool's to answer by
/// dropping the engine.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_evicted_at_the_cap_comes_back_still_in_plan_mode() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("evicted-plan", &stub)
        .settings(json!({ "permissions": {"mode": "acceptEdits"} }))
        .options(|o| o.session_cap(1))
        .start()
        .await?;
    let planner = run.session().await?;
    stub.script([
        Reply::calls("pm-1", "EnterPlanMode", json!({})),
        Reply::text("planned"),
    ]);
    run.turn(&planner, "plan it").await?;
    let before = runtime_of(&mut run, &planner, "before").await?;
    anyhow::ensure!(
        before["permission_mode"] == "plan",
        "the session never got into plan mode: {before}"
    );

    let other = run.session().await?;
    stub.script([Reply::text("other answer")]);
    run.turn(&other, "something else").await?;

    let target = run.project.join("written-after-eviction.txt");
    stub.script([
        Reply::calls(
            "w-1",
            "Write",
            json!({ "file_path": target.to_string_lossy(), "content": "x" }),
        ),
        Reply::text("wrote it"),
    ]);
    run.turn(&planner, "BACK-MARK write the file now").await?;
    let after = runtime_of(&mut run, &planner, "after").await?;

    let mut problems = Vec::new();
    if after["permission_mode"] != "plan" {
        problems.push(format!(
            "permission_mode plan -> {} after the eviction",
            after["permission_mode"]
        ));
    }
    if target.exists() {
        problems.push("Write ran in a session that was in plan mode".to_string());
    }
    assert!(problems.is_empty(), "{problems:#?}");
    run.finish().await;
    Ok(())
}

/// An interrupt stops the background agents its turn started; their tasks
/// end `stopped`, not `done` (protocol `session.interrupt`, `session.tasks`).
#[tokio::test(flavor = "multi_thread")]
async fn a_background_agent_stopped_by_an_interrupt_ends_stopped() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("bg-agent-interrupt", &stub)
        .settings(json!({
            "permissions": {"allow": ["Agent", "Bash"]}
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls(
                "a-bg",
                "Agent",
                json!({ "prompt": "BG-MARK work", "description": "bg", "background": true }),
            ),
            Reply::calls("b-1", "Bash", json!({ "command": WAIT_FOR_GO })),
            Reply::text("parent done"),
        ],
    );
    stub.script_when(
        "BG-MARK",
        [Reply::delayed(
            Duration::from_secs(120),
            Reply::text("bg done"),
        )],
    );
    let handle = run
        .spawn_turn(&session, "PARENT-MARK start a helper, then wait", json!({}))
        .await?;
    let waiting = run.project.join("waiting");
    wait_until(
        "the parent's command waiting",
        Duration::from_secs(30),
        || waiting.exists(),
    )
    .await?;
    task_of_kind(&mut run, &session, "agent").await?;
    let interrupted = interrupt(&mut run, &session).await?;
    assert_eq!(interrupted["interrupted"], json!(true), "{interrupted}");
    std::fs::write(run.project.join("go"), "")?;
    handle.await??;
    let deadline = Instant::now() + Duration::from_secs(30);
    let task = loop {
        let task = task_of_kind(&mut run, &session, "agent").await?;
        if task["status"] != "running" && task["status"] != "waiting" {
            break task;
        }
        anyhow::ensure!(Instant::now() < deadline, "the agent never ended: {task}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(task["status"], json!("stopped"), "{task}");
    run.finish().await;
    Ok(())
}

/// `session.setPrompt` rebuilds an idle session. A `SessionEnd` hook that takes
/// its time runs on in the background rather than holding the old engine up:
/// the rebuild goes through and the next turn is served.
#[tokio::test(flavor = "multi_thread")]
async fn a_rebuild_whose_old_engine_is_slow_to_stop_does_not_swallow_the_next_turn(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let go = scratch.path().join("go");
    let hook = executable(
        scratch.path(),
        "end.sh",
        &format!(
            "#!/bin/sh\ncat >/dev/null\nwhile [ ! -e '{}' ]; do sleep 0.05; done\necho '{{}}'\n",
            go.display()
        ),
    )?;
    let mut run = stubbed("slow-stop", &stub)
        .settings(json!({
            "hooks_config": { "SessionEnd": [{
                "type": "command", "command": hook, "timeout": 120_000
            }] }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([Reply::text("one")]);
    run.turn(&session, "ONE-MARK hello").await?;
    let set = run
        .client
        .call(
            "session.setPrompt",
            json!({ "session_id": session, "append": "Answer briefly." }),
        )
        .await?;
    stub.script([Reply::text("two answered")]);
    let second = run
        .spawn_turn(&session, "TWO-MARK again", json!({}))
        .await?;
    std::fs::write(&go, "")?;
    let set_answer = set
        .result
        .clone()
        .or(set.error.clone().map(|e| json!(e.message)));
    let Ok(events) = tokio::time::timeout(Duration::from_secs(30), second).await else {
        panic!(
            "the turn after the rebuild never ended once the old engine could stop; \
             setPrompt answered {set_answer:?}"
        );
    };
    let events = events??;
    assert!(set.error.is_none(), "setPrompt answered {set_answer:?}");
    let history = run
        .client
        .call(
            "session.history",
            json!({ "session_id": session, "limit": 100 }),
        )
        .await?
        .result
        .unwrap_or_default()
        .to_string();
    let error = events.response.as_ref().and_then(|r| r.error.clone());
    assert!(
        error.is_none() && history.contains("TWO-MARK"),
        "setPrompt answered {:?}; the next turn answered {error:?} and the history has it: {}",
        set.result.or(set.error.map(|e| json!(e.message))),
        history.contains("TWO-MARK")
    );
    run.finish().await;
    Ok(())
}

/// A turn cut off by `session.close` still ends for every subscriber with a
/// terminal frame (§5.5): a tab watching it is not left waiting.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_cut_off_by_close_ends_with_a_terminal_frame_for_its_watchers() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let (holding, release) = (
        scratch.path().join("holding"),
        scratch.path().join("release"),
    );
    let hook = executable(
        scratch.path(),
        "hold.sh",
        &format!(
            "#!/bin/sh\ncat >/dev/null\ntouch '{holding}'\n\
             while [ ! -e '{release}' ]; do sleep 0.05; done\necho '{{}}'\n",
            holding = holding.display(),
            release = release.display(),
        ),
    )?;
    let mut run = stubbed("close-mid-turn", &stub)
        .settings(json!({
            "permissions": {"allow": ["Bash"]},
            "hooks_config": { "PreToolUse": [{
                "type": "command", "command": hook, "timeout": 120_000
            }] }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    stub.script([
        Reply::calls("b-1", "Bash", json!({ "command": "echo hi" })),
        Reply::text("done"),
    ]);
    let handle = run.spawn_turn(&session, "run it", json!({})).await?;
    wait_until("the hook holding the call", Duration::from_secs(30), || {
        holding.exists()
    })
    .await?;
    let turn_id = watcher
        .wait_for("the tool_use frame", Duration::from_secs(30), |e| {
            e["kind"] == "tool_use"
        })
        .await?["turn_id"]
        .clone();
    let closed = run.client.session_close(&session).await?;
    anyhow::ensure!(closed.error.is_none(), "{closed:?}");
    std::fs::write(&release, "")?;
    let initiator = handle.await?;
    let terminal = watcher
        .wait_for("a terminal frame", Duration::from_secs(10), |e| {
            e["kind"] == "turn_complete" || e["kind"] == "error"
        })
        .await;
    assert!(
        terminal.is_ok(),
        "the watcher got no turn_complete or error for turn {turn_id}; the initiator got {:?}",
        initiator.map(|e| e.response)
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_tool_reply_with_a_bad_image_is_refused_and_can_be_given_again() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("host-image", &stub).start().await?;
    let session = run
        .session_with(json!({ "options": {
            "permission_rules": [
                { "source": "session", "behavior": "allow", "tool_name": "HostShot" }
            ],
            "host_tools": [{
                "name": "HostShot",
                "description": "A screenshot from the host.",
                "input_schema": { "type": "object", "properties": {} },
                "read_only": true
            }]
        }}))
        .await?;
    let host = run.watch(&session, false).await?;
    stub.script([
        Reply::calls("h1", "HostShot", json!({})),
        Reply::text("done"),
    ]);
    let turn = run
        .spawn_turn(&session, "take a screenshot", json!({}))
        .await?;
    let call = host
        .wait_for("the host tool call", Duration::from_secs(30), |e| {
            e["kind"] == "host_tool_call"
        })
        .await?;
    let call_id = call["event"]["call_id"].clone();
    let reply = |media_type: &str, data: String| {
        json!({
            "session_id": session,
            "call_id": call_id,
            "result": { "content": [
                { "type": "image", "source": { "type": "base64", "media_type": media_type, "data": data } }
            ] }
        })
    };
    for (media_type, data) in [
        ("image/png", "A".repeat(5_000_000)),
        ("text/html", "AAAA".to_string()),
    ] {
        let refused = run
            .client
            .call("session.respondToolCall", reply(media_type, data))
            .await?;
        let error = refused.error.expect("a bad image is refused");
        assert_eq!(error.code, -32602, "{media_type}: {}", error.message);
    }
    let answered = run
        .client
        .session_respond_tool_call(
            &session,
            call_id.as_str().unwrap_or_default(),
            rpc_client::ToolCallReply::Result {
                content: json!([
                    { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } }
                ]),
                is_error: false,
            },
        )
        .await?;
    assert!(answered.error.is_none(), "{answered:?}");
    tokio::time::timeout(Duration::from_secs(30), turn).await???;
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_tool_name_with_hyphens_is_accepted_and_reaches_the_host_unchanged(
) -> anyhow::Result<()> {
    const NAME: &str = "ext__hello-app__echo";
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("host-hyphen", &stub).start().await?;
    let decl = json!({
        "name": NAME,
        "description": "Echo from the host.",
        "input_schema": { "type": "object", "properties": {} },
        "read_only": true
    });
    let set = run.client.daemon_set_host_tools(vec![decl.clone()]).await?;
    anyhow::ensure!(set.error.is_none(), "{set:?}");
    anyhow::ensure!(
        set.result
            .as_ref()
            .is_some_and(|r| r["refused"] == json!([])),
        "{set:?}"
    );
    let bad = run
        .client
        .daemon_set_host_tools(vec![json!({ "name": "-lead", "description": "d",
            "input_schema": { "type": "object" } })])
        .await?;
    anyhow::ensure!(
        bad.result
            .as_ref()
            .is_some_and(|r| r["refused"].as_array().is_some_and(|a| a.len() == 1)),
        "{bad:?}"
    );

    let session = run
        .session_with(json!({ "options": {
            "permission_rules": [
                { "source": "session", "behavior": "allow", "tool_name": NAME }
            ],
            "host_tools": [decl]
        }}))
        .await?;
    let host = run.watch(&session, false).await?;
    stub.script([Reply::calls("h1", NAME, json!({})), Reply::text("done")]);
    let turn = run.spawn_turn(&session, "echo", json!({})).await?;
    let call = host
        .wait_for("the host tool call", Duration::from_secs(30), |e| {
            e["kind"] == "host_tool_call"
        })
        .await?;
    assert_eq!(call["event"]["name"], NAME, "{call}");
    let answered = run
        .client
        .session_respond_tool_call(
            &session,
            call["event"]["call_id"].as_str().unwrap_or_default(),
            rpc_client::ToolCallReply::Result {
                content: json!([{ "type": "text", "text": "ok" }]),
                is_error: false,
            },
        )
        .await?;
    assert!(answered.error.is_none(), "{answered:?}");
    tokio::time::timeout(Duration::from_secs(30), turn).await???;
    run.finish().await;
    Ok(())
}

async fn inspected_context(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    let r = run
        .client
        .call("session.inspectContext", json!({ "session_id": session }))
        .await?;
    anyhow::ensure!(r.error.is_none(), "{r:?}");
    Ok(r.result.unwrap_or_default())
}

async fn inspected_model(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    Ok(inspected_context(run, session).await?["model"].clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn inspect_context_reports_the_model_set_after_the_session_started() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("inspect-after-set-model", &stub).start().await?;
    let session = run.session().await?;
    let before = inspected_model(&mut run, &session).await?;
    let chosen = "switched-model-x";
    assert_ne!(before, json!(chosen));
    let set = run.client.session_set_model(&session, chosen).await?;
    anyhow::ensure!(set.error.is_none(), "{set:?}");
    let after = inspected_model(&mut run, &session).await?;
    assert_eq!(after, json!(chosen));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn inspect_context_reports_the_provider_the_next_call_goes_to() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let provider = |model: &str| {
        json!({
            "api_type": "anthropic",
            "base_url": stub.base_url().as_str(),
            "api_key": "stub-key",
            "default_model": model,
            "models": [model]
        })
    };
    let mut run = stubbed("inspect-provider", &stub)
        .settings(json!({
            "providers": { "first": provider("model-one"), "second": provider("model-two") },
            "default_provider": "first"
        }))
        .start()
        .await?;
    let session = run.session().await?;
    assert_eq!(
        {
            let c = inspected_context(&mut run, &session).await?;
            c["provider"].clone()
        },
        json!("first")
    );
    let set = run
        .client
        .call(
            "session.setModel",
            json!({ "session_id": session, "model": "model-two", "provider": "second" }),
        )
        .await?;
    anyhow::ensure!(set.error.is_none(), "{set:?}");
    let c = inspected_context(&mut run, &session).await?;
    assert_eq!(
        (&c["provider"], &c["model"]),
        (&json!("second"), &json!("model-two"))
    );
    run.finish().await;
    Ok(())
}
