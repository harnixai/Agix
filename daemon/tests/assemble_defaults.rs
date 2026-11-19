//! What `daemon::assemble::pool` does when the caller supplies nothing.
//!
//! Every other daemon test hands the assembly a store, a model and a
//! permission, because a test wants to control those. That leaves the branch
//! `agixd` actually takes — `Transcripts::UnderGlobalRoot` — run by
//! nothing, and it is the branch with the only irreversible side effect in
//! the whole module: relocating an older on-disk layout before the store is
//! opened against the new one.
//!
//! `migrate_layout` has unit tests of its own. That is exactly the shape of
//! bug this repository has been caught by before — every rule tested, no rule
//! reached from the entry point — so these cases go through `assemble::pool`
//! and look at the disk afterwards.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use daemon::config::{load_daemon_config, DaemonConfig, StaticDaemonPaths};
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};

/// A client that is never called: none of these cases runs a turn.
fn offline_client() -> Arc<dyn AnthropicClient> {
    Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("unused".into())).unwrap())
}

fn config_in(dir: &Path) -> DaemonConfig {
    load_daemon_config(
        "claude-sonnet-4-6",
        None,
        None,
        "code",
        &StaticDaemonPaths::new(dir.to_path_buf()),
    )
}

fn assembly() -> daemon::Assembly {
    daemon::Assembly {
        model_client: Some(offline_client()),
        ..Default::default()
    }
}

#[tokio::test]
async fn the_default_assembly_puts_transcripts_under_the_configured_root() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let config = config_in(dir.path());
    let global = config.settings.paths.global_data_dir.clone();

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(cwd.clone()),
            ..assembly()
        },
    )
    .await
    .expect("the daemon assembles");

    let created = pool
        .create_session(None, daemon::session_pool::ProjectSelector::Default, None)
        .await
        .expect("a session");
    let session_id = created.session_id.as_str();

    // A store was built, and it was built under the root the configuration
    // named rather than under the process's own home.
    let listed = pool.list_all(false, None).await;
    assert!(
        listed.iter().any(|s| s.session_id == session_id),
        "the session is not in the listing, so nothing persisted it: {listed:?}",
        listed = listed.iter().map(|s| &s.session_id).collect::<Vec<_>>()
    );
    let projects = global.join("projects");
    assert!(
        transcripts_under(&projects) > 0,
        "no transcript under {}",
        projects.display()
    );
}

/// `Transcripts::Nowhere` is the degraded mode a failed store open lands in.
/// It has to stay reachable and it has to stay distinguishable — a daemon
/// that quietly built a store anyway would be answering session reads that
/// its operator was told it could not.
#[tokio::test]
async fn nowhere_really_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let config = config_in(dir.path());
    let global = config.settings.paths.global_data_dir.clone();

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(cwd),
            transcripts: daemon::Transcripts::Nowhere,
            ..assembly()
        },
    )
    .await
    .expect("the daemon assembles");

    pool.create_session(None, daemon::session_pool::ProjectSelector::Default, None)
        .await
        .expect("a session");

    assert_eq!(
        transcripts_under(&global.join("projects")),
        0,
        "a pool told to keep no transcripts kept one"
    );
}

/// An unknown `--scenes` name fails the assembly rather than starting with
/// fewer scenes than were asked for. A daemon that came up "successfully"
/// serving three of four scenes is a daemon whose clients get
/// `SCENE_NOT_FOUND` at session creation with nothing in the log to explain.
#[tokio::test]
async fn an_unknown_extra_scene_fails_the_assembly() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let outcome = daemon::assemble::pool(
        &config_in(dir.path()),
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(cwd.clone()),
            extra_scenes: vec!["not-a-real-scene".into()],
            ..assembly()
        },
    )
    .await;
    let error = match outcome {
        Ok(_) => panic!("an unknown scene is not a warning; the assembly must fail"),
        Err(e) => e,
    };
    assert!(
        error.to_string().contains("not-a-real-scene"),
        "the failure names the scene that could not be activated: {error}"
    );

    // A known one still activates, so the check is about the name and not
    // about `extra_scenes` being populated at all.
    let pool = daemon::assemble::pool(
        &config_in(dir.path()),
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(cwd),
            extra_scenes: vec!["harnix".into(), "code".into()],
            ..assembly()
        },
    )
    .await
    .expect("harnix is a builtin scene");
    let scenes = pool.list_scenes().await;
    let active: Vec<&str> = scenes
        .iter()
        .filter(|s| s.active)
        .map(|s| s.scene.as_str())
        .collect();
    assert!(
        active.contains(&"harnix") && active.contains(&"code"),
        "both scenes are active: {active:?}"
    );
}

const VAULTED: &str = r#"{"providers": [
    {"id": "vaulted", "api": "anthropic", "tiers": {"strong": "m"}},
    {"id": "compatible", "api": "openai_chat", "base_url": "http://127.0.0.1:9",
     "tiers": {"strong": "m"}}],
    "model": {"order": ["vaulted", "compatible"]}}"#;

/// Whether each provider of the pool's own pair has a key, by id.
async fn keyed(
    host: Option<Arc<dyn base::interface::credentials::CredentialSource>>,
) -> Vec<(String, bool)> {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(dir.path().join("settings.json"), VAULTED).unwrap();
    let pool = daemon::assemble::pool(
        &config_in(dir.path()),
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(cwd.clone()),
            transcripts: daemon::Transcripts::Nowhere,
            credentials: host,
            ..assembly()
        },
    )
    .await
    .expect("a provider without a key does not stop the assembly");
    let view = pool
        .get_providers(Some(&cwd), "code", None, false)
        .await
        .unwrap();
    view.providers
        .into_iter()
        .map(|p| (p.id, p.credential_present))
        .collect()
}

/// A provider whose key the settings do not name: its calls fail with
/// `no_credentials` and go to the next provider, and a host's source can
/// supply it.
#[tokio::test]
async fn a_host_credential_source_supplies_keys_the_settings_leave_out() {
    use base::interface::credentials::{Secret, StaticCredentials};

    assert_eq!(
        keyed(None).await,
        [("vaulted".into(), false), ("compatible".into(), false)]
    );
    let host = Arc::new(StaticCredentials::new([(
        "vaulted".to_string(),
        Secret::new("sk-from-the-vault"),
    )]));
    assert_eq!(
        keyed(Some(host)).await,
        [("vaulted".into(), true), ("compatible".into(), false)]
    );
}

/// A host source that answers a provider with an empty key has not supplied
/// one: no client sends the prompts with an empty key.
#[tokio::test]
async fn an_empty_key_from_the_host_is_no_key() {
    use base::interface::credentials::{Secret, StaticCredentials};

    let host = Arc::new(StaticCredentials::new([
        ("vaulted".to_string(), Secret::new("")),
        ("compatible".to_string(), Secret::new("sk-from-the-vault")),
    ]));
    assert_eq!(
        keyed(Some(host)).await,
        [("vaulted".into(), false), ("compatible".into(), true)]
    );
}

/// Session directories under a project root, however deep the sanitized-cwd
/// naming puts them.
fn transcripts_under(projects: &Path) -> usize {
    let mut found = 0;
    let mut stack: Vec<PathBuf> = vec![projects.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                found += 1;
            }
        }
    }
    found
}
