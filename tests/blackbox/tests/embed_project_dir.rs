//! An embedder that names its own project directory (`EmbedConfig.project_dir`
//! = `.codix`), over `Embedded::serve_stream`: every file the engine keeps in
//! the project lands under that name and none under `.agix` — written files,
//! a sub-agent's worktree, an "always, this project" rule, a team, memory, an
//! import, telemetry — and the protection follows the name: the model cannot
//! rewrite `.codix`'s settings or hooks, nor a foreign `.agix`'s, and a
//! worktree's copy of a file the project denies is denied too. A name that
//! cannot be a project directory is refused at start.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use blackbox::{Answer, ProviderStub, Reply};
use daemon::embed::{EmbedConfig, EmbedError, Embedded};
use rpc_client::{DaemonRpcClient, Target, TurnEvents};
use serde_json::{json, Value};

const OWN: &str = ".codix";
const SECRET: &str = "WORKTREE-SECRET-4f2";
const MAIN: &str = "MAIN-MARK";
const SUB: &str = "SUB-MARK";
const MEMBER: &str = "MEMBER-MARK";
const NEXT: &str = "NEXT-MARK";
const PROJECT_MEMORY: &str = "PROJECT-MEMORY-MARK";
const ALWAYS: &str = "echo one > one.txt";

struct Root {
    _dir: tempfile::TempDir,
    data: PathBuf,
    project: PathBuf,
}

