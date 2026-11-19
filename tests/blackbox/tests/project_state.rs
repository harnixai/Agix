//! The project directory's `state/`, over `Embedded::serve_stream`: a
//! project still holding the engine's local data directly in its project
//! directory has it moved into `state/` when it opens, and its `.gitignore`
//! gains `state/` beside what it already had; the model writes
//! `state/scratchpad/` and `state/memory/` without asking, and in an
//! unattended session nothing else under `state/` — not the saved tool
//! results, not the embedder's own `state/<product id>/`, not the old
//! places. A product whose id is a name the engine keeps in `state/` is
//! refused at start. Where the project directory is the state root itself —
//! the engine started in the directory holding its state root — nothing of
//! the state root is moved.

use std::path::{Path, PathBuf};

use blackbox::{ProviderStub, Reply};
use daemon::embed::{EmbedConfig, EmbedError, Embedded};
use rpc_client::{DaemonRpcClient, TurnEvents};
use serde_json::json;

const MARK: &str = "STATE-MARK";

struct Root {
    _dir: tempfile::TempDir,
    data: PathBuf,
    project: PathBuf,
}

/// A trusted project with the stub provider in the data root.
fn root(stub: &ProviderStub) -> anyhow::Result<Root> {
    let dir = tempfile::tempdir()?;
    let base = dir.path().canonicalize()?;
    let (data, project) = (base.join("data"), base.join("project"));
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(&project)?;
    let settings = json!({
        "providers": { "stub": {
            "api_type": "anthropic",
            "base_url": stub.base_url().as_str(),
            "api_key": "stub-key",
            "default_model": "claude-sonnet-4-6",
            "models": ["claude-sonnet-4-6"]
        }},
        "default_provider": "stub",
        "permissions": {"mode": "default"},
        "memory": { "enabled": false }
    });
    std::fs::write(data.join("settings.json"), serde_json::to_vec(&settings)?)?;
    std::fs::write(
        data.join("settings.local.json"),
        serde_json::to_vec(&json!({ "app": { "trusted_projects": [project] } }))?,
    )?;
    Ok(Root {
        _dir: dir,
        data,
        project,
    })
}

fn connect(embedded: &Embedded) -> DaemonRpcClient {
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    embedded.serve_stream(theirs);
    DaemonRpcClient::with_transport(Box::new(rpc_client::transport::stream(ours, "embed")))
}

fn put(path: &Path, text: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().expect("a parent"))?;
    std::fs::write(path, text)?;
    Ok(())
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn write(id: &str, path: &Path) -> Reply {
    Reply::calls(
        id,
        "Write",
        json!({ "file_path": path.to_string_lossy(), "content": "x" }),
    )
}

fn is_error_of_write(events: &TurnEvents, path: &Path) -> bool {
    let path = path.to_string_lossy();
    let at = events
        .tool_uses
        .iter()
        .position(|(name, input)| name == "Write" && input["file_path"] == *path)
        .unwrap_or_else(|| panic!("no Write of {path}: {:?}", events.tool_uses));
    events.tool_results[at].2
}

