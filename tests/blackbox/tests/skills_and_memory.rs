//! Skills and memory through the daemon, against a stub model: a `/name`
//! command expands a project skill the way the `Skill` tool does, a skill
//! added mid-session is announced on the session's next turn, and a memory
//! filed under the project is in the prompt's index next to the global ones.

use std::time::{Duration, Instant};

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply, SeenRequest};
use serde_json::{json, Value};

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .settings(json!({ "permissions": {"mode": "default"} }))
}

fn skill(name: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: the {name} skill\n---\n{body}\n")
}

fn turn_calls(stub: &ProviderStub) -> Vec<SeenRequest> {
    stub.calls()
        .into_iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slash_command_runs_its_skills_inline_commands_without_the_frontmatter(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("slash-skill", &stub)
        .before_start(|_, project| {
            let dir = project.join(".agents/skills/stamp");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join("SKILL.md"),
                skill("stamp", "Stamp release $ARGUMENTS.\n!`echo inline-ran`"),
            )?;
            Ok(())
        })
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([Reply::text("stamped")]);
    run.turn(&session, "/stamp v1").await?;
    let first = turn_calls(&stub).remove(0);
    assert!(
        first.messages_contain("Stamp release v1."),
        "{}",
        first.messages_text()
    );
    assert!(
        first.messages_contain("inline-ran"),
        "{}",
        first.messages_text()
    );
    assert!(
        !first.messages_contain("!`echo"),
        "{}",
        first.messages_text()
    );
    assert!(
        !first.messages_contain("description: the stamp skill"),
        "{}",
        first.messages_text()
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skill_added_between_turns_is_announced_on_the_next() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("late-skill-stub", &stub)
        .before_start(|_, project| {
            let dir = project.join(".agents/skills/early");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("SKILL.md"), skill("early", "Reply EARLY."))?;
            Ok(())
        })
        .start()
        .await?;
    let session = run.session().await?;
    let watcher = run.watch(&session, false).await?;
    stub.script([Reply::text("one")]);
    run.turn(&session, "hello").await?;

    run.write_file(
        ".agents/skills/latecomer/SKILL.md",
        &skill("latecomer", "Reply LATE."),
    )?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let listed = run
            .client
            .call("commands.list", serde_json::Value::Null)
            .await?;
        if listed
            .result
            .unwrap_or_default()
            .to_string()
            .contains("latecomer")
        {
            break;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the daemon never saw the new skill"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stub.script([Reply::text("two")]);
    run.turn(&session, "again").await?;

    let changed = watcher.events("skills_changed");
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert_eq!(
        changed[0]["event"]["added"],
        json!(["latecomer"]),
        "{changed:?}"
    );
    let second = turn_calls(&stub).remove(1);
    assert!(second.messages_contain("\"latecomer\" was just added"));
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_memory_is_indexed_with_the_global_ones() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("memory-tiers", &stub)
        .settings(json!({ "permissions": {"mode": "default"}, "memory_enabled": true }))
        .before_start(|world, project| {
            for (dir, name) in [
                (world.global_memory_dir(), "GLOBAL-TIER-MEMORY"),
                (world.memory_dir(project), "PROJECT-TIER-MEMORY"),
            ] {
                std::fs::create_dir_all(&dir)?;
                std::fs::write(
                    dir.join("MEMORY.md"),
                    format!("- [{name}]({name}.md) — one of the tiers\n"),
                )?;
            }
            Ok(())
        })
        .start()
        .await?;
    let session = run.session().await?;
    stub.script([Reply::text("ok"), Reply::text("[]")]);
    run.turn(&session, "hello").await?;
    let first = turn_calls(&stub).remove(0);
    assert!(first.system_contains("GLOBAL-TIER-MEMORY"));
    assert!(first.system_contains("PROJECT-TIER-MEMORY"));
    assert!(first.system_contains(&run.world.global_memory_dir().to_string_lossy()));
    run.finish().await;
    Ok(())
}

/// A forked skill's `allowed-tools` are the fork's own turn grants, as they
/// are the turn's when the skill runs inline: its granted command runs without
/// asking, the parent is still asked about that command once the fork is
/// done, and a managed `ask` is asked about inside the fork all the same.
#[tokio::test(flavor = "multi_thread")]
async fn a_forked_skills_allowed_tools_are_granted_in_the_fork_only() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let managed = tempfile::tempdir()?;
    let managed_file = managed.path().join("managed-settings.json");
    std::fs::write(
        &managed_file,
        serde_json::to_string(&json!({
            "permissions": {"ask": ["Bash(echo managed > managed.txt)"]}
        }))?,
    )?;
    let managed_arg = managed_file.to_string_lossy().to_string();
    let mut run = stubbed("fork-grants", &stub)
        .settings(json!({
            "permissions": {"mode": "default", "allow": ["Skill"]}
        }))
        .mode(blackbox::Mode::Spawned)
        .options(move |o| o.arg("--managed-settings").arg(managed_arg.clone()))
        .before_start(|_, project| {
            let dir = project.join(".agents/skills/forker");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join("SKILL.md"),
                "---\nname: forker\ndescription: writes two files\ncontext: fork\n\
                 allowed-tools: [Bash(echo hi > fork.txt), Bash(echo managed > managed.txt)]\n---\n\
                 FORK-MARK write the files\n",
            )?;
            Ok(())
        })
        .start()
        .await?;
    let session = run.session().await?;
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let watcher = run
        .watch_with(&session, {
            let asked = asked.clone();
            move |event| {
                if event["prompt_type"] == "permission" {
                    asked.lock().unwrap().push(format!(
                        "{} | {}",
                        event["input"]["command"].as_str().unwrap_or_default(),
                        event["message"].as_str().unwrap_or_default()
                    ));
                }
                Some(blackbox::Answer::deny("not asked for"))
            }
        })
        .await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls("s-1", "Skill", json!({ "skill": "forker" })),
            Reply::calls("p-1", "Bash", json!({ "command": "echo hi > fork.txt" })),
            Reply::text("parent done"),
        ],
    );
    stub.script_when(
        "FORK-MARK",
        [
            Reply::calls("f-1", "Bash", json!({ "command": "echo hi > fork.txt" })),
            Reply::calls(
                "f-2",
                "Bash",
                json!({ "command": "echo managed > managed.txt" }),
            ),
            Reply::text("fork done"),
        ],
    );
    run.turn(&session, "PARENT-MARK fork it, then write again")
        .await?;
    drop(watcher);
    let asked = asked.lock().unwrap().clone();
    assert_eq!(
        run.read_file("fork.txt").as_deref(),
        Some("hi\n"),
        "the fork's granted command did not run; asked {asked:?}"
    );
    assert!(
        asked.len() == 2 && asked[0].contains("managed.txt") && asked[1].contains("fork.txt"),
        "the managed ask in the fork, then the parent's own call, and nothing else: {asked:?}"
    );
    assert!(run.read_file("managed.txt").is_none());
    run.finish().await;
    Ok(())
}

