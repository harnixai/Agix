//! `model.tasks` with a real model: in the `harnix` scene, which names its
//! sessions, the naming call goes to the model of the tier `model.tasks.title`
//! names and the summary of a compaction to that of `model.tasks.compact`,
//! while every turn stays on the `strong` tier's model.

use std::time::{Duration, Instant};

use blackbox::{LiveRun, Purpose};
use serde_json::{json, Value};

/// A `providers` list of the proxy, its tiers served by these models.
fn providers(run: &LiveRun, strong: &str, balanced: &str, fast: &str) -> Value {
    json!([{
        "id": "proxy",
        "api": "anthropic",
        "base_url": blackbox::ModelEndpoint::base_url(&run.provider).as_str(),
        "api_key_env": "ANTHROPIC_AUTH_TOKEN",
        "tiers": {"strong": strong, "balanced": balanced, "fast": fast},
    }])
}

#[tokio::test(flavor = "multi_thread")]
async fn title_and_compact_calls_go_to_the_models_of_their_tiers() -> anyhow::Result<()> {
    let name = "providers-routed";
    let mut run = LiveRun::builder(name)
        .options(|o| o.scene("harnix"))
        .start()
        .await?;
    let (main, compactor, titler) = (
        run.config().model.clone(),
        run.config().model_at(1).to_string(),
        run.config().model_at(2).to_string(),
    );
    assert!(
        main != compactor && main != titler && compactor != titler,
        "AGIX_LIVE_MODELS needs three Flash names"
    );
    let mut settings = LiveRun::base_settings();
    settings["providers"] = providers(&run, &main, &compactor, &titler);
    settings["model"] = json!({ "tasks": { "compact": "balanced", "title": "fast" } });
    run.world.write_project_settings(name, &settings)?;
    let reloaded = run.client.config_reload(&rpc_client::Target::own()).await?;
    let report = reloaded.result.unwrap_or_default();
    assert_eq!(report["routing"]["router_rebuilt"], true, "{report}");

    let session = run.session().await?;
    run.turn(
        &session,
        "We are planning a database migration for the billing service. Reply with the word \
         NOTED and nothing else.",
    )
    .await?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let named = loop {
        let got = run
            .client
            .session_get(&session)
            .await?
            .result
            .unwrap_or_default();
        if let Some(name) = got["name"].as_str() {
            break name.to_string();
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the session was never named: {got}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    assert!(!named.is_empty());
    run.turn(
        &session,
        "List three risks of such a migration, one line each.",
    )
    .await?;
    let compacted = run
        .client
        .call("session.compact", json!({ "session_id": session }))
        .await?;
    assert!(compacted.error.is_none(), "{compacted:?}");

    let calls = run.provider.calls();
    let of = |purpose: Purpose| -> Vec<String> {
        calls
            .iter()
            .filter(|c| c.purpose() == purpose)
            .map(|c| c.model().to_string())
            .collect()
    };
    let (titles, compactions, turns) =
        (of(Purpose::Title), of(Purpose::Compact), of(Purpose::Turn));
    assert_eq!(titles, std::slice::from_ref(&titler), "naming calls");
    assert_eq!(
        compactions,
        std::slice::from_ref(&compactor),
        "compaction calls"
    );
    assert!(
        !turns.is_empty() && turns.iter().all(|m| *m == main),
        "turns: {turns:?}"
    );
    run.finish_allowing_refusals().await;
    Ok(())
}