fn git(project: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(project)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// A trusted git project with a committed secret the settings deny reading,
/// Cursor rules to import, and a stub provider in the data root.
fn root(stub: &ProviderStub) -> anyhow::Result<Root> {
    let dir = tempfile::tempdir()?;
    let base = dir.path().canonicalize()?;
    let (data, project) = (base.join("data"), base.join("project"));
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(project.join(".cursor/rules"))?;
    std::fs::write(project.join("README.md"), "# project\n")?;
    std::fs::write(project.join("secret.txt"), SECRET)?;
    git(&project, &["init", "-q", "-b", "main"])?;
    git(&project, &["add", "README.md", "secret.txt"])?;
    git(
        &project,
        &[
            "-c",
            "user.name=Agix",
            "-c",
            "user.email=agix@agix.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "initial",
        ],
    )?;
    std::fs::write(project.join(".cursorrules"), "CURSOR-RULE: use tabs\n")?;
    std::fs::write(
        project.join(".cursor/rules/style.mdc"),
        "---\ndescription: style\nalwaysApply: true\n---\nCURSOR-MDC-RULE\n",
    )?;
    let settings = json!({
        "providers": { "stub": {
            "api_type": "anthropic",
            "base_url": stub.base_url().as_str(),
            "api_key": "stub-key",
            "default_model": "claude-sonnet-4-6",
            "models": ["claude-sonnet-4-6"]
        }},
        "default_provider": "stub",
        "permission_mode": "default",
        "memory_enabled": true,
        "team": { "enabled": true },
        "permission_rules": [
            { "tool": "Write", "action": "allow" },
            { "tool": "Agent", "action": "allow" },
            { "tool": "TeamCreate", "action": "allow" },
            { "tool": format!("Read({})", project.join("secret.txt").display()), "action": "deny" }
        ]
    });
    std::fs::write(data.join("settings.json"), serde_json::to_vec(&settings)?)?;
    std::fs::write(
        data.join("settings.local.json"),
        serde_json::to_vec(&json!({ "project_trust": { "trusted": [project] } }))?,
    )?;
    Ok(Root {
        _dir: dir,
        data,
        project,
    })
}

fn config(root: &Root, project_dir: &str) -> EmbedConfig {
    let mut config = EmbedConfig::new(&root.data);
    config.cwd = Some(root.project.clone());
    config.project_dir = project_dir.into();
    config
}

fn connect(embedded: &Embedded) -> DaemonRpcClient {
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    embedded.serve_stream(theirs);
    DaemonRpcClient::with_transport(Box::new(rpc_client::transport::stream(ours, "embed")))
}

/// A second connection on `session` that answers its permission prompts:
/// "always, this project" for the command [`ALWAYS`], once for anything
/// else. What it was asked about, by command.
fn answer_prompts(embedded: &Embedded, session: &str) -> Arc<Mutex<Vec<String>>> {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let (mut watcher, mut answerer) = (connect(embedded), connect(embedded));
    let (session, record) = (session.to_string(), asked.clone());
    tokio::spawn(async move {
        if watcher.session_subscribe(&session).await.is_err() {
            return;
        }
        let mut commands: Vec<(Value, String)> = Vec::new();
        while let Ok(Some(frame)) = watcher.next_frame().await {
            let event = &frame["params"]["event"];
            if frame["method"] != "session.event" {
                continue;
            }
            let event = if event["kind"] == "subagent_progress" {
                &event["event"]
            } else {
                event
            };
            if event["kind"] == "tool_use" {
                let command = event["input"]["command"].as_str().unwrap_or_default();
                commands.push((event["id"].clone(), command.to_string()));
                continue;
            }
            if event["kind"] != "prompt" {
                continue;
            }
            let command = commands
                .iter()
                .rev()
                .find(|(id, _)| *id == event["tool_use_id"])
                .map(|(_, c)| c.clone())
                .unwrap_or_default();
            let decision = if command == ALWAYS {
                Answer::permit_always("local")
            } else {
                Answer::permit()
            };
            record.lock().unwrap().push(command);
            let _ = answerer
                .call(
                    "session.respondToPrompt",
                    json!({
                        "session_id": session,
                        "prompt_id": event["prompt_id"],
                        "prompt_type": "permission",
                        "decision": decision,
                    }),
                )
                .await;
        }
    });
    asked
}

fn routine_replies(stub: &ProviderStub) {
    stub.script_when("标题生成器", (0..8).map(|_| Reply::text("a title")));
    stub.script_when(
        "Return only a JSON array of memories",
        (0..8).map(|_| Reply::text("[]")),
    );
    stub.script_when(
        "You are selecting memories",
        (0..8).map(|_| Reply::text("[]")),
    );
}

fn write(id: &str, path: &Path, content: &str) -> Reply {
    Reply::calls(
        id,
        "Write",
        json!({ "file_path": path.to_string_lossy(), "content": content }),
    )
}

fn bash(id: &str, command: &str) -> Reply {
    Reply::calls(id, "Bash", json!({ "command": command }))
}

fn result_of_write(events: &TurnEvents, path: &Path) -> (String, bool) {
    let path = path.to_string_lossy();
    let at = events
        .tool_uses
        .iter()
        .position(|(name, input)| name == "Write" && input["file_path"] == *path)
        .unwrap_or_else(|| panic!("no Write of {path}: {:?}", events.tool_uses));
    let (_, out, is_error) = events.tool_results[at].clone();
    (out, is_error)
}

fn entries_named(dir: &Path, name: &str, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if entry.file_name() == name {
            found.push(path.clone());
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            entries_named(&path, name, found);
        }
    }
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_project_directory_holds_everything_the_engine_keeps_and_is_guarded(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub)?;
    let (project, own) = (root.project.clone(), root.project.join(OWN));
    let embedded = Embedded::start(config(&root, OWN)).await?;
    let mut client = connect(&embedded);

    let imported = client.import_run(&Target::own(), "cursor").await?;
    assert!(imported.error.is_none(), "{imported:?}");

    routine_replies(&stub);
    let worktree = own.join("worktrees/wt");
    stub.script_when(
        MAIN,
        [
            write("w-file", &project.join("hello.txt"), "hi"),
            bash("b-always", ALWAYS),
            write(
                "w-memory",
                &own.join("memory/MEMORY.md"),
                &format!("- [{PROJECT_MEMORY}](note.md) — the project's\n"),
            ),
            write("w-settings", &own.join("settings.json"), "{}"),
            write("w-foreign", &project.join(".agix/settings.json"), "{}"),
            bash("b-settings", &format!("echo x > {OWN}/settings.json")),
            bash(
                "b-hook",
                &format!("mkdir -p {OWN}/hooks; echo x > {OWN}/hooks/a.sh"),
            ),
            Reply::calls(
                "a-sub",
                "Agent",
                json!({ "prompt": format!("{SUB} work"), "description": "worker", "worktree": "wt" }),
            ),
            Reply::calls(
                "t-team",
                "TeamCreate",
                json!({ "name": "crew", "stages": [{
                    "name": "only",
                    "agents": [{ "label": "one", "prompt": format!("{MEMBER} work") }]
                }] }),
            ),
            Reply::text("MAIN-DONE"),
        ],
    );
    stub.script_when(
        SUB,
        [
            write("s-made", &worktree.join("made.txt"), "made"),
            Reply::calls(
                "s-secret",
                "Read",
                json!({ "file_path": worktree.join("secret.txt").to_string_lossy() }),
            ),
            Reply::text("SUB-DONE"),
        ],
    );
    stub.script_when(MEMBER, [Reply::text("member result")]);

    let session = client.session_create(json!({})).await?;
    let asked = answer_prompts(&embedded, &session);
    let events = client
        .session_run_turn(&session, &format!("{MAIN} go"), "t1", None)
        .await?;
    assert!(events.turn_complete, "{events:?}");

    assert_eq!(read(project.join("hello.txt")), "hi");
    let local = read(own.join("settings.local.json"));
    assert!(local.contains(&format!("Bash({ALWAYS})")), "{local}");
    assert!(
        asked.lock().unwrap().iter().any(|c| c == ALWAYS),
        "{asked:?}"
    );
    for refused in [
        own.join("settings.json"),
        project.join(".agix/settings.json"),
    ] {
        let (out, is_error) = result_of_write(&events, &refused);
        assert!(is_error, "{}: {out}", refused.display());
    }
    assert!(!own.join("settings.json").exists());
    assert!(!own.join("hooks/a.sh").exists());
    assert!(
        worktree.join("made.txt").is_file(),
        "{:?}",
        events.tool_results
    );
    assert!(read(project.join(".gitignore"))
        .lines()
        .any(|l| l == format!("{OWN}/")));
    assert!(
        stub.calls()
            .iter()
            .all(|c| !c.body.to_string().contains(SECRET)),
        "the worktree's copy of a denied file reached the model"
    );
    let teams: Vec<_> = std::fs::read_dir(own.join("teams"))?.flatten().collect();
    assert_eq!(teams.len(), 1, "{teams:?}");
    assert!(own.join(".imported.json").is_file());
    assert!(read(own.join("rules/style.md")).contains("CURSOR-MDC-RULE"));
    assert!(read(project.join("AGENTS.md")).contains(&format!("{OWN}/rules/style.md")));

    stub.script_when(NEXT, [Reply::text("NEXT-DONE")]);
    let next = client.session_create(json!({})).await?;
    client
        .session_run_turn(&next, &format!("{NEXT} hello"), "t2", None)
        .await?;
    let next_call = stub
        .calls()
        .into_iter()
        .find(|c| c.has_tools() && c.messages_contain(NEXT))
        .expect("the next session's turn");
    assert!(next_call.system_contains(PROJECT_MEMORY));

    embedded.shutdown().await;
    let telemetry: Vec<_> = std::fs::read_dir(own.join("telemetry"))?
        .flatten()
        .collect();
    assert!(!telemetry.is_empty());
    let mut stray = Vec::new();
    entries_named(&project, ".agix", &mut stray);
    assert!(stray.is_empty(), "{stray:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_that_cannot_be_a_project_directory_is_refused_at_start() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub)?;
    for name in [
        "", "../x", "a/b", ".git", ".claude", ".Codix", "src", ".github", ".vscode",
    ] {
        match Embedded::start(config(&root, name)).await {
            Err(EmbedError::InvalidProjectDir { name: refused, .. }) => assert_eq!(refused, name),
            Err(other) => panic!("{name:?}: {other}"),
            Ok(_) => panic!("{name:?} was taken"),
        }
    }
    Ok(())
}

/// A sandboxed command writes nothing in a foreign configuration root — the
/// gate takes all of one for configuration, and the sandbox holds it to
/// that: not `.agix/memory`, not `.claude/commands`, not a new entry
/// directly in either — while the instance's own working space stays
/// writable. The prompts are answered yes, so the sandbox alone decides.
#[tokio::test(flavor = "multi_thread")]
async fn a_sandboxed_command_writes_nothing_in_a_foreign_configuration_root() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let root = root(&stub)?;
    let project = root.project.clone();
    for dir in [".agix/memory", ".claude/commands", ".claude/agents"] {
        std::fs::create_dir_all(project.join(dir))?;
    }
    let embedded = Embedded::start(config(&root, OWN)).await?;
    let mut client = connect(&embedded);
    routine_replies(&stub);
    let foreign = [
        ".agix/memory/m.md",
        ".agix/notes.md",
        ".claude/commands/c.md",
        ".claude/agents/a.md",
        ".claude/notes.md",
    ];
    let own_scratch = format!("{OWN}/scratchpad/n.md");
    let mut replies: Vec<Reply> = foreign
        .iter()
        .enumerate()
        .map(|(i, path)| bash(&format!("b-{i}"), &format!("echo x > {path}")))
        .collect();
    replies.push(bash(
        "b-own",
        &format!("mkdir -p {OWN}/scratchpad && echo x > {own_scratch}"),
    ));
    replies.push(Reply::text("FOREIGN-DONE"));
    stub.script_when("FOREIGN-MARK", replies);

    let session = client.session_create(json!({})).await?;
    let _asked = answer_prompts(&embedded, &session);
    let events = client
        .session_run_turn(&session, "FOREIGN-MARK go", "t1", None)
        .await?;
    assert!(events.turn_complete, "{events:?}");
    for path in foreign {
        assert!(
            !project.join(path).exists(),
            "{path} was written: {:?}",
            events.tool_results
        );
    }
    assert_eq!(read(project.join(&own_scratch)), "x\n");
    embedded.shutdown().await;
    Ok(())
}