fn turns_with(stub: &ProviderStub, mark: &str, not: &[&str]) -> Vec<SeenRequest> {
    stub.calls()
        .into_iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .filter(|c| c.messages_contain(mark) && not.iter().all(|n| !c.messages_contain(n)))
        .collect()
}

fn tool_result_in(calls: &[SeenRequest], tool_use_id: &str) -> Option<Value> {
    calls.iter().find_map(|c| {
        c.body["messages"]
            .as_array()?
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == tool_use_id)
            .cloned()
    })
}

async fn wait_until(what: &str, timeout: Duration, done: impl Fn() -> bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while !done() {
        anyhow::ensure!(Instant::now() < deadline, "{what} never happened");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

const WAIT_FOR_GO: &str = "touch waiting; while [ ! -e go ]; do sleep 0.05; done; echo released";

/// The first task of `kind` in `session.tasks`.
async fn task_of_kind(run: &mut LiveRun, session: &str, kind: &str) -> anyhow::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(task) = run
            .tasks(session)
            .await?
            .into_iter()
            .find(|t| t["kind"] == kind)
        {
            return Ok(task);
        }
        anyhow::ensure!(Instant::now() < deadline, "no {kind} task appeared");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A `context: fork` skill with `agent: explore` runs on the explore type's
/// read-only tools, and at the depth limit has no `Agent` — not the parent's,
/// either, which would count the grandchild against the parent's depth.
#[tokio::test(flavor = "multi_thread")]
async fn a_forked_skill_gets_its_agent_types_tools_and_no_parents_agent() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("fork-explore", &stub)
        .settings(json!({
            "execution": { "max_agent_depth": 1 },
            "permissions": {"allow": ["Skill", "Agent"]}
        }))
        .start()
        .await?;
    run.write_file(
        ".agents/skills/looker/SKILL.md",
        "---\nname: looker\ndescription: looks around\ncontext: fork\nagent: explore\n---\n\
         FORK-MARK look around the project\n",
    )?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls("s-1", "Skill", json!({ "skill": "looker" })),
            Reply::text("parent done"),
        ],
    );
    stub.script_when(
        "FORK-MARK",
        [
            Reply::calls(
                "f-1",
                "Agent",
                json!({ "prompt": "GRANDCHILD-MARK go deeper", "description": "deeper" }),
            ),
            Reply::text("fork done"),
        ],
    );
    stub.script_when("GRANDCHILD-MARK", [Reply::text("grandchild done")]);
    run.turn(&session, "PARENT-MARK use the looker skill")
        .await?;

    let fork = turns_with(&stub, "FORK-MARK", &["PARENT-MARK"]);
    assert!(!fork.is_empty(), "the skill never forked");
    let names = fork[0].tool_names();
    let mut problems = Vec::new();
    let writers: Vec<&String> = names
        .iter()
        .filter(|n| ["Write", "Edit", "Bash"].contains(&n.as_str()))
        .collect();
    if !writers.is_empty() {
        problems.push(format!("the explore fork was offered {writers:?}"));
    }
    if names.iter().any(|n| n == "Agent") {
        problems.push("the fork at the depth limit was offered Agent".to_string());
    }
    let grandchild = turns_with(&stub, "GRANDCHILD-MARK", &["FORK-MARK"]);
    if !grandchild.is_empty() {
        problems.push(format!(
            "the fork's Agent call spawned a grandchild past max_agent_depth 1 ({} request(s)); \
             its result: {:?}",
            grandchild.len(),
            tool_result_in(&fork, "f-1")
        ));
    }
    assert!(problems.is_empty(), "{problems:#?}\nfork tools: {names:?}");
    run.finish().await;
    Ok(())
}

