//! `config.setProvider` choosing its tier and trying a write without making
//! it, seen through the daemon: a global write waits for whoever holds the
//! global settings directory, a dry run leaves every settings file as it was
//! and refuses a provider that would not route, and an untrusted project's
//! sessions are served by a provider written to the global tier.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply};
use rpc_client::Target;
use serde_json::{json, Value};

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

fn provider_at(base_url: &str, api_type: &str) -> Value {
    json!({
        "api_type": api_type,
        "base_url": base_url,
        "api_key": "sk-tier-test",
        "default_model": "claude-sonnet-4-6",
        "models": ["claude-sonnet-4-6"]
    })
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// Every file directly in `dirs`, with its bytes.
fn snapshot(dirs: &[PathBuf]) -> BTreeMap<PathBuf, Vec<u8>> {
    dirs.iter()
        .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten().flatten())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .map(|entry| {
            let bytes = std::fs::read(entry.path()).unwrap_or_default();
            (entry.path(), bytes)
        })
        .collect()
}

/// An exclusive `flock` on a directory, as another process writing that
/// settings tier would hold it.
struct HeldDir {
    _file: std::fs::File,
}

impl HeldDir {
    fn hold(dir: &Path) -> anyhow::Result<Self> {
        let file = std::fs::File::open(dir)?;
        // SAFETY: `flock` only reads the descriptor, which `file` keeps open.
        let rc = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) };
        anyhow::ensure!(rc == 0, "flock: {}", std::io::Error::last_os_error());
        Ok(Self { _file: file })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_global_provider_is_written_under_the_global_tiers_lock() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let run = stubbed("tier-global-lock", &stub).start().await?;
    let global = run.world.global_root();
    let project_settings = run.project.join(".agix").join("settings.json");
    let project_before = std::fs::read(&project_settings)?;

    let held = HeldDir::hold(&global)?;
    let mut other = run.daemon.connect().await?;
    let target = Target::project(&run.project);
    let config = provider_at(stub.base_url().as_str(), "anthropic");
    let write = tokio::spawn(async move {
        other
            .config_set_provider(
                &target,
                json!({
                    "tier": "global",
                    "provider_id": "shared",
                    "config": config,
                    "default_provider": "shared"
                }),
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        read_json(&global.join("settings.json"))["providers"]["shared"].is_null(),
        "the global settings were written while another holder had the directory"
    );
    assert!(
        !write.is_finished(),
        "config.setProvider answered while the global tier was held"
    );
    drop(held);

    let saved = write.await??;
    let saved = saved.result.expect("setProvider result");
    assert_eq!(saved["tier"], "global", "{saved}");
    assert_eq!(saved["dry_run"], false, "{saved}");
    assert_eq!(
        canonical(saved["written_to"].as_str().unwrap_or_default()),
        canonical(&global.join("settings.json").to_string_lossy()),
        "{saved}"
    );
    assert_eq!(
        canonical(saved["secrets_written_to"].as_str().unwrap_or_default()),
        canonical(&global.join("settings.local.json").to_string_lossy()),
        "{saved}"
    );
    assert_eq!(saved["routing"]["router_rebuilt"], true, "{saved}");

    let shared = read_json(&global.join("settings.json"));
    assert_eq!(shared["providers"]["shared"]["api_type"], "anthropic");
    assert!(
        shared["providers"]["shared"]["api_key"].is_null(),
        "the key went into the committed file: {shared}"
    );
    let local = read_json(&global.join("settings.local.json"));
    assert_eq!(local["providers"]["shared"]["api_key"], "sk-tier-test");
    assert!(
        local["project_trust"]["trusted"]
            .as_array()
            .is_some_and(|t| !t.is_empty()),
        "writing the key dropped what the global settings.local.json already held: {local}"
    );
    assert_eq!(
        std::fs::read(&project_settings)?,
        project_before,
        "a global write touched the project tier"
    );

    let mut run = run;
    let seen = run
        .client
        .config_get_provider(&Target::project(&run.project), false)
        .await?;
    let seen = seen.result.expect("getProvider result");
    assert_eq!(seen["default_provider"], "shared", "{seen}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_global_tier_held_past_the_wait_is_refused_and_written_after_release(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("tier-global-locked", &stub).start().await?;
    let global = run.world.global_root();
    let tiers = [global.clone(), run.project.join(".agix")];
    let target = Target::project(&run.project);
    let params = json!({
        "tier": "global",
        "provider_id": "shared",
        "config": provider_at(stub.base_url().as_str(), "anthropic"),
        "default_provider": "shared"
    });

    let held = HeldDir::hold(&global)?;
    let before = snapshot(&tiers);
    let refused = run
        .client
        .config_set_provider(&target, params.clone())
        .await?;
    let error = refused
        .error
        .expect("a global tier held by another process is refused, not waited on forever");
    assert_eq!(error.code, -32026, "{}", error.message);
    assert!(
        error.message.contains("locked by another process")
            && error.message.contains("nothing was written"),
        "{}",
        error.message
    );
    assert_eq!(snapshot(&tiers), before, "the refused write changed a file");
    drop(held);

    let saved = run.client.config_set_provider(&target, params).await?;
    let saved = saved.result.expect("setProvider result after release");
    assert_eq!(saved["tier"], "global", "{saved}");
    assert_eq!(
        read_json(&global.join("settings.json"))["providers"]["shared"]["api_type"],
        "anthropic"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_writes_nothing_and_refuses_what_would_not_route() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("tier-dry-run", &stub).start().await?;
    let global = run.world.global_root();
    let tiers = [global.clone(), run.project.join(".agix")];
    let before = snapshot(&tiers);
    let target = Target::project(&run.project);

    let refused = run
        .client
        .config_set_provider(
            &target,
            json!({
                "tier": "global",
                "dry_run": true,
                "provider_id": "broken",
                "config": provider_at(stub.base_url().as_str(), "anthropic-typo"),
                "default_provider": "broken"
            }),
        )
        .await?;
    let error = refused
        .error
        .expect("a dry run of an api_type nothing builds is refused");
    assert_eq!(error.code, -32602, "{}", error.message);
    assert!(
        error.message.contains("anthropic-typo"),
        "{}",
        error.message
    );
    assert_eq!(snapshot(&tiers), before, "the refused dry run wrote a file");

    for tier in ["global", "project"] {
        let tried = run
            .client
            .config_set_provider(
                &target,
                json!({
                    "tier": tier,
                    "dry_run": true,
                    "provider_id": "fine",
                    "config": provider_at(stub.base_url().as_str(), "anthropic"),
                    "default_provider": "fine"
                }),
            )
            .await?;
        let tried = tried
            .result
            .expect("a dry run of a working provider answers");
        assert_eq!(tried["dry_run"], true, "{tried}");
        assert_eq!(tried["tier"], tier, "{tried}");
        assert_eq!(tried["routing"]["router_rebuilt"], true, "{tried}");
        assert_eq!(tried["default_provider"], "fine", "{tried}");
        let dir = if tier == "global" {
            global.clone()
        } else {
            run.project.join(".agix")
        };
        assert_eq!(
            canonical(tried["secrets_written_to"].as_str().unwrap_or_default()),
            canonical(&dir.join("settings.local.json").to_string_lossy()),
            "{tried}"
        );
        assert_eq!(snapshot(&tiers), before, "the {tier} dry run wrote a file");
    }

    let seen = run.client.config_get_provider(&target, false).await?;
    let seen = seen.result.expect("getProvider result");
    assert!(
        seen["providers"]["fine"].is_null(),
        "a dry run reached the live configuration: {seen}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_project_is_served_by_a_provider_written_to_the_global_tier(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let global_stub = ProviderStub::start().await?;
    let mut run = stubbed("tier-untrusted", &stub).untrusted().start().await?;
    let target = Target::project(&run.project);
    let config = provider_at(global_stub.base_url().as_str(), "anthropic");

    let refused = run
        .client
        .config_set_provider(
            &target,
            json!({ "provider_id": "shared", "config": config, "default_provider": "shared" }),
        )
        .await?;
    let error = refused
        .error
        .expect("the project tier of an untrusted project");
    assert_eq!(error.code, -32022, "{}", error.message);

    let saved = run
        .client
        .config_set_provider(
            &target,
            json!({
                "tier": "global",
                "provider_id": "shared",
                "config": config,
                "default_provider": "shared"
            }),
        )
        .await?;
    let saved = saved.result.expect("the global tier needs no trust");
    assert_eq!(saved["routing"]["router_rebuilt"], true, "{saved}");

    global_stub.script([
        Reply::text("served by the global provider"),
        Reply::text("served by the global provider"),
        Reply::text("served by the global provider"),
    ]);
    let session = run.session().await?;
    run.turn(&session, "hello").await?;
    assert!(
        global_stub
            .calls()
            .iter()
            .any(|c| c.purpose() == Purpose::Turn),
        "the untrusted project's turn did not go to the provider written to the global tier"
    );
    run.finish().await;
    Ok(())
}