/// What an older engine left directly in the project directory `own`.
fn an_old_layout(own: &Path) -> anyhow::Result<()> {
    put(&own.join("memory/old-fact.md"), "OLD-FACT")?;
    put(&own.join("scratchpad/notes.txt"), "OLD-NOTES")?;
    put(&own.join("teams/team-x/team.json"), "{}")?;
    put(&own.join("telemetry/s.jsonl"), "{}\n")?;
    put(&own.join("tool-results/s/r.txt"), "OLD-RESULT")?;
    put(&own.join(".imported.json"), r#"{"decision":"skipped"}"#)?;
    put(&own.join(".gitignore"), "memory/\n")?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_keeps_its_local_data_in_state_and_the_model_writes_only_its_working_space(
) -> anyhow::Result<()> {
    for engine in [None, Some("codix")] {
        let stub = ProviderStub::start().await?;
        let root = root(&stub)?;
        let own_name = format!(".{}", engine.unwrap_or("agix"));
        let own = root.project.join(&own_name);
        let state = own.join("state");
        an_old_layout(&own)?;

        let mut config = EmbedConfig::new(&root.data);
        config.cwd = Some(root.project.clone());
        config.product = "Codix".into();
        if let Some(engine) = engine {
            config.engine = engine.into();
        }
        let embedded = Embedded::start(config).await?;
        let mut client = connect(&embedded);

        for (moved, text) in [
            ("memory/old-fact.md", "OLD-FACT"),
            ("scratchpad/notes.txt", "OLD-NOTES"),
            ("tool-results/s/r.txt", "OLD-RESULT"),
            (".imported.json", r#"{"decision":"skipped"}"#),
        ] {
            assert_eq!(read(state.join(moved)), text, "{own_name}: {moved}");
        }
        assert!(state.join("teams/team-x/team.json").is_file());
        assert!(state.join("telemetry/s.jsonl").is_file());
        for old in [
            "memory",
            "scratchpad",
            "teams",
            "telemetry",
            "tool-results",
            ".imported.json",
        ] {
            assert!(
                own.join(old).symlink_metadata().is_err(),
                "{own_name}/{old} stayed"
            );
        }

        let (scratch, memory) = (state.join("scratchpad/x"), state.join("memory/m.md"));
        let refused = [
            state.join("codix/x"),
            state.join("tool-results/s/forged.txt"),
            state.join("notes.md"),
            own.join("memory/m.md"),
            own.join("scratchpad/x"),
        ];
        let mut replies = vec![write("w-scratch", &scratch), write("w-memory", &memory)];
        replies.extend(
            refused
                .iter()
                .enumerate()
                .map(|(i, path)| write(&format!("w-{i}"), path)),
        );
        replies.push(Reply::text("STATE-DONE"));
        stub.script_when(MARK, replies);
        stub.script_when("标题生成器", (0..4).map(|_| Reply::text("a title")));

        let session = client
            .session_create(json!({ "options": { "attended": false } }))
            .await?;
        let events = client
            .session_run_turn(&session, &format!("{MARK} go"), "t1", None)
            .await?;
        assert!(events.turn_complete, "{events:?}");

        let gitignore = read(own.join(".gitignore"));
        let lines: Vec<&str> = gitignore.lines().collect();
        assert_eq!(lines.first(), Some(&"memory/"), "{gitignore}");
        for line in ["state/", "settings.local.json", "mcp.local.json"] {
            assert!(lines.contains(&line), "{own_name}: {line} in {gitignore}");
        }
        assert_eq!(read(&scratch), "x");
        assert_eq!(read(&memory), "x");
        for path in &refused {
            assert!(
                is_error_of_write(&events, path),
                "{}: {:?}",
                path.display(),
                events.tool_results
            );
            assert!(!path.exists(), "{} was written", path.display());
        }
        embedded.shutdown().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_state_root_stays_where_it_is_when_the_project_directory_is_it() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub)?;
    let home = root.data.parent().expect("a parent").to_path_buf();
    let data = home.join(".agix");
    std::fs::rename(&root.data, &data)?;
    std::fs::write(
        data.join("settings.local.json"),
        serde_json::to_vec(&json!({ "app": { "trusted_projects": [home] } }))?,
    )?;
    put(&data.join("memory/MEMORY.md"), "GLOBAL-FACT")?;
    put(&data.join("scratchpad/sessions/x/f.txt"), "RUNNING")?;
    put(&data.join("tool-results/s/r.txt"), "GLOBAL-RESULT")?;
    put(&data.join(".imported.json"), "{}")?;

    let mut config = EmbedConfig::new(&data);
    config.cwd = Some(home.clone());
    let embedded = Embedded::start(config).await?;
    let mut client = connect(&embedded);
    client
        .session_create(json!({ "project_root": home, "options": { "attended": false } }))
        .await?;

    assert_eq!(read(data.join("memory/MEMORY.md")), "GLOBAL-FACT");
    assert_eq!(read(data.join("scratchpad/sessions/x/f.txt")), "RUNNING");
    assert_eq!(read(data.join("tool-results/s/r.txt")), "GLOBAL-RESULT");
    assert!(data.join(".imported.json").is_file());
    for moved in [
        "memory/MEMORY.md",
        "scratchpad/sessions/x",
        "tool-results/s",
        ".imported.json",
    ] {
        assert!(
            data.join("state").join(moved).symlink_metadata().is_err(),
            "{moved} moved into state/"
        );
    }
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_product_named_like_what_the_engine_keeps_in_state_is_refused_at_start(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    for name in [
        "Memory",
        "scratchpad",
        "Tool-Results",
        "HISTORY",
        "Teams",
        "Imported",
    ] {
        let mut config = EmbedConfig::new(dir.path());
        config.cwd = Some(dir.path().to_path_buf());
        config.product = name.into();
        match Embedded::start(config).await {
            Err(EmbedError::InvalidProductName { name: refused, .. }) => assert_eq!(refused, name),
            Err(other) => panic!("{name:?}: {other}"),
            Ok(_) => panic!("{name:?} was taken"),
        }
    }
    Ok(())
}
