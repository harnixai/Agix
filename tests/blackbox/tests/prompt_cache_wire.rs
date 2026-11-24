//! The prompt-cache breakpoints a code session's model calls send carry only
//! `type` and `ttl`, and number at most four: Anthropic's endpoint refuses
//! any other key and any fifth breakpoint.

use blackbox::{LiveConfig, LiveRun, ProviderStub, Reply};
use serde_json::Value;

fn collect_cache_controls(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                if k == "cache_control" {
                    out.push(child.clone());
                } else {
                    collect_cache_controls(child, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|i| collect_cache_controls(i, out)),
        _ => {}
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_code_sessions_first_call_has_no_scope_on_any_cache_control() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = LiveRun::builder("cache-control-wire")
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(&stub),
        ))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([Reply::text("done")]);
    run.turn(&session, "hello").await?;

    let first = stub.calls().remove(0);
    let mut controls = Vec::new();
    collect_cache_controls(&first.body, &mut controls);
    assert!(
        !controls.is_empty(),
        "no breakpoint was sent: {}",
        first.body
    );
    for cc in &controls {
        let keys: Vec<&String> = cc.as_object().expect("an object").keys().collect();
        assert!(
            keys.iter().all(|k| *k == "type" || *k == "ttl"),
            "unexpected cache_control keys: {cc}"
        );
    }
    run.finish().await;
    Ok(())
}

/// Anthropic refuses a request with more than four breakpoints in all, and
/// a code session's system prompt alone marks more than that.
#[tokio::test(flavor = "multi_thread")]
async fn a_code_sessions_calls_carry_at_most_four_breakpoints() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = LiveRun::builder("cache-control-budget")
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(&stub),
        ))
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([Reply::text("first")]);
    run.turn(&session, "hello").await?;
    stub.script([Reply::text("second")]);
    run.turn(&session, "and again").await?;

    let turns: Vec<_> = stub
        .calls()
        .into_iter()
        .filter(|c| c.purpose() == blackbox::Purpose::Turn)
        .collect();
    anyhow::ensure!(turns.len() >= 2, "{} turn calls", turns.len());
    for (i, call) in turns.iter().take(2).enumerate() {
        let mut controls = Vec::new();
        collect_cache_controls(&call.body, &mut controls);
        assert!(
            (1..=4).contains(&controls.len()),
            "call {i} carries {} breakpoints: {}",
            controls.len(),
            call.body
        );
    }
    run.finish().await;
    Ok(())
}
