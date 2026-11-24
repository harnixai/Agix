//! A compaction's first and last frames, against a scripted model: every
//! compaction that rewrites the history opens with `compact_started` and ends
//! in exactly one `compact` (it went through) or `compact_ended` (it did not)
//! — asked for, forced by the threshold, set off by a refusal for size,
//! declined by a hook, with nothing to drop, with its summary failing, and
//! interrupted while the summary is written.

use std::time::Duration;

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply, Watcher};
use rpc_client::CompactAt;
use serde_json::{json, Value};

const SUMMARY: &str = "You are compacting an AI coding agent's conversation";

const TOO_LONG: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 212034 tokens > 200000 maximum"}}"#;

const WAIT: Duration = Duration::from_secs(60);

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

/// `rounds` rounds of file reads, `r0`, `r1`, …, so a compaction has
/// something to summarize.
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

fn is_end(event: &Value) -> bool {
    event["kind"] == "compact_ended" || (event["kind"] == "compact" && event["trim"] != true)
}

/// The compaction frames so far, trims left out, once the last start has its
/// end.
async fn bracketed(watcher: &Watcher) -> anyhow::Result<Vec<Value>> {
    watcher
        .wait_for("the end of a compaction", WAIT, is_end)
        .await?;
    let frames: Vec<Value> = watcher
        .frames()
        .into_iter()
        .map(|f| f["event"].clone())
        .filter(|e| e["kind"] == "compact_started" || is_end(e))
        .collect();
    let mut open = false;
    for frame in &frames {
        let starts = frame["kind"] == "compact_started";
        anyhow::ensure!(starts != open, "start and end out of turn: {frames:?}");
        open = starts;
    }
    anyhow::ensure!(!open, "a compaction never ended: {frames:?}");
    Ok(frames)
}

