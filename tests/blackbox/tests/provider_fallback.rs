//! `model.order` over JSON-RPC: a provider out of its quota, or failing to
//! answer, hands the session's calls to the next one, and the session is
//! told with a `provider_switched` event.

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply};
use serde_json::{json, Value};

/// Providers `first` and `second`, both on `stub` under their own path,
/// `first` with `quota`.
fn two_providers(name: &str, stub: &ProviderStub, quota: Value) -> LiveRunBuilder {
    let provider = |id: &str| {
        json!({
            "id": id,
            "api": "anthropic",
            "base_url": format!("{}{id}/", stub.base_url()),
            "api_key_env": "ANTHROPIC_AUTH_TOKEN",
            "tiers": {"strong": format!("{id}-model")},
        })
    };
    let mut first = provider("first");
    first["quota"] = quota;
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .settings(json!({
            "providers": [first, provider("second")],
            "model": {"order": ["first", "second"]},
        }))
}

/// Which provider each of the turn loop's calls went to, in order.
fn turn_calls_by_provider(stub: &ProviderStub) -> Vec<String> {
    stub.calls()
        .into_iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .map(|c| {
            c.path
                .trim_start_matches('/')
                .split('/')
                .next()
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

fn switches(events: &rpc_client::TurnEvents) -> Vec<Value> {
    events
        .events
        .iter()
        .filter(|e| e["kind"] == "provider_switched")
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_past_its_daily_quota_hands_the_next_turn_to_the_next_one() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = two_providers("quota-fallback", &stub, json!({"day": 20}))
        .start()
        .await?;
    let session = run
        .session_with(json!({"options": {"auto_name": false}}))
        .await?;
    stub.script([Reply::text("FIRST-ANSWER"), Reply::text("SECOND-ANSWER")]);

    let first = run.turn(&session, "one").await?;
    assert!(first.text.contains("FIRST-ANSWER"), "{}", first.text);
    assert!(switches(&first).is_empty(), "{:?}", first.events);
    let second = run.turn(&session, "two").await?;
    assert!(second.text.contains("SECOND-ANSWER"), "{}", second.text);

    assert_eq!(turn_calls_by_provider(&stub), ["first", "second"]);
    let switched = switches(&second);
    assert_eq!(switched.len(), 1, "{:?}", second.events);
    assert_eq!(
        (
            &switched[0]["from"],
            &switched[0]["to"],
            &switched[0]["task"]
        ),
        (&json!("first"), &json!("second"), &json!("main"))
    );
    assert!(
        switched[0]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("day quota")),
        "{switched:?}"
    );

    let shown = run
        .client
        .call(
            "config.getProvider",
            json!({"project_root": run.project, "provider_id": "first"}),
        )
        .await?;
    let usage = &shown.result.expect("getProvider")["providers"][0]["usage"]["day"];
    assert_eq!(usage["limit"], 20, "{usage}");
    assert_eq!(
        usage["used"],
        blackbox::provider::INPUT_TOKENS
            + blackbox::provider::OUTPUT_TOKENS
            + blackbox::provider::CACHE_CREATION_TOKENS,
        "cache reads are not counted: {usage}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_failing_with_a_server_error_hands_the_call_to_the_next_one(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = two_providers("server-error-fallback", &stub, json!({}))
        .start()
        .await?;
    let session = run
        .session_with(json!({"options": {"auto_name": false}}))
        .await?;
    stub.inject_failures_when(
        "\"model\":\"first-model\"",
        10,
        Reply::status(
            501,
            r#"{"type":"error","error":{"type":"server_error","message":"down"}}"#,
        ),
    );
    stub.script([Reply::text("SERVED-BY-SECOND"), Reply::text("STILL-SECOND")]);

    let first = run.turn(&session, "one").await?;
    assert!(first.text.contains("SERVED-BY-SECOND"), "{}", first.text);
    let switched = switches(&first);
    assert_eq!(switched.len(), 1, "{:?}", first.events);
    assert_eq!(
        (&switched[0]["from"], &switched[0]["to"]),
        (&json!("first"), &json!("second"))
    );
    assert!(
        switched[0]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("501")),
        "{switched:?}"
    );

    let second = run.turn(&session, "two").await?;
    assert!(second.text.contains("STILL-SECOND"), "{}", second.text);
    assert!(switches(&second).is_empty(), "{:?}", second.events);
    assert_eq!(
        turn_calls_by_provider(&stub),
        ["first", "second", "second"],
        "a provider that failed is passed over while it cools down"
    );
    run.finish().await;
    Ok(())
}
