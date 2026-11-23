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

#[tokio::test(flavor = "multi_thread")]
async fn a_project_whose_providers_do_not_route_gets_no_session_and_nothing_is_sent(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let inhouse = |base_url: String| {
        json!({
            "id": "inhouse",
            "api": "anthropic",
            "base_url": base_url,
            "api_key_env": "ANTHROPIC_AUTH_TOKEN",
            "tiers": {"strong": "inhouse-model"},
        })
    };
    let broken = [
        (
            "order-typo",
            json!({
                "providers": [inhouse(format!("{}inhouse/", stub.base_url()))],
                "model": {"order": ["inhuose"]},
            }),
            "inhuose",
        ),
        (
            "bad-url",
            json!({"providers": [inhouse("not a url".into())]}),
            "not a url",
        ),
    ];
    let projects = broken.clone();
    let mut run = LiveRun::builder("unrouted")
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(&stub),
        ))
        .before_start(move |world, _| {
            for (name, settings, _) in projects {
                world.write_project_settings(name, &settings)?;
                world.trust_project(&world.project(name)?)?;
            }
            Ok(())
        })
        .start()
        .await?;

    for (name, _, cause) in broken {
        let root = run.world.project(name)?;
        let created = run
            .client
            .call(
                "session.create",
                json!({"project_root": root, "options": {"auto_name": false}}),
            )
            .await?;
        let error = created
            .error
            .unwrap_or_else(|| panic!("{name}: a session was created: {:?}", created.result));
        assert_eq!(error.code, -32602, "{name}: {error:?}");
        assert!(error.message.contains(cause), "{name}: {error:?}");
    }
    assert!(stub.calls().is_empty(), "{:?}", stub.calls());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_whose_id_is_not_a_plain_name_is_left_out_and_the_doctor_says_why(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let provider = |id: &str, path: &str| {
        json!({
            "id": id,
            "api": "anthropic",
            "base_url": format!("{}{path}/", stub.base_url()),
            "api_key_env": "ANTHROPIC_AUTH_TOKEN",
            "tiers": {"strong": format!("{path}-model")},
        })
    };
    let global = json!({
        "providers": [provider("first.one", "dotted"), provider("first_one", "underscored")],
    });
    let mut run = LiveRun::builder("provider-id")
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(&stub),
        ))
        .before_start(move |world, _| world.write_global_settings(&global))
        .start()
        .await?;
    let session = run
        .session_with(json!({"options": {"auto_name": false}}))
        .await?;
    stub.script([Reply::text("UNDERSCORED-ANSWER")]);
    let turn = run.turn(&session, "one").await?;
    assert!(turn.text.contains("UNDERSCORED-ANSWER"), "{}", turn.text);
    assert_eq!(turn_calls_by_provider(&stub), ["underscored"]);

    let doctor = run
        .client
        .call("daemon.doctor", json!({}))
        .await?
        .result
        .unwrap_or_default();
    let tiers = doctor["health"]["checks"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["name"] == "settings.tiers"))
        .map(|c| c["details"].clone())
        .unwrap_or_default();
    let global_tier = tiers
        .as_array()
        .and_then(|t| t.iter().find(|t| t["tier"] == "global"))
        .cloned()
        .unwrap_or_default();
    let ignored = &global_tier["ignored_values"];
    assert_eq!(ignored[0]["key"], "providers[0]", "{tiers}");
    assert!(
        ignored[0]["error"]
            .as_str()
            .is_some_and(|e| e.contains("first.one")),
        "{ignored}"
    );
    run.finish().await;
    Ok(())
}
