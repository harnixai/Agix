//! The engine's own HTTP follows the environment's proxy unless the global
//! settings say `network.use_system_proxy: false`. A `PageServer` stands in
//! for the proxy and records what reaches it; the daemon is spawned so the
//! proxy variables are its own.

use blackbox::{
    Block, DaemonOptions, LiveConfig, LiveRun, LiveRunBuilder, PageServer, ProviderStub, Reply,
};
use serde_json::json;

const TARGET: &str = "http://example.invalid/page";

fn proxied_through(
    name: &str,
    stub: &ProviderStub,
    proxy: &PageServer,
    mut project: serde_json::Value,
) -> LiveRunBuilder {
    project["permissions"]["allow"] = json!(["WebFetch"]);
    let proxy_url = proxy.url("");
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .settings(project)
        .mode(blackbox::Mode::Spawned)
        .options(move |o: DaemonOptions| {
            o.env("HTTP_PROXY", proxy_url.clone())
                .env("http_proxy", proxy_url.clone())
                .env("NO_PROXY", "")
                .env("no_proxy", "")
        })
}

async fn fetch_through(run: &mut LiveRun, stub: &ProviderStub) -> anyhow::Result<()> {
    let session = run.session().await?;
    stub.script([
        Reply::Blocks(vec![Block::tool(
            "f1",
            "WebFetch",
            json!({ "url": TARGET }),
        )]),
        Reply::text("done"),
    ]);
    run.turn(&session, "fetch").await?;
    Ok(())
}

fn reached(proxy: &PageServer) -> bool {
    proxy
        .requests()
        .iter()
        .any(|r| r.contains("example.invalid"))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_system_proxy_carries_the_engines_requests_by_default() -> anyhow::Result<()> {
    let proxy = PageServer::start().await?;
    let stub = ProviderStub::start().await?;
    let mut run = proxied_through("proxy-default", &stub, &proxy, json!({}))
        .start()
        .await?;
    fetch_through(&mut run, &stub).await?;
    assert!(reached(&proxy), "seen by the proxy: {:?}", proxy.requests());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_global_tier_can_make_the_engine_connect_directly() -> anyhow::Result<()> {
    let proxy = PageServer::start().await?;
    let stub = ProviderStub::start().await?;
    let mut run = proxied_through("proxy-direct", &stub, &proxy, json!({}))
        .before_start(|world, _| {
            world.write_global_settings(&json!({ "network": { "use_system_proxy": false } }))
        })
        .start()
        .await?;
    fetch_through(&mut run, &stub).await?;
    assert!(
        !reached(&proxy),
        "seen by the proxy: {:?}",
        proxy.requests()
    );
    let doctor = run
        .client
        .call("daemon.doctor", json!({}))
        .await?
        .result
        .unwrap_or_default();
    let check = doctor["health"]["checks"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["name"] == "network.proxy"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(check["details"]["mode"], "direct", "{check}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_projects_own_setting_cannot_turn_the_system_proxy_off() -> anyhow::Result<()> {
    let proxy = PageServer::start().await?;
    let stub = ProviderStub::start().await?;
    let mut run = proxied_through(
        "proxy-project",
        &stub,
        &proxy,
        json!({ "network": { "use_system_proxy": false } }),
    )
    .start()
    .await?;
    fetch_through(&mut run, &stub).await?;
    assert!(reached(&proxy), "seen by the proxy: {:?}", proxy.requests());
    run.finish().await;
    Ok(())
}
