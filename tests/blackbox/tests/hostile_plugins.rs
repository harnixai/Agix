//! A Rust plugin added to the daemon that misbehaves — panics in `register`,
//! panics in a tool, blocks in a health check, takes a built-in's name — and
//! what it takes down with it. In this process only: a spawned `agixd`
//! cannot be handed Rust code.

use std::sync::Arc;
use std::time::{Duration, Instant};

use blackbox::{Host, LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Reply};
use hostile_plugins as hostile;
use runtime::plugin::Plugin;
use serde_json::json;

fn with_plugins(name: &str, stub: &ProviderStub, plugins: Vec<Arc<dyn Plugin>>) -> LiveRunBuilder {
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .host(Host {
            plugins,
            ..Default::default()
        })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_that_panics_fails_its_call_and_nothing_else() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_plugins(
        "hostile-call",
        &stub,
        vec![
            Arc::new(hostile::PanicOnCall),
            Arc::new(hostile::WellBehaved),
        ],
    )
    .start()
    .await?;
    let session = run.session().await?;
    stub.script([
        Reply::calls("c1", hostile::PANICKING_TOOL, json!({})),
        Reply::calls("c2", hostile::CONTROL_TOOL, json!({})),
        Reply::text("finished"),
    ]);
    let events = run.turn(&session, "go").await?;
    assert!(events.turn_complete && events.text.contains("finished"));
    let (_, _, panicked) = &events.tool_results[0];
    assert!(*panicked, "the panicking call was not reported as failed");
    let (_, out, failed) = &events.tool_results[1];
    assert!(!*failed && out.contains(hostile::CONTROL_MARKER), "{out}");

    let other = run.session().await?;
    stub.script([Reply::text("still here")]);
    let events = run.turn(&other, "hello").await?;
    assert!(events.text.contains("still here"));
    let ping = run.client.call("daemon.ping", json!({})).await?;
    assert!(ping.error.is_none());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_that_panics_in_register_does_not_take_the_daemon_down() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_plugins(
        "hostile-register",
        &stub,
        vec![Arc::new(hostile::PanicOnRegister)],
    )
    .start()
    .await?;
    let created = run
        .client
        .call(
            "session.create",
            json!({ "project_root": run.project.to_string_lossy() }),
        )
        .await?;
    assert!(
        created.error.is_some(),
        "a session was built around a plugin whose register panicked: {created:?}"
    );

    let mut second = run.daemon.connect().await?;
    let ping = second.call("daemon.ping", json!({})).await?;
    assert!(ping.error.is_none(), "{ping:?}");
    let status = run.client.call("daemon.status", json!({})).await?;
    assert!(status.error.is_none(), "{status:?}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_named_like_a_builtin_is_refused_by_name_and_replaces_nothing() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let mut run = with_plugins("hostile-shadow", &stub, vec![Arc::new(hostile::Shadowing)])
        .start()
        .await?;
    let created = run
        .client
        .call(
            "session.create",
            json!({ "project_root": run.project.to_string_lossy() }),
        )
        .await?;
    let error = created
        .error
        .as_ref()
        .expect("a session was built with a tool shadowing a built-in");
    assert!(
        error.message.contains("\"Read\""),
        "the refusal does not name the tool: {error:?}"
    );
    let ping = run.client.call("daemon.ping", json!({})).await?;
    assert!(ping.error.is_none());
    assert_eq!(stub.call_count(), 0);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_health_check_that_blocks_does_not_stall_other_sessions() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let block = Duration::from_secs(30);
    let mut run = with_plugins(
        "hostile-health",
        &stub,
        vec![Arc::new(hostile::HangingHealth { block })],
    )
    .start()
    .await?;
    let session = run.session().await?;
    let mut doctor = run.daemon.connect().await?;
    let watched = session.clone();
    let stuck = tokio::spawn(async move {
        let started = Instant::now();
        let report = doctor
            .call("session.get", json!({ "session_id": watched }))
            .await;
        (report, started.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    stub.script([Reply::text("quick")]);
    let started = Instant::now();
    let events = run.turn(&session, "hello").await?;
    assert!(events.text.contains("quick"));
    assert!(
        started.elapsed() < block,
        "a turn waited for another request's health check"
    );
    let (report, took) = stuck.await?;
    assert!(
        took < block,
        "session.get waited out the whole block: {took:?}"
    );
    let report = report?.result.expect("a session.get result");
    let hung = report["health"]["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == "hostile.hang")
        .cloned()
        .expect("the hanging check is reported");
    assert_eq!(hung["status"], "degraded", "{hung}");
    run.finish().await;
    Ok(())
}
