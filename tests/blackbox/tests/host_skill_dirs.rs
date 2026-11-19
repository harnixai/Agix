//! Which skills a session has, from the outside: its scene's built-ins, and
//! the skill directories its host lists for it (`options.skill_dirs`) merged
//! over the binding's tiers — through the skill list, the `Skill` tool, `/name`
//! and `commands.list`, and what no session may write.

use std::path::Path;
use std::time::{Duration, Instant};

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, ProviderStub, Purpose, Reply, SeenRequest};
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(30);

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name)
        .settings(json!({ "permissions": {"mode": "default"}, "memory": { "enabled": false } }))
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
}

fn skill_file(name: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: the {name} skill\n---\n{body}\n")
}

/// Outside the OS temporary directory: on macOS a sandboxed command writes
/// there, and a host skill directory in it is refused.
fn scratch() -> anyhow::Result<tempfile::TempDir> {
    Ok(tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?)
}

fn host_dir(skills: &[(&str, &str)]) -> anyhow::Result<tempfile::TempDir> {
    let dir = scratch()?;
    for (name, body) in skills {
        add_skill(dir.path(), name, body)?;
    }
    Ok(dir)
}

fn add_skill(dir: &Path, name: &str, body: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir.join(name))?;
    std::fs::write(dir.join(name).join("SKILL.md"), skill_file(name, body))?;
    Ok(())
}

fn path_of(dir: &Path) -> String {
    dir.to_string_lossy().into_owned()
}

fn dirs_option(dirs: &[&Path]) -> Value {
    let list: Vec<Value> = dirs.iter().map(|d| json!({ "path": path_of(d) })).collect();
    json!({ "options": { "skill_dirs": list } })
}

fn turn_calls(stub: &ProviderStub) -> Vec<SeenRequest> {
    stub.calls()
        .into_iter()
        .filter(|c| c.purpose() == Purpose::Turn)
        .collect()
}

async fn rpc(run: &mut LiveRun, method: &str, params: Value) -> anyhow::Result<Value> {
    let resp = run.client.call(method, params).await?;
    anyhow::ensure!(resp.error.is_none(), "{method}: {:?}", resp.error);
    Ok(resp.result.unwrap_or_default())
}

async fn get(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    rpc(run, "session.get", json!({ "session_id": session })).await
}

async fn commands(run: &mut LiveRun, session: &str) -> anyhow::Result<Vec<Value>> {
    let listed = rpc(run, "commands.list", json!({ "session_id": session })).await?;
    Ok(listed["commands"].as_array().cloned().unwrap_or_default())
}

fn has_command(commands: &[Value], name: &str) -> bool {
    commands.iter().any(|c| c["name"] == name)
}