/// On a filesystem that ignores case, `.CODIX/settings.local.json` is the
/// instance's own key file and `.Claude/SETTINGS.JSON` Claude Code's
/// settings: the write tool refuses every spelling of the engines' settings
/// as it does the usual one, under a rule that allows `Write`, where the
/// directory exists and where it does not.
#[tokio::test(flavor = "multi_thread")]
async fn the_engines_settings_are_refused_in_every_spelling_of_their_names() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub)?;
    let project = root.project.clone();
    std::fs::create_dir_all(project.join(".claude"))?;
    let embedded = Embedded::start(config(&root, OWN)).await?;
    let mut client = connect(&embedded);
    routine_replies(&stub);
    std::fs::create_dir_all(project.join(OWN))?;
    let spellings = [
        ".codix/Settings.json",
        ".CODIX/MCP.local.json",
        ".Agix/settings.json",
        ".Claude/SETTINGS.JSON",
        ".Env",
    ];
    let mut replies: Vec<Reply> = spellings
        .iter()
        .enumerate()
        .map(|(i, path)| write(&format!("w-{i}"), &project.join(path), "{}"))
        .collect();
    replies.push(Reply::text("CASE-DONE"));
    stub.script_when("CASE-MARK", replies);

    let session = client.session_create(json!({})).await?;
    let _asked = answer_prompts(&embedded, &session);
    let events = client
        .session_run_turn(&session, "CASE-MARK go", "t1", None)
        .await?;
    assert!(events.turn_complete, "{events:?}");
    for path in spellings {
        let (out, is_error) = result_of_write(&events, &project.join(path));
        assert!(is_error, "{path} was written: {out}");
        assert!(!project.join(path).exists(), "{path} exists");
    }
    embedded.shutdown().await;
    Ok(())
}

