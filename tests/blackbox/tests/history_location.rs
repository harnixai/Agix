//! Where a session's history is kept, seen from outside: with `app.history`
//! set to `project`, a trusted project's sessions go to its
//! `.agix/state/history/<id>/` (a worktree's to its main checkout's) with only
//! an index under the global `sessions/<id>/`; they come back after a restart
//! and after the project moved; a session planted there without an index is
//! not anyone's; an untrusted project, the `global` setting and sessions kept
//! globally before behave as they always did; history in a project that lost
//! its trust is refused, saying so.

use std::path::{Path, PathBuf};

use blackbox::{Daemon, DaemonOptions, ProviderStub, Reply, World};
use serde_json::{json, Value};

const PROJECT_UNTRUSTED: i32 = -32022;

fn keep_in_projects(world: &World) -> anyhow::Result<()> {
    world.write_global_settings(&json!({
        "memory": { "enabled": false },
        "app": { "history": "project" }
    }))
}

fn keep_globally(world: &World) -> anyhow::Result<()> {
    world.write_global_settings(&json!({ "memory": { "enabled": false } }))
}

fn kept_in(project: &Path, session: &str) -> PathBuf {
    project
        .canonicalize()
        .unwrap_or_else(|_| project.to_path_buf())
        .join(".agix/state/history")
        .join(session)
}

