//! Where a sub-agent's worktree goes, judged from the repository alone: in
//! `.wkts/` at its root, the one line the engine adds to its `.gitignore`,
//! so what the project directory holds can still be committed. The user's
//! global settings name another directory with `app.worktree_dir`; the
//! project's own settings cannot. What the engine writes in the project
//! directory for this machine alone is ignored there.

use std::path::Path;

use blackbox::{Block, Daemon, DaemonOptions, ProviderStub, Reply, World};
use serde_json::json;

fn git(world: &World, repo: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    Ok(std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("HOME", world.home())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()?)
}

fn ignored(world: &World, repo: &Path, path: &str) -> anyhow::Result<bool> {
    Ok(
        git(world, repo, &["check-ignore", "-q", "--no-index", path])?
            .status
            .success(),
    )
}

/// A sub-agent given the worktree `probe` leaves a file in it, so the
/// worktree is kept; the repository is returned as the turn left it.
async fn a_kept_worktree_in(world: &World, name: &str) -> anyhow::Result<std::path::PathBuf> {
    let project = world.git_project(name)?;
    world.trust_project(&project)?;
    world.write_project_settings(
        name,
        &json!({
            "memory": { "enabled": false },
            "app": { "worktree_dir": ".from-the-project" },
            "permissions": {"allow": ["Agent", "Bash"]}
        }),
    )?;
    let provider = ProviderStub::start().await?;
    provider.script_when(
        "PARENT-MARK",
        [
            Reply::Blocks(vec![Block::tool(
                "call-1",
                "Agent",
                json!({
                    "prompt": "make a file",
                    "description": "file maker",
                    "worktree": "probe"
                }),
            )]),
            Reply::text("done"),
        ],
    );
    provider.script([
        Reply::calls("b1", "Bash", json!({ "command": "echo made > made.txt" })),
        Reply::text("SUB-MADE-IT"),
    ]);
    let daemon = Daemon::start(world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({ "project_root": project.to_string_lossy() }))
        .await?;
    let events = client
        .session_run_turn(&session, "PARENT-MARK work apart", "t1", None)
        .await?;
    anyhow::ensure!(events.turn_complete, "{events:?}");
    daemon.stop().await;
    Ok(project.canonicalize()?)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_goes_in_wkts_and_the_project_directory_stays_committable() -> anyhow::Result<()>
{
    let world = World::new()?;
    let repo = a_kept_worktree_in(&world, "wl").await?;

    assert!(repo.join(".wkts/probe/made.txt").is_file());
    assert!(!repo.join(".agix/worktrees").exists());
    assert!(!repo.join(".from-the-project").exists());
    let gitignore = std::fs::read_to_string(repo.join(".gitignore"))?;
    assert!(gitignore.lines().any(|l| l == ".wkts/"), "{gitignore}");
    assert!(!gitignore.contains(".agix"), "{gitignore}");

    assert!(!ignored(&world, &repo, ".agix/settings.json")?);
    assert!(!ignored(&world, &repo, ".agix/agents/reviewer.md")?);
    assert!(ignored(&world, &repo, ".wkts/probe/made.txt")?);
    for local in [
        ".agix/settings.local.json",
        ".agix/mcp.local.json",
        ".agix/state/.imported.json",
        ".agix/state/memory/a-fact.md",
        ".agix/state/scratchpad/notes.txt",
        ".agix/state/teams/team-x/SCRATCHPAD.md",
        ".agix/state/telemetry/s.jsonl",
        ".agix/state/tool-results/s/r.txt",
        ".agix/state/history/s/transcript.jsonl",
        ".agix/state/agix/runs.json",
    ] {
        assert!(ignored(&world, &repo, local)?, "{local} is committable");
    }
    let added = git(&world, &repo, &["add", ".agix/settings.json"])?;
    assert!(added.status.success(), "{added:?}");
    let status = git(&world, &repo, &["status", "--porcelain"])?;
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(status.contains("A  .agix/settings.json"), "{status}");
    assert!(!status.contains(".wkts"), "{status}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_users_global_settings_name_the_worktrees_directory() -> anyhow::Result<()> {
    let world = World::new()?;
    world.write_global_settings(&json!({ "app": { "worktree_dir": ".trees" } }))?;
    let repo = a_kept_worktree_in(&world, "wg").await?;

    assert!(repo.join(".trees/probe/made.txt").is_file());
    assert!(!repo.join(".wkts").exists());
    assert!(!repo.join(".from-the-project").exists());
    let gitignore = std::fs::read_to_string(repo.join(".gitignore"))?;
    assert!(gitignore.lines().any(|l| l == ".trees/"), "{gitignore}");
    assert!(
        !gitignore.contains(".wkts") && !gitignore.contains(".agix"),
        "{gitignore}"
    );
    assert!(ignored(&world, &repo, ".trees/probe/made.txt")?);
    assert!(!ignored(&world, &repo, ".agix/settings.json")?);
    Ok(())
}