/// APFS folds `ſ` to `s`, so `.codix/ſettings.json` is `.codix/settings.json`
/// there: under a rule that allows `Write`, the write tool refuses the
/// engines' settings in the spellings only case folding equates.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn the_engines_settings_are_refused_in_spellings_the_filesystem_folds_onto_them(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub)?;
    let project = root.project.clone();
    let embedded = Embedded::start(config(&root, OWN)).await?;
    let mut client = connect(&embedded);
    routine_replies(&stub);
    std::fs::create_dir_all(project.join(OWN))?;
    let spellings = [
        ".codix/\u{17F}ettings.json",
        ".Claude/\u{17F}ettings.j\u{17F}on",
    ];
    let mut replies: Vec<Reply> = spellings
        .iter()
        .enumerate()
        .map(|(i, path)| write(&format!("f-{i}"), &project.join(path), "{}"))
        .collect();
    replies.push(Reply::text("FOLD-DONE"));
    stub.script_when("FOLD-MARK", replies);

    let session = client.session_create(json!({})).await?;
    let events = client
        .session_run_turn(&session, "FOLD-MARK go", "t1", None)
        .await?;
    assert!(events.turn_complete, "{events:?}");
    for path in spellings {
        let (out, is_error) = result_of_write(&events, &project.join(path));
        assert!(is_error, "{path} was written: {out}");
    }
    for path in [
        ".agents/skills/pwn",
        ".codix/settings.json",
        ".claude/settings.json",
    ] {
        assert!(!project.join(path).exists(), "{path} exists");
    }
    embedded.shutdown().await;
    Ok(())
}