/// `ARCHITECTURE.md` §3.4: a sub-agent's tools are narrowed by the caller's
/// lists, a skill's `allowed-tools` among them. A `context: fork` skill that
/// lists only `Read` forks a sub-agent that is offered `Read`, not `Write`,
/// `Edit` or `Bash`.
#[tokio::test(flavor = "multi_thread")]
async fn a_forked_skill_is_narrowed_to_its_allowed_tools() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("fork-allowed", &stub)
        .settings(json!({
            "permissions": {"allow": ["Skill"]}
        }))
        .start()
        .await?;
    run.write_file(
        ".agents/skills/reader/SKILL.md",
        "---\nname: reader\ndescription: reads a file\ncontext: fork\nallowed-tools: Read\n---\n\
         FORK-MARK read the readme\n",
    )?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls("s-1", "Skill", json!({ "skill": "reader" })),
            Reply::text("parent done"),
        ],
    );
    stub.script_when("FORK-MARK", [Reply::text("fork done")]);
    run.turn(&session, "PARENT-MARK use the reader skill")
        .await?;

    let fork = turns_with(&stub, "FORK-MARK", &["PARENT-MARK"]);
    anyhow::ensure!(!fork.is_empty(), "the skill never forked");
    let names = fork[0].tool_names();
    anyhow::ensure!(
        names.iter().any(|n| n == "Read"),
        "the fork has no Read at all: {names:?}"
    );
    let beyond: Vec<&String> = names
        .iter()
        .filter(|n| ["Write", "Edit", "Bash"].contains(&n.as_str()))
        .collect();
    assert!(
        beyond.is_empty(),
        "a fork of a skill with `allowed-tools: Read` was offered {beyond:?}"
    );
    run.finish().await;
    Ok(())
}