#[tokio::test(flavor = "multi_thread")]
async fn session_compact_opens_with_a_start_and_ends_with_its_compact_frame() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("compact-frames-manual", &stub).start().await?;
    let session = run.session().await?;
    a_few_rounds(&mut run, &stub, &session, 4).await?;
    let watcher = run.watch(&session, false).await?;
    stub.script_when(SUMMARY, [Reply::text("1. User intent — read four files.")]);
    let compacted = run.client.session_compact(&session, None, None).await?;
    anyhow::ensure!(compacted.error.is_none(), "{compacted:?}");
    let frames = bracketed(&watcher).await?;
    assert_eq!(frames.len(), 2, "{frames:?}");
    let (started, ended) = (&frames[0], &frames[1]);
    assert_eq!(started["trigger"], "manual", "{started}");
    assert!(started.get("strategy").is_none(), "{started}");
    assert_eq!(ended["kind"], "compact", "{ended}");
    assert_eq!(ended["strategy"], "FullCompact", "{ended}");
    assert_eq!(
        started["tokens_before"], ended["tokens_before"],
        "{frames:?}"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_with_nothing_to_compact_ends_saying_so() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("compact-frames-nothing", &stub).start().await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    let events = run.turn(&session, "/compact").await?;
    assert!(
        events.text.contains("Nothing to compact"),
        "{}",
        events.text
    );
    let frames = bracketed(&watcher).await?;
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["trigger"], "manual", "{frames:?}");
    assert_eq!(frames[1]["kind"], "compact_ended", "{frames:?}");
    assert_eq!(frames[1]["outcome"], "nothing_to_compact", "{frames:?}");
    assert!(frames[1].get("reason").is_none(), "{frames:?}");
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_a_pre_compact_hook_declines_ends_declined() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let hooks = tempfile::tempdir()?;
    let script = hooks.path().join("decline.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\ncat >/dev/null\necho '{\"decision\":\"block\",\"reason\":\"not now\"}'\n",
    )?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    }
    let mut run = stubbed("compact-frames-declined", &stub)
        .settings(json!({ "hooks": { "PreCompact": [
            { "type": "command", "command": script, "timeout": 30_000 }
        ] } }))
        .start()
        .await?;
    let session = run.session().await?;
    a_few_rounds(&mut run, &stub, &session, 4).await?;
    let watcher = run.watch(&session, false).await?;
    run.turn(&session, "/compact").await?;
    let frames = bracketed(&watcher).await?;
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["trigger"], "manual", "{frames:?}");
    assert_eq!(frames[1]["kind"], "compact_ended", "{frames:?}");
    assert_eq!(frames[1]["outcome"], "declined", "{frames:?}");
    assert!(
        !stub.calls().iter().any(|c| c.purpose() == Purpose::Compact),
        "a declined compaction asked for a summary"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// Compacting up to a chosen point has no cascade to fall back on, so the
/// summary's failure is the compaction's, and the frame says why.
#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_whose_summary_fails_ends_failed_with_the_reason() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("compact-frames-failed", &stub).start().await?;
    let session = run.session().await?;
    a_few_rounds(&mut run, &stub, &session, 4).await?;
    let watcher = run.watch(&session, false).await?;
    stub.script_when(
        SUMMARY,
        [Reply::status(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"SUMMARY-REFUSED"}}"#,
        )],
    );
    run.client
        .session_compact(&session, None, Some(CompactAt::ToolUse("r2")))
        .await?;
    let frames = bracketed(&watcher).await?;
    assert_eq!(frames.len(), 2, "{frames:?}");
    let (started, ended) = (&frames[0], &frames[1]);
    assert_eq!(started["trigger"], "manual", "{started}");
    assert_eq!(started["strategy"], "FullCompact", "{started}");
    assert_eq!(ended["kind"], "compact_ended", "{ended}");
    assert_eq!(ended["outcome"], "failed", "{ended}");
    assert!(
        ended["reason"]
            .as_str()
            .is_some_and(|r| r.contains("SUMMARY-REFUSED")),
        "{ended}"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_the_threshold_forces_starts_as_threshold() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("compact-frames-threshold", &stub)
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
        (0..4).map(|_| Reply::text("1. User intent — read the parts.")),
    );
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    run.turn(&session, "read the four parts").await?;
    let frames = bracketed(&watcher).await?;
    let first = frames.first().expect("a compaction");
    assert_eq!(first["kind"], "compact_started", "{frames:?}");
    assert_eq!(first["trigger"], "threshold", "{frames:?}");
    assert!(
        frames.iter().any(|f| f["kind"] == "compact"),
        "no compaction went through: {frames:?}"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_a_refusal_for_size_sets_off_starts_as_overflow() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("compact-frames-overflow", &stub).start().await?;
    let session = run.session().await?;
    a_few_rounds(&mut run, &stub, &session, 4).await?;
    let watcher = run.watch(&session, false).await?;
    stub.script_when(SUMMARY, [Reply::text("1. User intent — read four files.")]);
    stub.inject_failures(1, Reply::status(400, TOO_LONG));
    stub.script([Reply::text("ANSWERED-AFTER-COMPACTING")]);
    let events = run.turn(&session, "and now?").await?;
    assert!(
        events.text.contains("ANSWERED-AFTER-COMPACTING"),
        "{}",
        events.text
    );
    let frames = bracketed(&watcher).await?;
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["trigger"], "overflow", "{frames:?}");
    assert_eq!(frames[1]["kind"], "compact", "{frames:?}");
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_interrupted_while_its_summary_is_written_ends_cancelled() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("compact-frames-cancelled", &stub).start().await?;
    let session = run.session().await?;
    a_few_rounds(&mut run, &stub, &session, 4).await?;
    let watcher = run.watch(&session, false).await?;
    stub.script_when(
        SUMMARY,
        [Reply::delayed(
            Duration::from_secs(300),
            Reply::text("SUMMARY-TOO-LATE"),
        )],
    );
    let mut compactor = run.daemon.connect().await?;
    let id = session.clone();
    let compacting = tokio::spawn(async move {
        compactor
            .call("session.compact", json!({ "session_id": id }))
            .await
    });
    watcher
        .wait_for("compact_started", WAIT, |e| e["kind"] == "compact_started")
        .await?;
    let deadline = std::time::Instant::now() + WAIT;
    while !stub.calls().iter().any(|c| c.purpose() == Purpose::Compact) {
        anyhow::ensure!(std::time::Instant::now() < deadline, "no summary request");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let interrupted = run
        .client
        .call("session.interrupt", json!({ "session_id": session }))
        .await?;
    assert_eq!(
        interrupted.result.unwrap_or_default()["interrupted"],
        json!(true)
    );
    compacting.await??;
    let frames = bracketed(&watcher).await?;
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["trigger"], "manual", "{frames:?}");
    assert_eq!(frames[1]["kind"], "compact_ended", "{frames:?}");
    assert_eq!(frames[1]["outcome"], "cancelled", "{frames:?}");
    drop(watcher);
    run.finish().await;
    Ok(())
}