/// A configuration root that is not there cannot be made by a sandboxed
/// command — not `.claude`, not a foreign `.agix`, not `.agents/skills` —
/// and holding it leaves nothing behind in the project. The prompts are
/// answered yes, so the sandbox alone decides.
#[tokio::test(flavor = "multi_thread")]
async fn a_sandboxed_command_cannot_make_a_configuration_root_that_is_not_there(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub)?;
    let project = root.project.clone();
    let embedded = Embedded::start(config(&root, OWN)).await?;
    let mut client = connect(&embedded);
    routine_replies(&stub);
    let planted = [
        ".claude/settings.json",
        ".agix/settings.json",
        ".agents/skills/s/SKILL.md",
    ];
    let mut replies: Vec<Reply> = planted
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let parent = Path::new(path).parent().expect("a parent").display();
            bash(
                &format!("b-{i}"),
                &format!("mkdir -p {parent} && echo x > {path}"),
            )
        })
        .collect();
    replies.push(Reply::text("ABSENT-DONE"));
    stub.script_when("ABSENT-MARK", replies);

    let session = client.session_create(json!({})).await?;
    let _asked = answer_prompts(&embedded, &session);
    let events = client
        .session_run_turn(&session, "ABSENT-MARK go", "t1", None)
        .await?;
    assert!(events.turn_complete, "{events:?}");
    for dir in [".claude", ".agix", ".agents"] {
        assert!(
            project.join(dir).symlink_metadata().is_err(),
            "{dir} is in the project: {:?}",
            events.tool_results
        );
    }
    embedded.shutdown().await;
    Ok(())
}

/// In the instance's own directory a sandboxed command writes only what the
/// gate leaves out of configuration — `scratchpad/`, `memory/`,
/// `worktrees/` — not the teams, telemetry or any other entry the engine
/// keeps there, which the engine itself still writes. The prompts are
/// answered yes, so the sandbox alone decides.
#[tokio::test(flavor = "multi_thread")]
async fn a_sandboxed_command_writes_only_the_working_space_of_the_own_directory(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub)?;
    let own = root.project.join(OWN);
    for dir in ["teams", "telemetry", "scratchpad", "memory"] {
        std::fs::create_dir_all(own.join(dir))?;
    }
    let embedded = Embedded::start(config(&root, OWN)).await?;
    let mut client = connect(&embedded);
    routine_replies(&stub);
    let refused = [
        "teams/t.json",
        "telemetry/x.jsonl",
        ".imported.json",
        "notes.md",
    ];
    let mut replies: Vec<Reply> = refused
        .iter()
        .enumerate()
        .map(|(i, path)| bash(&format!("b-{i}"), &format!("echo x > {OWN}/{path}")))
        .collect();
    replies.push(bash(
        "b-work",
        &format!("echo x > {OWN}/scratchpad/n.md && echo x > {OWN}/memory/m.md"),
    ));
    replies.push(Reply::text("OWN-DONE"));
    stub.script_when("OWN-MARK", replies);

    let session = client.session_create(json!({})).await?;
    let _asked = answer_prompts(&embedded, &session);
    let events = client
        .session_run_turn(&session, "OWN-MARK go", "t1", None)
        .await?;
    assert!(events.turn_complete, "{events:?}");
    for path in refused {
        assert!(
            !own.join(path).exists(),
            "{OWN}/{path} was written: {:?}",
            events.tool_results
        );
    }
    assert_eq!(read(own.join("scratchpad/n.md")), "x\n");
    assert_eq!(read(own.join("memory/m.md")), "x\n");
    embedded.shutdown().await;
    Ok(())
}