/// `session.stopTask` on a background `context: fork` skill stops that
/// sub-agent; the turn that forked it goes on, as it does for any stopped
/// task (protocol `session.stopTask`).
#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_background_forked_skill_leaves_the_turn_running() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("bg-fork-stop", &stub)
        .settings(json!({
            "permissions": {"allow": ["Skill", "Bash"]}
        }))
        .start()
        .await?;
    run.write_file(
        ".agents/skills/bgfork/SKILL.md",
        "---\nname: bgfork\ndescription: works in the background\ncontext: fork\nbackground: true\n---\n\
         FORK-MARK work for a while\n",
    )?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls("s-1", "Skill", json!({ "skill": "bgfork" })),
            Reply::calls("b-1", "Bash", json!({ "command": WAIT_FOR_GO })),
            Reply::text("parent done"),
        ],
    );
    stub.script_when(
        "FORK-MARK",
        [Reply::delayed(
            Duration::from_secs(120),
            Reply::text("fork done"),
        )],
    );
    let handle = run
        .spawn_turn(&session, "PARENT-MARK fork it, then wait", json!({}))
        .await?;
    let waiting = run.project.join("waiting");
    wait_until(
        "the parent's command waiting",
        Duration::from_secs(30),
        || waiting.exists(),
    )
    .await?;
    let fork = task_of_kind(&mut run, &session, "agent").await?;
    let stopped = run
        .client
        .call(
            "session.stopTask",
            json!({ "session_id": session, "task_id": fork["task_id"] }),
        )
        .await?;
    anyhow::ensure!(stopped.error.is_none(), "{stopped:?}");
    std::fs::write(run.project.join("go"), "")?;
    let events = handle.await??;
    let stop = events
        .turn_complete_event
        .as_ref()
        .map(|e| e["stop_reason"].clone())
        .unwrap_or_default();
    assert_eq!(
        stop,
        json!("end_turn"),
        "stopping the forked skill's task ended the turn that forked it; Bash said {:?}",
        events.tool_results
    );
    run.finish().await;
    Ok(())
}

/// A `context: fork` skill the turn waits on is a task of the session while it
/// runs: `session.tasks` lists it, and stopping it hands the turn back.
#[tokio::test(flavor = "multi_thread")]
async fn a_waited_on_forked_skill_is_a_task_the_host_can_stop() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("fork-task", &stub)
        .settings(json!({
            "permissions": {"allow": ["Skill"]}
        }))
        .start()
        .await?;
    run.write_file(
        ".agents/skills/slowfork/SKILL.md",
        "---\nname: slowfork\ndescription: takes its time\ncontext: fork\n---\n\
         FORK-MARK work for a while\n",
    )?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls("s-1", "Skill", json!({ "skill": "slowfork" })),
            Reply::text("parent done"),
        ],
    );
    stub.script_when(
        "FORK-MARK",
        [Reply::delayed(
            Duration::from_secs(120),
            Reply::text("fork done"),
        )],
    );
    let handle = run
        .spawn_turn(&session, "PARENT-MARK fork it", json!({}))
        .await?;
    wait_until(
        "the fork calling the model",
        Duration::from_secs(30),
        || !turns_with(&stub, "FORK-MARK", &["PARENT-MARK"]).is_empty(),
    )
    .await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let fork = loop {
        let tasks = run.tasks(&session).await?;
        if let Some(task) = tasks.into_iter().find(|t| t["kind"] == "agent") {
            break task;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the running fork is not among the session's tasks"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let stopped = run
        .client
        .call(
            "session.stopTask",
            json!({ "session_id": session, "task_id": fork["task_id"] }),
        )
        .await?;
    anyhow::ensure!(stopped.error.is_none(), "{stopped:?}");
    let events = tokio::time::timeout(Duration::from_secs(60), handle).await???;
    let stop = events
        .turn_complete_event
        .as_ref()
        .map(|e| e["stop_reason"].clone())
        .unwrap_or_default();
    assert_eq!(stop, json!("end_turn"), "{:?}", events.tool_results);
    run.finish().await;
    Ok(())
}