fn global_entry(world: &World, session: &str) -> PathBuf {
    world.global_root().join("sessions").join(session)
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A session of `project` with one finished turn, on a daemon serving it.
async fn one_turn(
    world: &World,
    provider: &ProviderStub,
    project: &Path,
) -> anyhow::Result<(Daemon, String)> {
    let daemon = Daemon::start(world, provider, DaemonOptions::new(project.to_path_buf())).await?;
    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    provider.script([Reply::text("noted")]);
    let events = client
        .session_run_turn(&session, "remember the plan", "t1", None)
        .await?;
    anyhow::ensure!(events.turn_complete, "{events:?}");
    Ok((daemon, session))
}

async fn listed(daemon: &Daemon) -> anyhow::Result<Vec<Value>> {
    let mut client = daemon.connect().await?;
    let resp = client.call("session.list", json!({})).await?;
    anyhow::ensure!(resp.error.is_none(), "{:?}", resp.error);
    Ok(resp.result.unwrap_or_default()["sessions"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

fn lists(sessions: &[Value], session: &str) -> bool {
    sessions.iter().any(|s| s["session_id"] == session)
}

async fn resume(daemon: &Daemon, session: &str) -> anyhow::Result<daemon::rpc::RpcResponse> {
    let mut client = daemon.connect().await?;
    client
        .call("session.resume", json!({ "session_id": session }))
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trusted_projects_session_is_kept_in_it_and_resumes_after_a_restart() -> anyhow::Result<()>
{
    let world = World::new()?;
    let project = world.project("kept")?;
    keep_in_projects(&world)?;
    world.trust_project(&project)?;
    let provider = ProviderStub::start().await?;
    let (daemon, session) = one_turn(&world, &provider, &project).await?;

    let dir = kept_in(&project, &session);
    assert!(
        dir.join(format!("{session}.jsonl")).is_file(),
        "{}",
        dir.display()
    );
    assert!(dir.join("metadata.json").is_file());
    for name in [".gitignore", ".ignore"] {
        assert_eq!(std::fs::read_to_string(kept_in(&project, name))?, "*\n");
    }
    let global = names_in(&global_entry(&world, &session));
    assert!(global.contains(&"index.json".to_string()), "{global:?}");
    assert!(!global.contains(&"metadata.json".to_string()), "{global:?}");
    assert!(!world
        .transcript_dir(&project)
        .join(format!("{session}.jsonl"))
        .exists());
    let index: Value = serde_json::from_str(&std::fs::read_to_string(
        global_entry(&world, &session).join("index.json"),
    )?)?;
    assert_eq!(index["dir"], json!(dir));

    let mut client = daemon.connect().await?;
    let inside = kept_in(&project, "x.zip");
    let refused = client
        .call(
            "session.export",
            json!({ "session_id": session, "path": inside.to_string_lossy() }),
        )
        .await?;
    assert!(
        refused.error.is_some(),
        "an export into the history: {refused:?}"
    );
    #[cfg(unix)]
    {
        let link = world.root().join("into-history");
        std::os::unix::fs::symlink(kept_in(&project, ""), &link)?;
        let refused = client
            .call(
                "session.export",
                json!({ "session_id": session, "path": link.join("x.zip").to_string_lossy() }),
            )
            .await?;
        assert!(
            refused.error.is_some(),
            "an export into the history through a link: {refused:?}"
        );
    }
    let archive = world.root().join("out.zip");
    let exported = client
        .call(
            "session.export",
            json!({ "session_id": session, "path": archive.to_string_lossy() }),
        )
        .await?;
    assert!(exported.error.is_none(), "{exported:?}");
    daemon.stop().await;

    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    assert!(lists(&listed(&daemon).await?, &session));
    let resumed = resume(&daemon, &session).await?;
    assert!(resumed.error.is_none(), "{resumed:?}");
    assert!(
        resumed.result.unwrap_or_default()["message_count"].as_u64() >= Some(2),
        "the conversation came back empty"
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_moved_whole_is_found_again_where_it_is_opened() -> anyhow::Result<()> {
    let world = World::new()?;
    let old = world.project("before")?;
    keep_in_projects(&world)?;
    world.trust_project(&old)?;
    let provider = ProviderStub::start().await?;
    let (daemon, session) = one_turn(&world, &provider, &old).await?;
    daemon.stop().await;

    let new = old.with_file_name("after");
    std::fs::rename(&old, &new)?;
    world.trust_project(&new)?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(new.clone())).await?;
    let sessions = listed(&daemon).await?;
    let entry = sessions
        .iter()
        .find(|s| s["session_id"] == session.as_str())
        .ok_or_else(|| anyhow::anyhow!("the moved session is not listed: {sessions:?}"))?;
    assert_eq!(entry["project_root"], json!(new.canonicalize()?), "{entry}");
    let resumed = resume(&daemon, &session).await?;
    assert!(resumed.error.is_none(), "{resumed:?}");
    let index: Value = serde_json::from_str(&std::fs::read_to_string(
        global_entry(&world, &session).join("index.json"),
    )?)?;
    assert_eq!(index["project_root"], json!(new.canonicalize()?));
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worktrees_session_is_kept_in_the_main_checkout() -> anyhow::Result<()> {
    let world = World::new()?;
    let main = world.project("repo")?;
    let worktree = main.join(".wkts/feature");
    std::fs::create_dir_all(&worktree)?;
    keep_in_projects(&world)?;
    world.trust_project(&main)?;
    world.trust_project(&worktree)?;
    let provider = ProviderStub::start().await?;
    let (daemon, session) = one_turn(&world, &provider, &worktree).await?;

    assert!(kept_in(&main, &session)
        .join(format!("{session}.jsonl"))
        .is_file());
    assert!(!kept_in(&worktree, &session).exists());
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_planted_in_the_project_without_an_index_is_not_anyones() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("planted")?;
    keep_in_projects(&world)?;
    world.trust_project(&project)?;
    let provider = ProviderStub::start().await?;
    let (daemon, session) = one_turn(&world, &provider, &project).await?;
    daemon.stop().await;

    let planted = base::session::SessionId::new().to_string();
    let to = kept_in(&project, &planted);
    std::fs::create_dir_all(&to)?;
    std::fs::copy(
        kept_in(&project, &session).join(format!("{session}.jsonl")),
        to.join(format!("{planted}.jsonl")),
    )?;

    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let sessions = listed(&daemon).await?;
    assert!(lists(&sessions, &session));
    assert!(!lists(&sessions, &planted), "{sessions:?}");
    let resumed = resume(&daemon, &planted).await?;
    assert!(resumed.error.is_some(), "{resumed:?}");
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_id_whose_directory_was_planted_does_not_take_in_what_is_there() -> anyhow::Result<()>
{
    let world = World::new()?;
    let project = world.project("planted-id")?;
    keep_in_projects(&world)?;
    world.trust_project(&project)?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let donor = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    provider.script([Reply::text("ok")]);
    client
        .session_run_turn(&donor, "PLANTED-WORDS", "t1", None)
        .await?;

    let planted = base::session::SessionId::new().to_string();
    let to = kept_in(&project, &planted);
    std::fs::create_dir_all(&to)?;
    std::fs::copy(
        kept_in(&project, &donor).join(format!("{donor}.jsonl")),
        to.join(format!("{planted}.jsonl")),
    )?;
    let taken = client
        .session_create(json!({
            "project_root": project.to_string_lossy(),
            "session_id": planted,
        }))
        .await?;
    assert_eq!(taken, planted);
    provider.script([Reply::text("ok")]);
    let events = client
        .session_run_turn(&planted, "real words", "t2", None)
        .await?;
    anyhow::ensure!(events.turn_complete, "{events:?}");
    daemon.stop().await;

    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let history = client
        .call("session.history", json!({ "session_id": planted }))
        .await?;
    let text = serde_json::to_string(&history)?;
    assert!(text.contains("real words"), "{text}");
    assert!(!text.contains("PLANTED-WORDS"), "{text}");
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_session_of_a_project_that_lost_its_trust_takes_its_sidechains_too(
) -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("cascade")?;
    keep_in_projects(&world)?;
    world.trust_project(&project)?;
    world.write_project_settings(
        "cascade",
        &json!({
            "memory": { "enabled": false },
            "permissions": {"allow": ["Agent"]}
        }),
    )?;
    let provider = ProviderStub::start().await?;
    provider.script_when(
        "PARENT-MARK",
        [
            Reply::calls(
                "a1",
                "Agent",
                json!({ "description": "helper", "prompt": "say the word" }),
            ),
            Reply::text("done"),
        ],
    );
    provider.script([Reply::text("the word")]);
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let parent = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    let events = client
        .session_run_turn(&parent, "PARENT-MARK delegate", "t1", None)
        .await?;
    anyhow::ensure!(events.turn_complete, "{events:?}");
    daemon.stop().await;
    let history = kept_in(&project, "");
    let children: Vec<String> = names_in(&history)
        .into_iter()
        .filter(|name| *name != parent && !name.starts_with('.'))
        .collect();
    anyhow::ensure!(children.len() == 1, "{:?}", names_in(&history));
    let child = &children[0];

    std::fs::remove_file(world.global_root().join("settings.local.json"))?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let dry = client
        .call(
            "session.delete",
            json!({ "session_id": parent, "dry_run": true }),
        )
        .await?;
    assert_eq!(
        dry.result.unwrap_or_default()["sidechains_deleted"],
        json!(1),
        "{:?}",
        dry.error
    );
    let deleted = client
        .call("session.delete", json!({ "session_id": parent }))
        .await?;
    assert!(deleted.error.is_none(), "{deleted:?}");
    assert_eq!(
        deleted.result.unwrap_or_default()["sidechains_deleted"],
        json!(1)
    );
    assert!(!kept_in(&project, child).exists());
    assert!(!global_entry(&world, child).exists());
    daemon.stop().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_state_directory_linked_into_the_tracked_tree_keeps_history_globally(
) -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("linked-state")?;
    let tracked = project.join("docs/log");
    std::fs::create_dir_all(&tracked)?;
    std::fs::create_dir_all(project.join(".agix"))?;
    std::os::unix::fs::symlink("../docs/log", project.join(".agix/state"))?;
    keep_in_projects(&world)?;
    world.trust_project(&project)?;
    let provider = ProviderStub::start().await?;
    let (daemon, session) = one_turn(&world, &provider, &project).await?;

    assert!(world
        .transcript_dir(&project)
        .join(format!("{session}.jsonl"))
        .is_file());
    assert!(
        !tracked.join("history").exists(),
        "{:?}",
        names_in(&tracked)
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_project_or_the_global_setting_keeps_history_globally() -> anyhow::Result<()> {
    let world = World::new()?;
    let provider = ProviderStub::start().await?;

    let untrusted = world.project("untrusted")?;
    keep_in_projects(&world)?;
    let (daemon, session) = one_turn(&world, &provider, &untrusted).await?;
    assert!(world
        .transcript_dir(&untrusted)
        .join(format!("{session}.jsonl"))
        .is_file());
    assert!(!kept_in(&untrusted, &session).exists());
    daemon.stop().await;

    let trusted = world.project("trusted")?;
    keep_globally(&world)?;
    world.trust_project(&trusted)?;
    let (daemon, session) = one_turn(&world, &provider, &trusted).await?;
    assert!(world
        .transcript_dir(&trusted)
        .join(format!("{session}.jsonl"))
        .is_file());
    assert!(global_entry(&world, &session)
        .join("metadata.json")
        .is_file());
    assert!(!kept_in(&trusted, &session).exists());
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_kept_globally_before_is_read_where_it_is() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("older")?;
    keep_globally(&world)?;
    world.trust_project(&project)?;
    let provider = ProviderStub::start().await?;
    let (daemon, older) = one_turn(&world, &provider, &project).await?;
    daemon.stop().await;

    keep_in_projects(&world)?;
    let (daemon, newer) = one_turn(&world, &provider, &project).await?;
    let sessions = listed(&daemon).await?;
    assert!(
        lists(&sessions, &older) && lists(&sessions, &newer),
        "{sessions:?}"
    );
    let resumed = resume(&daemon, &older).await?;
    assert!(resumed.error.is_none(), "{resumed:?}");
    assert!(world
        .transcript_dir(&project)
        .join(format!("{older}.jsonl"))
        .is_file());
    assert!(kept_in(&project, &newer).is_dir());
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn history_in_a_project_that_lost_its_trust_is_refused_saying_why() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("revoked")?;
    keep_in_projects(&world)?;
    world.trust_project(&project)?;
    let provider = ProviderStub::start().await?;
    let (daemon, session) = one_turn(&world, &provider, &project).await?;
    daemon.stop().await;

    std::fs::remove_file(world.global_root().join("settings.local.json"))?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    assert!(!lists(&listed(&daemon).await?, &session));
    let resumed = resume(&daemon, &session).await?;
    let error = resumed
        .error
        .ok_or_else(|| anyhow::anyhow!("an untrusted project's history was read"))?;
    assert_eq!(error.code, PROJECT_UNTRUSTED, "{error:?}");
    assert!(error.message.contains("not trusted"), "{error:?}");
    assert!(
        kept_in(&project, &session).is_dir(),
        "refusing deletes nothing"
    );
    daemon.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn naming_a_session_of_a_project_that_lost_its_trust_is_refused_with_its_own_code(
) -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("revoked-by-id")?;
    keep_in_projects(&world)?;
    world.trust_project(&project)?;
    let provider = ProviderStub::start().await?;
    let (daemon, session) = one_turn(&world, &provider, &project).await?;
    daemon.stop().await;

    std::fs::remove_file(world.global_root().join("settings.local.json"))?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let created = client
        .call(
            "session.create",
            json!({ "project_root": project.to_string_lossy(), "session_id": session }),
        )
        .await?;
    let error = created
        .error
        .ok_or_else(|| anyhow::anyhow!("created over an untrusted project's session"))?;
    assert_eq!(error.code, PROJECT_UNTRUSTED, "{error:?}");

    let turn = client
        .session_run_turn(&session, "go on", "t2", None)
        .await?;
    let error = turn
        .response
        .and_then(|r| r.error)
        .ok_or_else(|| anyhow::anyhow!("ran a turn on an untrusted project's session"))?;
    assert_eq!(error.code, PROJECT_UNTRUSTED, "{error:?}");
    daemon.stop().await;
    Ok(())
}

/// A session log planted in an untrusted project's history directory — one
/// whose recorded spec gives the session its own prompt — is nobody's: it is
/// not listed, not resumed, not read, and a session created under its id
/// starts empty with the project's own prompt.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_planted_in_an_untrusted_projects_history_is_not_read() -> anyhow::Result<()> {
    let world = World::new()?;
    keep_in_projects(&world)?;
    let donor = world.project("donor")?;
    world.trust_project(&donor)?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(donor.clone())).await?;
    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({
            "project_root": donor.to_string_lossy(),
            "options": {
                "auto_name": false,
                "settings": { "prompt": { "override": "PLANTED-OVERRIDE" } }
            }
        }))
        .await?;
    provider.script([Reply::text("ok")]);
    let events = client
        .session_run_turn(&session, "PLANTED-WORDS", "t1", None)
        .await?;
    anyhow::ensure!(events.turn_complete, "{events:?}");
    daemon.stop().await;

    let crafted = world.project("crafted")?;
    let planted = base::session::SessionId::new().to_string();
    let to = kept_in(&crafted, &planted);
    std::fs::create_dir_all(&to)?;
    let from = kept_in(&donor, &session);
    std::fs::copy(
        from.join(format!("{session}.jsonl")),
        to.join(format!("{planted}.jsonl")),
    )?;
    std::fs::copy(from.join("metadata.json"), to.join("metadata.json"))?;

    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(crafted.clone())).await?;
    let mut client = daemon.connect().await?;
    let in_project = client
        .call(
            "session.list",
            json!({ "project_root": crafted.to_string_lossy() }),
        )
        .await?;
    assert!(
        !lists(
            in_project.result.unwrap_or_default()["sessions"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default(),
            &planted
        ),
        "the planted session was listed"
    );
    assert!(!lists(&listed(&daemon).await?, &planted));
    let resumed = resume(&daemon, &planted).await?;
    assert!(resumed.error.is_some(), "{resumed:?}");
    let history = client
        .call("session.history", json!({ "session_id": planted }))
        .await?;
    assert!(
        !serde_json::to_string(&history)?.contains("PLANTED-WORDS"),
        "{history:?}"
    );

    let taken = client
        .session_create(json!({
            "project_root": crafted.to_string_lossy(),
            "session_id": planted,
            "options": { "auto_name": false },
        }))
        .await?;
    assert_eq!(taken, planted);
    provider.script([Reply::text("fresh")]);
    let events = client
        .session_run_turn(&planted, "real words", "t2", None)
        .await?;
    anyhow::ensure!(events.turn_complete, "{events:?}");
    let call = provider
        .calls()
        .into_iter()
        .find(|c| c.messages_contain("real words"))
        .expect("the turn's model call");
    let system = call.system_text();
    assert!(!system.contains("PLANTED-OVERRIDE"), "{system}");
    assert!(!call.messages_contain("PLANTED-WORDS"));
    daemon.stop().await;
    Ok(())
}