fn skill_names(detail: &Value, dir: usize) -> Vec<String> {
    detail["skill_dirs"][dir]["skills"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|n| n.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

async fn use_skill(
    run: &mut LiveRun,
    stub: &ProviderStub,
    session: &str,
    name: &str,
) -> anyhow::Result<(rpc_client::TurnEvents, SeenRequest)> {
    stub.script([
        Reply::calls("sk", "Skill", json!({ "skill": name })),
        Reply::text("used"),
    ]);
    let events = run.turn(session, &format!("use {name}")).await?;
    let follow_up = turn_calls(stub).pop().expect("the follow-up request");
    Ok((events, follow_up))
}

fn skill_result_is_error(events: &rpc_client::TurnEvents) -> bool {
    events
        .tool_results
        .iter()
        .find(|(n, _, _)| n == "Skill")
        .is_some_and(|(_, _, error)| *error)
}

async fn skills_changed_after(
    watcher: &blackbox::Watcher,
    before: usize,
    what: &str,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let frames = watcher.events("skills_changed");
        if let Some(frame) = frames.get(before) {
            return Ok(frame["event"].clone());
        }
        anyhow::ensure!(Instant::now() < deadline, "no skills_changed: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `harnix` lists only `skillify` of the built-ins: `/simplify` there is not
/// a command, so the text goes to the model as typed.
#[tokio::test(flavor = "multi_thread")]
async fn a_built_in_the_scene_does_not_list_is_not_a_slash_command() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("bundled-bypass", &stub).start().await?;
    let session = run.scene_session("harnix", json!({})).await?;
    stub.script([Reply::text("ok")]);
    run.turn(&session, "/simplify the parser").await?;
    let turn = stub
        .calls()
        .into_iter()
        .find(|c| c.purpose() == Purpose::Turn)
        .expect("the turn reached the model");
    assert!(
        !turn.messages_contain("# Simplify: Code Review and Cleanup"),
        "the scene's left-out built-in expanded as a command"
    );
    assert!(turn.messages_contain("/simplify the parser"));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_capability_is_advertised() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-dirs-capability", &stub).start().await?;
    let ping = rpc(&mut run, "daemon.ping", json!({})).await?;
    assert!(ping.to_string().contains("session_skill_dirs"), "{ping}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_dir_is_seen_only_by_the_session_that_lists_it() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-dirs-scope", &stub).start().await?;
    let dir = host_dir(&[("tour", "HOSTBODY-TOUR for $ARGUMENTS")])?;
    let a = run.session_with(dirs_option(&[dir.path()])).await?;
    let b = run.session().await?;

    stub.script([Reply::text("a1")]);
    run.turn(&a, "hello").await?;
    let first = turn_calls(&stub).remove(0);
    let seen = format!("{}\n{}", first.system_text(), first.messages_text());
    assert!(seen.contains("the tour skill"), "{seen}");

    let (events, follow_up) = use_skill(&mut run, &stub, &a, "tour").await?;
    assert!(!skill_result_is_error(&events), "{:?}", events.tool_results);
    assert!(follow_up.messages_contain("HOSTBODY-TOUR"));

    stub.script([Reply::text("slash")]);
    run.turn(&a, "/tour hi").await?;
    let slash = turn_calls(&stub).pop().unwrap();
    assert!(
        slash.messages_contain("HOSTBODY-TOUR for hi"),
        "{}",
        slash.messages_text()
    );

    let listed = commands(&mut run, &a).await?;
    let tour = listed
        .iter()
        .find(|c| c["name"] == "tour")
        .expect("tour listed");
    assert_eq!(tour["source"], "host", "{tour}");

    stub.script([Reply::text("b1")]);
    run.turn(&b, "hello").await?;
    let b_first = turn_calls(&stub).pop().unwrap();
    assert!(!b_first.system_contains("the tour skill"));
    let (events, follow_up) = use_skill(&mut run, &stub, &b, "tour").await?;
    assert!(skill_result_is_error(&events), "{:?}", events.tool_results);
    assert!(!follow_up.messages_contain("HOSTBODY-TOUR"));
    stub.script([Reply::text("plain")]);
    run.turn(&b, "/tour hi").await?;
    let plain = turn_calls(&stub).pop().unwrap();
    assert!(!plain.messages_contain("HOSTBODY-TOUR"));
    assert!(plain.messages_contain("/tour hi"));
    assert!(!has_command(&commands(&mut run, &b).await?, "tour"));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_skill_gets_only_narrowed_grants() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-dirs-grants", &stub).start().await?;
    let dir = scratch()?;
    std::fs::create_dir_all(dir.path().join("grant"))?;
    std::fs::write(
        dir.path().join("grant/SKILL.md"),
        "---\nname: grant\ndescription: the grant skill\nallowed-tools: Write\n---\nGRANTBODY\n",
    )?;
    let session = run.session_with(dirs_option(&[dir.path()])).await?;
    let watcher = run.watch(&session, false).await?;
    let outside = tempfile::tempdir()?;
    let target = outside.path().join("out.txt");
    stub.script([
        Reply::calls("sk", "Skill", json!({ "skill": "grant" })),
        Reply::calls(
            "w1",
            "Write",
            json!({ "file_path": path_of(&target), "content": "x" }),
        ),
        Reply::text("done"),
    ]);
    let turn = run.spawn_turn(&session, "go", json!({})).await?;
    let asked = watcher
        .wait_for("a prompt for the write", WAIT, |e| e["kind"] == "prompt")
        .await?;
    assert_eq!(asked["event"]["tool_name"], "Write", "{asked}");
    drop(turn);
    run.finish_allowing_refusals().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_users_tier_shadows_a_host_dir_and_the_first_dir_wins() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-dirs-merge", &stub)
        .before_start(|world, _| {
            let dir = world.global_root().join("skills/tour");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("SKILL.md"), skill_file("tour", "GLOBALBODY"))?;
            Ok(())
        })
        .start()
        .await?;
    let a = host_dir(&[
        ("tour", "HOSTBODY-TOUR"),
        ("guide", "BODY-FROM-A"),
        ("simplify", "HOST-SIMPLIFY"),
    ])?;
    let b = host_dir(&[("guide", "BODY-FROM-B")])?;
    let session = run.session_with(dirs_option(&[a.path(), b.path()])).await?;

    let (_, follow_up) = use_skill(&mut run, &stub, &session, "tour").await?;
    assert!(follow_up.messages_contain("GLOBALBODY"));
    assert!(!follow_up.messages_contain("HOSTBODY-TOUR"));

    let (_, follow_up) = use_skill(&mut run, &stub, &session, "guide").await?;
    assert!(follow_up.messages_contain("BODY-FROM-A"));
    assert!(!follow_up.messages_contain("BODY-FROM-B"));

    let (_, follow_up) = use_skill(&mut run, &stub, &session, "simplify").await?;
    assert!(follow_up.messages_contain("HOST-SIMPLIFY"));

    let detail = get(&mut run, &session).await?;
    let first = &detail["skill_dirs"][0];
    assert!(
        first["shadowed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["skill"] == "tour" && s["by"] == "user"),
        "{first}"
    );
    let won = skill_names(&detail, 0);
    assert!(!won.contains(&"tour".to_string()), "{won:?}");
    assert!(won.contains(&"guide".to_string()) && won.contains(&"simplify".to_string()));
    assert!(
        detail["skill_dirs"][1]["shadowed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["skill"] == "guide" && s["by"] == "host"),
        "{detail}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn set_skill_dirs_swaps_the_set_in_place() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-dirs-swap", &stub).start().await?;
    let a = host_dir(&[("tour", "TOURBODY")])?;
    let b = host_dir(&[("guide", "GUIDEBODY")])?;
    let session = run.session_with(dirs_option(&[a.path()])).await?;
    let watcher = run.watch(&session, false).await?;
    stub.script([Reply::text("one")]);
    run.turn(&session, "hello").await?;
    let before = watcher.events("skills_changed").len();

    let set = rpc(
        &mut run,
        "session.setSkillDirs",
        json!({ "session_id": session, "skill_dirs": [{ "path": path_of(b.path()) }] }),
    )
    .await?;
    assert_eq!(set["applies"], "next_turn", "{set}");
    assert_eq!(set["skill_dirs"], json!([{ "path": path_of(b.path()) }]));

    stub.script([Reply::text("two")]);
    run.turn(&session, "again").await?;
    let changed = skills_changed_after(&watcher, before, "after the swap").await?;
    assert!(changed["added"].to_string().contains("guide"), "{changed}");
    assert!(changed["removed"].to_string().contains("tour"), "{changed}");
    let listed = commands(&mut run, &session).await?;
    assert!(has_command(&listed, "guide") && !has_command(&listed, "tour"));
    drop(watcher);
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skill_added_to_a_dir_reaches_only_its_holders() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-dirs-watch", &stub).start().await?;
    let dir = host_dir(&[("tour", "TOURBODY")])?;
    let s1 = run.session_with(dirs_option(&[dir.path()])).await?;
    let s2 = run.session().await?;
    let w1 = run.watch(&s1, false).await?;
    let w2 = run.watch(&s2, false).await?;
    for s in [&s1, &s2] {
        stub.script([Reply::text("one")]);
        run.turn(s, "hello").await?;
    }
    let (before1, before2) = (
        w1.events("skills_changed").len(),
        w2.events("skills_changed").len(),
    );

    add_skill(dir.path(), "newone", "NEWBODY")?;
    let deadline = Instant::now() + WAIT;
    loop {
        if skill_names(&get(&mut run, &s1).await?, 0).contains(&"newone".to_string()) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the daemon never saw newone");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    for s in [&s1, &s2] {
        stub.script([Reply::text("two")]);
        run.turn(s, "again").await?;
    }
    let changed = skills_changed_after(&w1, before1, "for the holder").await?;
    assert!(changed["added"].to_string().contains("newone"), "{changed}");
    assert_eq!(w2.events("skills_changed").len(), before2);
    drop((w1, w2));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_dirs_survive_close_resume_and_fork_and_a_cold_set() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-dirs-persist", &stub).start().await?;
    let a = host_dir(&[("tour", "TOURBODY")])?;
    let b = host_dir(&[("guide", "GUIDEBODY")])?;
    let session = run.session_with(dirs_option(&[a.path()])).await?;
    stub.script([Reply::text("one")]);
    run.turn(&session, "hello").await?;

    rpc(&mut run, "session.close", json!({ "session_id": session })).await?;
    rpc(&mut run, "session.resume", json!({ "session_id": session })).await?;
    assert!(has_command(&commands(&mut run, &session).await?, "tour"));

    let forked = rpc(&mut run, "session.fork", json!({ "session_id": session })).await?;
    let fork = forked["session_id"].as_str().unwrap().to_string();
    rpc(&mut run, "session.close", json!({ "session_id": fork })).await?;
    rpc(&mut run, "session.resume", json!({ "session_id": fork })).await?;
    assert!(has_command(&commands(&mut run, &fork).await?, "tour"));

    rpc(&mut run, "session.close", json!({ "session_id": session })).await?;
    let set = rpc(
        &mut run,
        "session.setSkillDirs",
        json!({ "session_id": session, "skill_dirs": [{ "path": path_of(b.path()) }] }),
    )
    .await?;
    assert_eq!(set["applies"], "next_turn", "{set}");
    let cold = get(&mut run, &session).await?;
    assert_eq!(
        cold["skill_dirs"],
        json!([{ "path": path_of(b.path()) }]),
        "{cold}"
    );
    rpc(&mut run, "session.resume", json!({ "session_id": session })).await?;
    let listed = commands(&mut run, &session).await?;
    assert!(has_command(&listed, "guide") && !has_command(&listed, "tour"));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_skill_dirs_are_each_named_and_nothing_is_created() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-dirs-invalid", &stub).start().await?;
    let dir = host_dir(&[("tour", "TOURBODY")])?;
    let plain = tempfile::NamedTempFile::new()?;
    let global_skills = run.world.global_root().join("skills");
    std::fs::create_dir_all(&global_skills)?;
    let before = run.client.session_list().await?.result.unwrap_or_default()["sessions"]
        .as_array()
        .map_or(0, Vec::len);

    let refused = run
        .client
        .call(
            "session.create",
            json!({
                "project_root": run.project.to_string_lossy(),
                "options": { "skill_dirs": [
                    { "path": "relative/x" },
                    { "path": path_of(plain.path()) },
                    { "path": path_of(dir.path()) },
                    { "path": format!("{}/", path_of(dir.path())) },
                    { "path": path_of(&global_skills) },
                ] }
            }),
        )
        .await?;
    let error = refused.error.expect("the list is refused");
    assert_eq!(error.code, -32602, "{error:?}");
    let invalid = error.data.as_ref().unwrap()["invalid_options"].clone();
    let fields: Vec<&str> = invalid
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["field"].as_str())
        .collect();
    assert_eq!(
        fields,
        [
            "skill_dirs[0].path",
            "skill_dirs[1].path",
            "skill_dirs[3].path",
            "skill_dirs[4].path"
        ],
        "{invalid}"
    );

    let inside = run.project.join("sub");
    std::fs::create_dir_all(&inside)?;
    let refused = run
        .client
        .call(
            "session.create",
            json!({
                "project_root": run.project.to_string_lossy(),
                "options": { "skill_dirs": [{ "path": path_of(&inside) }] }
            }),
        )
        .await?;
    let error = refused
        .error
        .expect("a directory of the project is refused");
    assert_eq!(error.code, -32602, "{error:?}");
    let after = run.client.session_list().await?.result.unwrap_or_default()["sessions"]
        .as_array()
        .map_or(0, Vec::len);
    assert_eq!(before, after, "no session was created");

    let later = scratch()?;
    let missing = later.path().join("not-yet");
    let session = run.session_with(dirs_option(&[&missing])).await?;
    let detail = get(&mut run, &session).await?;
    assert_eq!(detail["skill_dirs"][0]["state"], "missing", "{detail}");
    add_skill(&missing, "tour", "LATEBODY")?;
    stub.script([Reply::text("one")]);
    let deadline = Instant::now() + WAIT;
    loop {
        run.turn(&session, "hello").await?;
        if skill_names(&get(&mut run, &session).await?, 0).contains(&"tour".to_string()) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the created dir was never read");
        stub.script([Reply::text("again")]);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let detail = get(&mut run, &session).await?;
    assert_eq!(detail["skill_dirs"][0]["state"], "ok", "{detail}");
    let (events, follow_up) = use_skill(&mut run, &stub, &session, "tour").await?;
    assert!(
        follow_up.messages_contain("LATEBODY"),
        "{:?}\n{}",
        events.tool_results,
        follow_up.messages_text()
    );
    run.finish().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_link_out_of_the_dir_is_not_read() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-dirs-link", &stub).start().await?;
    let outside = host_dir(&[("secret", "SECRETBODY")])?;
    let dir = host_dir(&[("tour", "TOURBODY")])?;
    std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("escape"))?;
    let session = run.session_with(dirs_option(&[dir.path()])).await?;
    let detail = get(&mut run, &session).await?;
    let names = skill_names(&detail, 0);
    assert!(names.contains(&"tour".to_string()), "{detail}");
    assert!(!names.contains(&"escape".to_string()), "{detail}");
    let (events, follow_up) = use_skill(&mut run, &stub, &session, "escape").await?;
    assert!(skill_result_is_error(&events), "{:?}", events.tool_results);
    assert!(!follow_up.messages_contain("SECRETBODY"));
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn no_session_writes_a_host_dir() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-dirs-write", &stub)
        .settings(json!({ "permissions": {"mode": "acceptEdits"}, "memory": { "enabled": false } }))
        .start()
        .await?;
    let dir = host_dir(&[("tour", "ORIGINAL")])?;
    let target = dir.path().join("tour/SKILL.md");
    let original = std::fs::read_to_string(&target)?;
    let holder = run.session_with(dirs_option(&[dir.path()])).await?;
    let other = run.session().await?;
    for session in [&holder, &other] {
        let watcher = run.watch(session, true).await?;
        stub.script([
            Reply::calls("r1", "Read", json!({ "file_path": path_of(&target) })),
            Reply::calls(
                "w1",
                "Write",
                json!({ "file_path": path_of(&target), "content": "OVERWRITTEN" }),
            ),
            Reply::text("done"),
        ]);
        let events = run.turn(session, "overwrite the skill").await?;
        let (_, content, error) = events
            .tool_results
            .iter()
            .find(|(n, _, _)| n == "Write")
            .expect("the Write answered")
            .clone();
        assert!(error, "{content}");
        assert!(content.contains("skill directory"), "{content}");
        assert!(
            watcher
                .events("prompt")
                .iter()
                .all(|p| p["event"]["tool_name"] != "Write"),
            "no prompt for a denial"
        );
        assert_eq!(std::fs::read_to_string(&target)?, original);
    }
    run.finish_allowing_refusals().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unattended_session_reads_its_host_dir() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-dirs-unattended", &stub).start().await?;
    let dir = host_dir(&[("tour", "TOURBODY")])?;
    std::fs::write(dir.path().join("tour/notes.md"), "NOTES-CONTENT-7")?;
    let session = run
        .scene_session("harnix", dirs_option(&[dir.path()]))
        .await?;
    let watcher = run.watch(&session, false).await?;
    stub.script([
        Reply::calls(
            "r1",
            "Read",
            json!({ "file_path": path_of(&dir.path().join("tour/notes.md")) }),
        ),
        Reply::text("read"),
    ]);
    let events = run.turn(&session, "read the notes").await?;
    let (_, content, error) = events
        .tool_results
        .iter()
        .find(|(n, _, _)| n == "Read")
        .expect("the Read answered")
        .clone();
    assert!(!error && content.contains("NOTES-CONTENT-7"), "{content}");
    assert!(watcher.events("prompt").is_empty());
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// The command an unattended session runs confined without asking, writing
/// into `dir` without naming it: `cd` to its parent, then into it.
fn unnamed_write_into(dir: &Path) -> String {
    let parent = path_of(dir.parent().expect("a parent"));
    let name = dir.file_name().expect("a name").to_string_lossy();
    format!("cd '{parent}' && cd '{name}' && printf OVERWRITTEN > tour/SKILL.md; echo rc=$?")
}

/// On macOS the per-user temporary directory is a root the sandbox lets a
/// confined command write: a host skill directory there is refused, so the
/// write that does not name it cannot reach a host skill.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn a_host_dir_in_the_temp_dir_is_refused() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-dirs-in-temp", &stub).start().await?;
    let dir = tempfile::tempdir()?;
    add_skill(dir.path(), "tour", "ORIGINAL")?;
    let refused = run
        .client
        .call(
            "session.create",
            json!({
                "project_root": run.project.to_string_lossy(),
                "options": { "skill_dirs": [{ "path": path_of(dir.path()) }] }
            }),
        )
        .await?;
    let error = refused.error.expect("a dir the sandbox writes is refused");
    assert_eq!(error.code, -32602, "{error:?}");
    let invalid = &error.data.as_ref().unwrap()["invalid_options"][0];
    assert_eq!(invalid["field"], "skill_dirs[0].path", "{invalid}");
    run.finish_allowing_refusals().await;
    Ok(())
}

/// A host dir the sandbox does not let a command write: the same unnamed
/// write, run confined without asking, is held off it.
#[tokio::test(flavor = "multi_thread")]
async fn a_sandboxed_command_does_not_write_a_host_dir() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("skill-dirs-sandboxed-write", &stub).start().await?;
    let dir = host_dir(&[("tour", "ORIGINAL")])?;
    let target = dir.path().join("tour/SKILL.md");
    let original = std::fs::read_to_string(&target)?;
    let session = run
        .scene_session("harnix", dirs_option(&[dir.path()]))
        .await?;
    stub.script([
        Reply::calls(
            "b1",
            "Bash",
            json!({ "command": unnamed_write_into(dir.path()) }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "overwrite the skill").await?;
    let answered = events.tool_results.iter().find(|(n, _, _)| n == "Bash");
    assert_eq!(
        std::fs::read_to_string(&target)?,
        original,
        "the host skill was overwritten: {answered:?}"
    );
    run.finish_allowing_refusals().await;
    Ok(())
}
