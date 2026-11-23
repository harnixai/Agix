//! The `forge` example plugin (`examples/plugins/forge`, `docs/forge.md`)
//! against a model stub: packed from the example directory, installed with
//! `plugin.install`, its command driven by scripted `RunExt` calls, a
//! program and the user's `/forge`. Only what the RPC, the session frames, the
//! model's requests, the project's `forge/` and the toy MCP store show.

use std::path::Path;

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, McpToy, ProviderStub, Purpose, Reply};
use serde_json::{json, Value};
use test_runner::plugin_fixture::{
    file_url, forge_package_dir, package_dir, package_forge, FORGE_PACKAGE, FORGE_PACKAGE_VERSION,
};

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

async fn install(run: &mut LiveRun, archive: &Path) -> anyhow::Result<Value> {
    let resp = run
        .client
        .plugin_install_and_enable(
            json!({ "name": FORGE_PACKAGE, "version": FORGE_PACKAGE_VERSION,
                    "download_url": file_url(archive) }),
        )
        .await?;
    Ok(match (resp.result, resp.error) {
        (Some(result), _) => result,
        (None, Some(error)) => json!({ "error": error.message }),
        (None, None) => Value::Null,
    })
}

async fn with_forge(name: &str, stub: &ProviderStub, settings: Value) -> anyhow::Result<LiveRun> {
    with_forge_from(stubbed(name, stub).settings(settings)).await
}

async fn with_forge_from(builder: LiveRunBuilder) -> anyhow::Result<LiveRun> {
    let mut run = builder.start().await?;
    let (archive, _) = package_forge(run.world.root())?;
    let installed = install(&mut run, &archive).await?;
    anyhow::ensure!(installed["success"] == true, "install: {installed}");
    Ok(run)
}

fn forge(id: &str, command: &str, input: Value) -> Reply {
    let mut call = json!({ "command": command });
    if !input.is_null() {
        call["input"] = input;
    }
    Reply::calls(id, "RunExt", call)
}

/// Each `RunExt` result of the turn: its value when it succeeded, the error
/// text when it did not.
fn ran(events: &rpc_client::TurnEvents) -> Vec<Result<Value, String>> {
    events
        .tool_results
        .iter()
        .filter(|(name, _, _)| name == "RunExt")
        .map(|(_, text, failed)| {
            let text = blackbox::inside_fence(text);
            if *failed {
                Err(text.to_string())
            } else {
                Ok(serde_json::from_str(text).unwrap_or_else(|_| json!(text)))
            }
        })
        .collect()
}

fn all_ok(results: &[Result<Value, String>]) -> Vec<Value> {
    results
        .iter()
        .map(|r| {
            r.clone()
                .unwrap_or_else(|e| panic!("a forge call failed: {e}\n{results:?}"))
        })
        .collect()
}

fn turn_calls(stub: &ProviderStub) -> Vec<blackbox::SeenRequest> {
    stub.calls()
        .into_iter()
        .filter(|c| c.purpose() != Purpose::Title)
        .collect()
}

const REQ: &str = "---\nid: R-001\ntitle: Ledger totals\nstatus: proposed\npriority: high\n\
                   specs: []\nchanges: []\n---\nTotals per category.\n";

/// The stages in order, the way the skills drive them, ending archived.
fn whole_flow() -> Vec<Reply> {
    vec![
        forge(
            "r1",
            "forge req add",
            json!({ "title": "Ledger totals", "priority": "high",
                    "body": "Sum a ledger's amounts per category.\n" }),
        ),
        forge(
            "c1",
            "forge flow new",
            json!({ "title": "Totals per category", "reqs": ["R-001"],
                    "body": "Why: the user wants totals.\n" }),
        ),
        forge("a1", "forge flow advance C-001", Value::Null),
        forge(
            "d1",
            "forge flow delta C-001",
            json!({ "add": [{ "capability": "ledger", "title": "Totals per category",
                              "body": "`ledger totals FILE` prints one line per category.",
                              "criteria": ["Each category is printed once with its sum",
                                           "An empty file prints nothing"] }] }),
        ),
        forge("a2", "forge flow advance C-001", Value::Null),
        forge(
            "p1",
            "forge flow plan C-001",
            json!({ "items": ["Parse the ledger", "Sum and print"] }),
        ),
        forge("a3", "forge flow advance C-001", Value::Null),
        forge("k1", "forge flow check C-001 P1", Value::Null),
        forge("k2", "forge flow check C-001 P1 P2", Value::Null),
        forge("n1", "forge flow next C-001", Value::Null),
        forge("a4", "forge flow advance C-001", Value::Null),
        forge(
            "v1",
            "forge flow verify C-001",
            json!({ "results": [
                { "spec": "S-ledger-001", "criterion": "A1", "pass": true, "note": "test totals" },
                { "spec": "S-ledger-001", "criterion": "A2", "pass": true, "note": "test empty" }
            ] }),
        ),
        forge("a5", "forge flow advance C-001", Value::Null),
        forge("s1", "forge flow show C-001", Value::Null),
        forge("x1", "forge flow archive C-001", Value::Null),
        Reply::text("archived"),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn install_discloses_the_command_its_scope_skills_and_verifier() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("forge-disclosed", &stub).start().await?;
    let (archive, _) = package_forge(run.world.root())?;
    let installed = install(&mut run, &archive).await?;
    let disclosure = &installed["disclosure"];
    assert_eq!(disclosure["commands"], json!(["forge"]), "{installed}");
    assert!(
        disclosure["capabilities"]
            .to_string()
            .contains("adds command `forge` that may write forge/**"),
        "{disclosure}"
    );
    let skills: Vec<&str> = disclosure["skills"]
        .as_array()
        .map(|s| s.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for stage in [
        "propose", "spec", "plan", "apply", "verify", "archive", "req", "next",
    ] {
        assert!(
            skills.contains(&format!("forge-{stage}").as_str()),
            "{skills:?}"
        );
    }
    assert!(
        disclosure["model_visible"]
            .to_string()
            .contains("You verify one forge change"),
        "the verifier's prompt is disclosed: {disclosure}"
    );

    let listed = run.client.call("plugin.list", json!({})).await?;
    let forge = listed.result.unwrap_or_default()["plugins"]
        .as_array()
        .and_then(|all| all.iter().find(|p| p["name"] == FORGE_PACKAGE).cloned())
        .expect("forge is listed");
    assert_eq!(forge["commands"][0]["name"], "forge", "{forge}");
    assert_eq!(
        forge["commands"][0]["writes"],
        json!(["forge/**"]),
        "{forge}"
    );
    assert!(
        forge["commands"][0]["read_only"]
            .as_array()
            .is_some_and(|r| r.contains(&json!("flow next"))),
        "{forge}"
    );
    assert_eq!(forge["script_faults"], json!([]), "{forge}");
    run.finish().await;
    Ok(())
}

/// The model walks one change through every stage with the command alone;
/// each write subcommand is asked about once and the command's own writes
/// under `forge/` are not asked about again.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_goes_from_propose_to_archive_through_the_command() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge("forge-flow", &stub, json!({})).await?;
    stub.script(whole_flow());
    let session = run.session().await?;
    let watcher = run.watch(&session, true).await?;
    let events = run.turn(&session, "build the totals").await?;
    let out = all_ok(&ran(&events));

    assert_eq!(out[0]["added"][0]["id"], "R-001", "{out:?}");
    assert_eq!(out[1]["id"], "C-001");
    assert_eq!(out[2]["stage"], "spec");
    assert_eq!(out[3]["add"], json!(["S-ledger-001"]));
    assert_eq!(out[8]["open"], json!([]), "{}", out[8]);
    assert_eq!(out[9]["stage"], "apply", "flow next: {}", out[9]);
    assert_eq!(out[9]["skill"], "forge-apply");
    assert_eq!(out[11]["failed"], json!([]), "{}", out[11]);
    assert_eq!(out[12]["stage"], "archive");
    assert_eq!(
        out[13]["delta"]["add"][0]["criteria"][1]["verified"]["pass"], true,
        "{}",
        out[13]
    );
    assert_eq!(out[14]["archived_to"], "forge/archive/C-001", "{}", out[14]);

    let req = run.read_file("forge/reqs/R-001.md").unwrap_or_default();
    assert!(req.contains("status: done"), "{req}");
    assert!(
        req.contains("S-ledger-001") && req.contains("C-001"),
        "{req}"
    );
    assert!(
        req.ends_with("Sum a ledger's amounts per category.\n"),
        "{req}"
    );
    let spec = run.read_file("forge/specs/ledger.md").unwrap_or_default();
    assert!(spec.starts_with("---\ncapability: ledger\n---\n"), "{spec}");
    assert!(
        spec.contains("## S-ledger-001 Totals per category\nreqs: R-001\n")
            && spec.contains("### 验收\n- [A1] Each category is printed once with its sum\n- [A2] An empty file prints nothing\n"),
        "{spec}"
    );
    let archived = run
        .read_file("forge/archive/C-001/change.md")
        .unwrap_or_default();
    assert!(archived.contains("stage: done"), "{archived}");
    assert_eq!(archived.matches("criterion: A").count(), 2, "{archived}");
    assert!(run.read_file("forge/archive/C-001/delta.md").is_some());
    let left = run
        .read_file("forge/changes/C-001/change.md")
        .unwrap_or_default();
    assert!(left.contains("archived: true"), "{left}");

    let asked: Vec<Value> = watcher
        .asked()
        .iter()
        .map(|a| a.prompt["event"]["tool_name"].clone())
        .collect();
    assert_eq!(
        asked,
        vec![json!("RunExt"); 13],
        "each write subcommand once, the read-only ones not at all"
    );
    let system = turn_calls(&stub)[0].system_text();
    assert!(
        system.contains("keeps its requirements, specs and changes in forge"),
        "the rules block is in the prompt"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// `flow next` tells auto mode to go on until a `stop_before` stage, and
/// `advance` refuses on structure alone.
#[tokio::test(flavor = "multi_thread")]
async fn auto_mode_goes_on_until_a_stop_before_stage() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge_from(
        stubbed("forge-auto", &stub)
            .settings(json!({ "permissions": {"mode": "acceptEdits"} }))
            .before_start(|world, _| {
                world.write_project_file(
                    "forge-auto",
                    "forge/config.yaml",
                    "flow:\n  auto: true\n  stop_before: [apply]\n",
                )?;
                Ok(())
            }),
    )
    .await?;
    let mut script = whole_flow()[..6].to_vec();
    script.insert(5, forge("a-early", "forge flow advance C-001", Value::Null));
    script.extend([
        forge("a3", "forge flow advance C-001", Value::Null),
        Reply::text("stopped"),
    ]);
    stub.script(script);
    let session = run.session().await?;
    let events = run.turn(&session, "go").await?;
    let results = ran(&events);

    let spec = results[2].as_ref().expect("advance to spec");
    assert_eq!(
        (spec["stage"].clone(), spec["continue"].clone()),
        (json!("spec"), json!(true)),
        "{spec}"
    );
    assert!(
        spec["say"]
            .as_str()
            .unwrap_or_default()
            .contains("forge-spec"),
        "{spec}"
    );
    let early = results[5]
        .as_ref()
        .expect_err("advance from plan without a plan");
    assert!(early.contains("stays at plan: no plan yet"), "{early}");
    let apply = results[7].as_ref().expect("advance to apply");
    assert_eq!(
        (apply["stage"].clone(), apply["continue"].clone()),
        (json!("apply"), json!(false)),
        "{apply}"
    );
    assert!(
        apply["say"]
            .as_str()
            .unwrap_or_default()
            .contains("stops before apply"),
        "{apply}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_subcommands_run_in_plan_mode_and_write_ones_do_not() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge_from(stubbed("forge-plan", &stub).before_start(|world, _| {
        world.write_project_file("forge-plan", "forge/reqs/R-001.md", REQ)?;
        Ok(())
    }))
    .await?;
    stub.script([
        forge("l", "forge req list", Value::Null),
        forge("g", "forge req get R-001", Value::Null),
        forge("c", "forge spec coverage", Value::Null),
        forge("u", "forge req update R-001 --status accepted", Value::Null),
        Reply::text("done"),
    ]);
    let session = run
        .session_with(json!({ "options": { "settings": { "permissions": { "mode": "plan" } } } }))
        .await?;
    let watcher = run.watch(&session, true).await?;
    let events = run.turn(&session, "look").await?;
    let results = ran(&events);
    let ok = all_ok(&results[..3]);
    assert_eq!(
        ok[0],
        json!({ "items": [{ "id": "R-001", "title": "Ledger totals",
                            "status": "proposed", "priority": "high" }] })
    );
    assert_eq!(ok[1]["body"], "Totals per category.\n");
    assert_eq!(ok[2]["accepted_without_specs"], json!([]));
    let refused = results[3].as_ref().expect_err("a write in plan mode");
    assert!(refused.contains("plan mode"), "{refused}");
    assert!(run
        .read_file("forge/reqs/R-001.md")
        .unwrap()
        .contains("status: proposed"));
    assert!(watcher.asked().is_empty(), "nothing is asked in plan mode");
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// A hand-broken file is reported by file and field, as is a file whose
/// name disagrees with what it holds; a wrong option gets the group's usage
/// back.
#[tokio::test(flavor = "multi_thread")]
async fn a_broken_file_and_a_wrong_option_are_reported_precisely() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge_from(stubbed("forge-broken", &stub).before_start(|world, _| {
        world.write_project_file(
            "forge-broken",
            "forge/reqs/R-001.md",
            &REQ.replace("status: proposed", "status: maybe"),
        )?;
        world.write_project_file("forge-broken", "forge/reqs/R-003.md", REQ)?;
        world.write_project_file(
            "forge-broken",
            "forge/specs/ledger.md",
            "---\ncapability: report\n---\n",
        )?;
        Ok(())
    }))
    .await?;
    stub.script([
        forge("g", "forge req get R-001", Value::Null),
        forge("o", "forge req list --bogus", Value::Null),
        forge("x", "forge req get ../secret", Value::Null),
        forge("n", "forge req get R-003", Value::Null),
        forge("s", "forge spec list", Value::Null),
        Reply::text("done"),
    ]);
    let session = run.session().await?;
    let events = run.turn(&session, "look").await?;
    let results = ran(&events);
    let broken = results[0].as_ref().expect_err("a broken file");
    assert!(
        broken.contains("forge/reqs/R-001.md") && broken.contains("/status"),
        "{broken}"
    );
    let option = results[1].as_ref().expect_err("an unknown option");
    assert!(
        option.contains("--bogus") && option.contains("forge req list [--status"),
        "{option}"
    );
    let id = results[2].as_ref().expect_err("not an id");
    assert!(id.contains("is not a requirement id"), "{id}");
    let named = results[3].as_ref().expect_err("id and file name differ");
    assert!(named.contains("its id is R-001"), "{named}");
    let spec = results[4]
        .as_ref()
        .expect_err("capability and file name differ");
    assert!(spec.contains("the file name says ledger"), "{spec}");
    run.finish().await;
    Ok(())
}

/// The guard refuses the model's and a program's direct reads and writes of
/// `forge/` with the command to use instead; the command's own reads pass.
#[tokio::test(flavor = "multi_thread")]
async fn direct_access_to_forge_is_refused_and_the_command_still_reads_it() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge_from(stubbed("forge-guard", &stub).before_start(|world, _| {
        world.write_project_file("forge-guard", "forge/reqs/R-001.md", REQ)?;
        Ok(())
    }))
    .await?;
    let file = run.project.join("forge/reqs/R-001.md");
    let file = file.to_string_lossy().into_owned();
    let other = run.project.join("forge/reqs/R-002.md");
    stub.script([
        Reply::calls("read", "Read", json!({ "file_path": file })),
        Reply::calls("glob", "Glob", json!({ "pattern": "forge/**/*.md" })),
        Reply::calls(
            "grep",
            "Grep",
            json!({ "pattern": "status", "path": "forge" }),
        ),
        Reply::calls(
            "bash",
            "Bash",
            json!({ "command": "cat forge/reqs/R-001.md" }),
        ),
        Reply::calls(
            "write",
            "Write",
            json!({ "file_path": other.to_string_lossy(), "content": "x" }),
        ),
        Reply::calls(
            "code",
            "RunCode",
            json!({ "code": format!("return await tools.Read({{file_path: {}}});", json!(file)) }),
        ),
        forge("cmd", "forge req get R-001", Value::Null),
        Reply::text("done"),
    ]);
    let session = run.session().await?;
    let watcher = run.watch(&session, true).await?;
    let events = run.turn(&session, "read the requirement").await?;
    for tool in ["Read", "Glob", "Grep", "Bash", "Write", "RunCode"] {
        let (_, text, failed) = events
            .tool_results
            .iter()
            .find(|(name, _, _)| name == tool)
            .unwrap_or_else(|| panic!("no {tool} result: {:?}", events.tool_results));
        assert!(
            *failed && text.contains("forge/ is kept by the forge command"),
            "{tool}: {text}"
        );
    }
    let read = events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "Read")
        .unwrap();
    assert!(read.1.contains("forge req get"), "{read:?}");
    let command = all_ok(&ran(&events));
    assert_eq!(command[0]["title"], "Ledger totals");
    assert!(run.read_file("forge/reqs/R-002.md").is_none());
    let asked: Vec<Value> = watcher
        .asked()
        .iter()
        .map(|a| a.prompt["event"]["tool_name"].clone())
        .collect();
    assert!(
        !asked.contains(&json!("Bash")) && !asked.contains(&json!("Write")),
        "the guard refuses before anyone is asked: {asked:?}"
    );
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// With `reqs: {backend: mcp}` the same requirement commands answer the same,
/// and the requirements land in the toy's store, not in `forge/reqs`.
#[tokio::test(flavor = "multi_thread")]
async fn requirements_on_an_mcp_backend_answer_as_the_local_ones() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let store = tempfile::tempdir()?;
    let toy = McpToy::stdio_config(&[
        "--name",
        "reqdb",
        "--store",
        &store.path().to_string_lossy(),
    ])?;
    let allow: Vec<String> = ["items_query", "items_get", "items_put"]
        .iter()
        .map(|t| format!("mcp__reqdb__{t}"))
        .collect();
    let mut run = with_forge_from(
        stubbed("forge-local", &stub)
            .settings(json!({ "permissions": {"mode": "acceptEdits"} }))
            .before_start(move |world, _| {
                world.write_global_settings(
                    &json!({ "mcp": { "servers": { "reqdb": toy } }, "permissions": { "allow": allow } }),
                )?;
                let remote = world.project("forge-remote")?;
                world.trust_project(&remote)?;
                world.write_project_settings(
                    "forge-remote",
                    &json!({ "memory": { "enabled": false }, "permissions": {"mode": "acceptEdits"} }),
                )?;
                world.write_project_file(
                    "forge-remote",
                    "forge/config.yaml",
                    "reqs: { backend: mcp, server: reqdb }\n",
                )?;
                Ok(())
            }),
    )
    .await?;
    let remote = run.world.project("forge-remote")?;
    let steps = || {
        vec![
            forge(
                "add",
                "forge req add",
                json!({ "items": [
                    { "title": "Import CSV", "priority": "high", "body": "From a bank export." },
                    { "title": "Export totals", "body": "As CSV." }
                ] }),
            ),
            forge(
                "upd",
                "forge req update R-002 --priority low",
                json!({ "body": "As CSV, one line per category." }),
            ),
            forge(
                "new",
                "forge flow new",
                json!({ "title": "Import", "reqs": ["R-001"], "body": "First." }),
            ),
            forge("list", "forge req list --status accepted", Value::Null),
            forge("get", "forge req get R-002", Value::Null),
            Reply::text("done"),
        ]
    };

    stub.script(steps());
    let local_session = run.session().await?;
    let local = all_ok(&ran(&run.turn(&local_session, "track").await?));
    stub.script(steps());
    let remote_session = run
        .session_with(json!({ "project_root": remote.to_string_lossy() }))
        .await?;
    let remote_out = all_ok(&ran(&run.turn(&remote_session, "track").await?));

    assert_eq!(local, remote_out, "the two backends answer alike");
    assert_eq!(
        local[3],
        json!({ "items": [{ "id": "R-001", "title": "Import CSV",
                            "status": "accepted", "priority": "high" }] })
    );
    assert_eq!(local[4]["body"], "As CSV, one line per category.");
    assert!(run.read_file("forge/reqs/R-002.md").is_some());
    assert!(
        !remote.join("forge/reqs").exists(),
        "remote reqs stay remote"
    );
    assert!(
        remote.join("forge/changes/C-001/change.md").exists(),
        "changes are local"
    );
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(store.path().join("req.json"))?)?;
    assert_eq!(stored[0]["id"], "R-001", "{stored}");
    assert_eq!(stored[0]["status"], "accepted", "{stored}");
    assert_eq!(stored[0]["changes"], json!(["C-001"]), "{stored}");
    assert_eq!(stored[1]["priority"], "low", "{stored}");
    run.finish().await;
    Ok(())
}

/// Specs kept on an MCP backend: the whole flow lands its entries in the
/// toy's store, nothing under `forge/specs`, and `spec get` reads them back.
#[tokio::test(flavor = "multi_thread")]
async fn specs_on_an_mcp_backend_take_the_merged_entries() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let store = tempfile::tempdir()?;
    let toy = McpToy::stdio_config(&[
        "--name",
        "specdb",
        "--store",
        &store.path().to_string_lossy(),
    ])?;
    let allow: Vec<String> = ["items_query", "items_get", "items_put"]
        .iter()
        .map(|t| format!("mcp__specdb__{t}"))
        .collect();
    let mut run = with_forge_from(
        stubbed("forge-specdb", &stub)
            .settings(json!({ "permissions": {"mode": "acceptEdits"} }))
            .before_start(move |world, _| {
                world.write_global_settings(
                    &json!({ "mcp": { "servers": { "specdb": toy } }, "permissions": { "allow": allow } }),
                )?;
                world.write_project_file(
                    "forge-specdb",
                    "forge/config.yaml",
                    "specs: { backend: mcp, server: specdb }\n",
                )?;
                Ok(())
            }),
    )
    .await?;
    let mut script = whole_flow();
    script.pop();
    script.extend([
        forge("get", "forge spec get S-ledger-001", Value::Null),
        forge("list", "forge spec list", Value::Null),
        Reply::text("done"),
    ]);
    stub.script(script);
    let session = run.session().await?;
    let out = all_ok(&ran(&run.turn(&session, "build the totals").await?));
    assert_eq!(
        out[14]["specs"]["added"],
        json!(["S-ledger-001"]),
        "{}",
        out[14]
    );
    assert_eq!(out[15]["title"], "Totals per category", "{}", out[15]);
    assert_eq!(out[15]["criteria"][1]["id"], "A2", "{}", out[15]);
    assert_eq!(out[16]["items"][0]["criteria"], 2, "{}", out[16]);
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(store.path().join("spec.json"))?)?;
    assert_eq!(stored[0]["id"], "S-ledger-001", "{stored}");
    assert!(
        !run.project.join("forge/specs").exists(),
        "specs stay remote"
    );
    run.finish().await;
    Ok(())
}

/// From `propose` to the `archive` stage: `delta` written, a one-item plan
/// checked, and every criterion in `passes` verified as passing.
fn up_to_archive(id: &str, delta: Value, passes: &[(&str, &str)]) -> Vec<Reply> {
    let advance = |n: u32| {
        forge(
            &format!("{id}-a{n}"),
            &format!("forge flow advance {id}"),
            Value::Null,
        )
    };
    let results: Vec<Value> = passes
        .iter()
        .map(|(spec, criterion)| json!({ "spec": spec, "criterion": criterion, "pass": true }))
        .collect();
    let mut steps = vec![
        advance(1),
        forge(&format!("{id}-d"), &format!("forge flow delta {id}"), delta),
        advance(2),
        forge(
            &format!("{id}-p"),
            &format!("forge flow plan {id}"),
            json!({ "items": ["Do it"] }),
        ),
        advance(3),
        forge(
            &format!("{id}-k"),
            &format!("forge flow check {id} P1"),
            Value::Null,
        ),
        advance(4),
    ];
    if !results.is_empty() {
        steps.push(forge(
            &format!("{id}-v"),
            &format!("forge flow verify {id}"),
            json!({ "results": results }),
        ));
    }
    steps.push(advance(5));
    steps
}

fn new_change(id: &str, reqs: Value) -> Reply {
    forge(
        &format!("{id}-new"),
        "forge flow new",
        json!({ "title": format!("Change {id}"), "reqs": reqs, "body": "Why.\n" }),
    )
}

fn ledger_entry(title: &str, criteria: &[&str]) -> Value {
    json!({ "capability": "ledger", "title": title, "body": format!("{title}, in words."),
            "criteria": criteria })
}

/// One turn of scripted forge calls that must all succeed.
async fn setup(
    run: &mut LiveRun,
    stub: &ProviderStub,
    session: &str,
    mut steps: Vec<Reply>,
) -> anyhow::Result<Vec<Value>> {
    steps.push(Reply::text("set up"));
    stub.script(steps);
    Ok(all_ok(&ran(&run.turn(session, "set up").await?)))
}

/// A body line that reads as an entry heading or as the criteria heading,
/// and a blank title, would not survive the Markdown round trip and are
/// refused, as is a delta naming one entry twice or written at propose; a
/// later delta of another change is not held up by any of them.
#[tokio::test(flavor = "multi_thread")]
async fn spec_text_that_would_not_read_back_is_refused() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge(
        "forge-roundtrip",
        &stub,
        json!({ "permissions": {"mode": "acceptEdits"} }),
    )
    .await?;
    let session = run.session().await?;
    let entry = |title: &str, body: &str| {
        json!({ "add": [{ "capability": "ledger", "title": title, "body": body,
                          "criteria": ["Prints the total"] }] })
    };
    stub.script([
        new_change("C-001", json!([])),
        forge("early", "forge flow delta C-001", entry("Totals", "x")),
        forge("a", "forge flow advance C-001", Value::Null),
        forge(
            "heading",
            "forge flow delta C-001",
            entry("Totals", "a\n## S-ledger-900 x\nb"),
        ),
        forge(
            "criteria",
            "forge flow delta C-001",
            entry("Totals", "a\n### 验收\nnot a criterion"),
        ),
        forge("blank", "forge flow delta C-001", entry("  ", "x")),
        forge(
            "twice",
            "forge flow delta C-001",
            json!({ "remove": ["S-ledger-001", "S-ledger-001"] }),
        ),
        new_change("C-002", json!([])),
        forge("a2", "forge flow advance C-002", Value::Null),
        forge("other", "forge flow delta C-002", entry("Totals", "Fine.")),
        Reply::text("done"),
    ]);
    let results = ran(&run.turn(&session, "spec").await?);
    let refused = |i: usize| -> String {
        results[i]
            .clone()
            .expect_err(&format!("call {i} is refused: {results:?}"))
    };
    assert!(refused(1).contains("is at propose"), "{}", refused(1));
    for i in [3, 4] {
        assert!(refused(i).contains("would not read back"), "{}", refused(i));
    }
    assert!(refused(5).contains("/add/0/title"), "{}", refused(5));
    assert!(refused(6).contains("more than once"), "{}", refused(6));
    let other = results[9].as_ref().expect("another change's delta");
    assert_eq!(other["add"], json!(["S-ledger-001"]), "{other}");
    assert!(
        run.read_file("forge/changes/C-001/delta.md").is_none(),
        "nothing refused was written"
    );
    run.finish().await;
    Ok(())
}

/// A failed criterion sends the change back to apply; once the delta is
/// sent again with a changed entry, that entry's passes are gone and the
/// change cannot leave verify until it is verified again.
#[tokio::test(flavor = "multi_thread")]
async fn a_failure_or_a_changed_delta_means_verifying_again() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge(
        "forge-reverify",
        &stub,
        json!({ "permissions": {"mode": "acceptEdits"} }),
    )
    .await?;
    let session = run.session().await?;
    let delta = json!({ "add": [ledger_entry("Totals", &["Sums", "Empty prints nothing"])] });
    let mut steps = vec![new_change("C-001", json!([]))];
    steps.extend(up_to_archive("C-001", delta, &[]));
    steps.pop();
    setup(&mut run, &stub, &session, steps).await?;

    let result = |criterion: &str, pass: bool| json!({ "spec": "S-ledger-001", "criterion": criterion, "pass": pass });
    stub.script([
        forge(
            "fail",
            "forge flow verify C-001",
            json!({ "results": [result("A1", true), result("A2", false)] }),
        ),
        forge("back", "forge flow advance C-001", Value::Null),
        forge(
            "twice",
            "forge flow verify C-001",
            json!({ "results": [result("A1", true), result("A1", true)] }),
        ),
        forge(
            "pass",
            "forge flow verify C-001",
            json!({ "results": [result("A1", true), result("A2", true)] }),
        ),
        forge(
            "redelta",
            "forge flow delta C-001",
            json!({ "add": [ledger_entry("Totals", &["Sums", "An empty file prints 0.00"])] }),
        ),
        forge("stale", "forge flow advance C-001", Value::Null),
        forge("show", "forge flow show C-001", Value::Null),
        Reply::text("done"),
    ]);
    let results = ran(&run.turn(&session, "verify").await?);
    let failed = results[0].as_ref().expect("verify with a failure");
    assert_eq!(failed["stage"], "apply", "{failed}");
    assert_eq!(failed["failed"], json!(["S-ledger-001 A2"]), "{failed}");
    assert_eq!(results[1].as_ref().expect("advance")["stage"], "verify");
    let twice = results[2].as_ref().expect_err("a criterion twice");
    assert!(twice.contains("more than once"), "{twice}");
    assert_eq!(results[3].as_ref().expect("all pass")["failed"], json!([]));
    let redelta = results[4].as_ref().expect("the delta again");
    assert!(
        redelta["say"]
            .as_str()
            .unwrap_or_default()
            .contains("S-ledger-001"),
        "{redelta}"
    );
    let stale = results[5].as_ref().expect_err("passes of the old text");
    assert!(
        stale.contains("criteria without a passing record"),
        "{stale}"
    );
    let shown = results[6].as_ref().expect("show");
    assert_eq!(
        shown["delta"]["add"][0]["criteria"][1]["verified"],
        Value::Null,
        "{shown}"
    );
    run.finish().await;
    Ok(())
}

/// Two open changes modify the same entry: the second to archive is
/// refused rather than overwriting the first, sends its delta again on top
/// of the current entry, verifies again and archives both edits. A removed
/// entry's id is not handed out again, and an archive with a modify and a
/// remove merges both.
#[tokio::test(flavor = "multi_thread")]
async fn changes_to_one_entry_do_not_overwrite_each_other() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge(
        "forge-two",
        &stub,
        json!({ "permissions": {"mode": "acceptEdits"} }),
    )
    .await?;
    let session = run.session().await?;
    let both = [("S-ledger-001", "A1"), ("S-ledger-002", "A1")];
    let mut steps = vec![
        forge(
            "r",
            "forge req add",
            json!({ "title": "Ledger", "body": "Totals.\n" }),
        ),
        new_change("C-001", json!(["R-001"])),
    ];
    steps.extend(up_to_archive(
        "C-001",
        json!({ "add": [ledger_entry("Totals", &["Sums"]), ledger_entry("Months", &["Per month"])] }),
        &both,
    ));
    steps.push(forge("x1", "forge flow archive C-001", Value::Null));
    setup(&mut run, &stub, &session, steps).await?;
    let mut steps = vec![new_change("C-002", json!([]))];
    steps.extend(up_to_archive(
        "C-002",
        json!({ "modify": [{ "id": "S-ledger-001", "body": "Sums by category." }],
                "remove": ["S-ledger-002"] }),
        &[("S-ledger-001", "A1")],
    ));
    steps.push(new_change("C-003", json!([])));
    steps.extend(up_to_archive(
        "C-003",
        json!({ "modify": [{ "id": "S-ledger-001", "title": "Totals per category" }] }),
        &[("S-ledger-001", "A1")],
    ));
    setup(&mut run, &stub, &session, steps).await?;

    stub.script([
        forge("x2", "forge flow archive C-002", Value::Null),
        forge("x3", "forge flow archive C-003", Value::Null),
        forge(
            "again",
            "forge flow delta C-003",
            json!({ "modify": [{ "id": "S-ledger-001", "title": "Totals per category" }] }),
        ),
        forge(
            "v3",
            "forge flow verify C-003",
            json!({ "results": [{ "spec": "S-ledger-001", "criterion": "A1", "pass": true }] }),
        ),
        forge("a3", "forge flow advance C-003", Value::Null),
        forge("x3b", "forge flow archive C-003", Value::Null),
        new_change("C-004", json!([])),
        forge("a4", "forge flow advance C-004", Value::Null),
        forge(
            "d4",
            "forge flow delta C-004",
            json!({ "add": [ledger_entry("Export", &["Writes CSV"])] }),
        ),
        Reply::text("done"),
    ]);
    let results = ran(&run.turn(&session, "archive").await?);
    let merged = results[0].as_ref().expect("archive C-002");
    assert_eq!(
        merged["specs"]["modified"],
        json!(["S-ledger-001"]),
        "{merged}"
    );
    assert_eq!(
        merged["specs"]["removed"],
        json!(["S-ledger-002"]),
        "{merged}"
    );
    let refused = results[1]
        .as_ref()
        .expect_err("C-003 was written against the old entry");
    assert!(
        refused.contains("S-ledger-001 changed after its delta was written")
            && refused.contains("forge flow delta C-003"),
        "{refused}"
    );
    let again = results[2].as_ref().expect("the delta again");
    assert_eq!(again["stage"], "verify", "{again}");
    assert_eq!(
        results[5].as_ref().expect("archive C-003")["stage"],
        "done",
        "{results:?}"
    );
    let added = results[8].as_ref().expect("a new entry");
    assert_eq!(
        added["add"],
        json!(["S-ledger-003"]),
        "a removed id stays used: {added}"
    );

    let spec = run.read_file("forge/specs/ledger.md").unwrap_or_default();
    assert!(
        spec.contains("## S-ledger-001 Totals per category\n")
            && spec.contains("Sums by category."),
        "both edits are kept: {spec}"
    );
    assert!(!spec.contains("S-ledger-002"), "{spec}");
    run.finish().await;
    Ok(())
}

/// An archive that stopped part way — the entry it removes already gone,
/// the archived copy of the change already written — runs again to the end.
#[tokio::test(flavor = "multi_thread")]
async fn an_archive_that_stopped_part_way_runs_again() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge(
        "forge-retry",
        &stub,
        json!({ "permissions": {"mode": "acceptEdits"} }),
    )
    .await?;
    let session = run.session().await?;
    let mut steps = vec![new_change("C-001", json!([]))];
    steps.extend(up_to_archive(
        "C-001",
        json!({ "add": [ledger_entry("Totals", &["Sums"]), ledger_entry("Months", &["Per month"])] }),
        &[("S-ledger-001", "A1"), ("S-ledger-002", "A1")],
    ));
    steps.push(forge("x1", "forge flow archive C-001", Value::Null));
    steps.push(new_change("C-002", json!([])));
    steps.extend(up_to_archive(
        "C-002",
        json!({ "modify": [{ "id": "S-ledger-001", "body": "Sums by category." }],
                "remove": ["S-ledger-002"] }),
        &[("S-ledger-001", "A1")],
    ));
    setup(&mut run, &stub, &session, steps).await?;

    let spec = run.read_file("forge/specs/ledger.md").unwrap_or_default();
    let cut = spec.find("\n\n## S-ledger-002").expect("the second entry");
    std::fs::write(
        run.project.join("forge/specs/ledger.md"),
        format!("{}\n", &spec[..cut]),
    )?;
    let open = run
        .read_file("forge/changes/C-002/change.md")
        .unwrap_or_default();
    std::fs::create_dir_all(run.project.join("forge/archive/C-002"))?;
    std::fs::write(run.project.join("forge/archive/C-002/change.md"), open)?;

    stub.script([
        forge("x2", "forge flow archive C-002", Value::Null),
        Reply::text("done"),
    ]);
    let results = ran(&run.turn(&session, "archive again").await?);
    let done = results[0].as_ref().expect("the archive finishes");
    assert_eq!(done["stage"], "done", "{done}");
    let spec = run.read_file("forge/specs/ledger.md").unwrap_or_default();
    assert!(
        spec.contains("Sums by category.") && !spec.contains("S-ledger-002"),
        "{spec}"
    );
    let archived = run
        .read_file("forge/archive/C-002/change.md")
        .unwrap_or_default();
    assert!(archived.contains("stage: done"), "{archived}");
    run.finish().await;
    Ok(())
}

/// Coverage reads the archived changes only, in the order they were
/// archived: a failure recorded by a change still open does not undo a
/// pass that is already current.
#[tokio::test(flavor = "multi_thread")]
async fn coverage_counts_what_was_archived() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge(
        "forge-coverage",
        &stub,
        json!({ "permissions": {"mode": "acceptEdits"} }),
    )
    .await?;
    let session = run.session().await?;
    let mut unowned = ledger_entry("Months", &["Per month"]);
    unowned["reqs"] = json!([]);
    let mut steps = vec![
        forge(
            "r",
            "forge req add",
            json!({ "items": [{ "title": "Totals", "body": "x" }, { "title": "Export", "body": "y" }] }),
        ),
        forge("u", "forge req update R-002 --status accepted", Value::Null),
        new_change("C-001", json!(["R-001"])),
    ];
    steps.extend(up_to_archive(
        "C-001",
        json!({ "add": [ledger_entry("Totals", &["Sums"]), unowned] }),
        &[("S-ledger-001", "A1"), ("S-ledger-002", "A1")],
    ));
    steps.push(forge("x1", "forge flow archive C-001", Value::Null));
    steps.push(new_change("C-002", json!([])));
    steps.extend(up_to_archive(
        "C-002",
        json!({ "modify": [{ "id": "S-ledger-001", "body": "Sums by category." }] }),
        &[],
    ));
    steps.pop();
    steps.push(forge(
        "v2",
        "forge flow verify C-002",
        json!({ "results": [{ "spec": "S-ledger-001", "criterion": "A1", "pass": false }] }),
    ));
    setup(&mut run, &stub, &session, steps).await?;

    stub.script([
        forge("cov", "forge spec coverage", Value::Null),
        Reply::text("done"),
    ]);
    let out = all_ok(&ran(&run.turn(&session, "coverage").await?));
    assert_eq!(
        out[0],
        json!({
            "accepted_without_specs": [{ "id": "R-002", "title": "Export",
                                         "status": "accepted", "priority": "medium" }],
            "entries_without_reqs": ["S-ledger-002"],
            "criteria_not_passed": [],
        })
    );
    run.finish().await;
    Ok(())
}

/// The guard keeps to the project's `forge/`: a `forge` directory elsewhere
/// is readable, a differently cased `Forge/` and a shell command naming the
/// directory are refused.
#[tokio::test(flavor = "multi_thread")]
async fn the_guard_refuses_forge_and_nothing_else() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge_from(
        stubbed("forge-guard-edges", &stub).before_start(|world, _| {
            world.write_project_file("forge-guard-edges", "forge/reqs/R-001.md", REQ)?;
            world.write_project_file("forge-guard-edges", "src/forge/mod.rs", "pub fn f() {}\n")?;
            Ok(())
        }),
    )
    .await?;
    stub.script([
        Reply::calls("own", "Read", json!({ "file_path": "src/forge/mod.rs" })),
        Reply::calls(
            "cased",
            "Write",
            json!({ "file_path": "Forge/reqs/R-009.md", "content": "x" }),
        ),
        Reply::calls(
            "rm",
            "Bash",
            json!({ "command": "rm -r forge", "description": "remove" }),
        ),
        Reply::text("done"),
    ]);
    let session = run.session().await?;
    let watcher = run.watch(&session, true).await?;
    let events = run.turn(&session, "tidy up").await?;
    let result = |tool: &str| {
        events
            .tool_results
            .iter()
            .find(|(name, _, _)| name == tool)
            .cloned()
            .unwrap_or_else(|| panic!("no {tool} result: {:?}", events.tool_results))
    };
    let (_, text, failed) = result("Read");
    assert!(!failed && text.contains("pub fn f"), "{text}");
    for tool in ["Write", "Bash"] {
        let (_, text, failed) = result(tool);
        assert!(
            failed && text.contains("forge/ is kept by the forge command"),
            "{tool}: {text}"
        );
    }
    assert!(run.read_file("forge/reqs/R-001.md").is_some());
    assert!(run.read_file("forge/reqs/R-009.md").is_none());
    drop(watcher);
    run.finish().await;
    Ok(())
}

/// Help is read-only wherever `--help` is written, so plan mode allows it;
/// a missing argument gets the usage back; files under `reqs/` that are not
/// requirements are skipped; an inherited property name is not a command.
#[tokio::test(flavor = "multi_thread")]
async fn help_and_odd_input_are_answered_plainly() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge_from(
        stubbed("forge-odd", &stub)
            .settings(json!({ "permissions": {"mode": "acceptEdits"} }))
            .before_start(|world, _| {
                world.write_project_file("forge-odd", "forge/reqs/R-001.md", REQ)?;
                world.write_project_file("forge-odd", "forge/reqs/README.md", "Requirements.\n")?;
                Ok(())
            }),
    )
    .await?;
    stub.script([
        forge("h1", "forge --help", Value::Null),
        forge("h2", "forge flow archive --help", Value::Null),
        forge("count", "forge flow show", Value::Null),
        forge("list", "forge req list", Value::Null),
        Reply::text("done"),
    ]);
    let planning = run
        .session_with(json!({ "options": { "settings": { "permissions": { "mode": "plan" } } } }))
        .await?;
    let results = ran(&run.turn(&planning, "help").await?);
    let help = results[0].as_ref().expect("forge --help in plan mode");
    assert!(help.to_string().contains("forge flow archive"), "{help}");
    assert!(results[1].is_ok(), "{results:?}");
    let count = results[2].as_ref().expect_err("no change id");
    assert!(
        count.contains("expected 1 argument") && count.contains("forge flow show <C-id>"),
        "{count}"
    );
    let listed = results[3].as_ref().expect("README.md is not a requirement");
    assert_eq!(
        listed["items"].as_array().map(Vec::len),
        Some(1),
        "{listed}"
    );

    stub.script([
        forge("proto", "forge req constructor", Value::Null),
        Reply::text("done"),
    ]);
    let session = run.session().await?;
    let results = ran(&run.turn(&session, "odd").await?);
    let unknown = results[0].as_ref().expect_err("not a command");
    assert!(unknown.contains("unknown command"), "{unknown}");
    run.finish().await;
    Ok(())
}

/// One program updates a batch of requirements: one `RunCode` call, a
/// `RunExt` per item inside it. The batch is added with `input` sent as JSON
/// text, as models often send it.
#[tokio::test(flavor = "multi_thread")]
async fn a_program_updates_requirements_in_a_batch() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge(
        "forge-batch",
        &stub,
        json!({ "permissions": {"allow": ["RunCode", "RunExt(forge req:*)"]} }),
    )
    .await?;
    let items: Vec<Value> = (1..=3)
        .map(|n| json!({ "title": format!("Item {n}"), "body": "x" }))
        .collect();
    let code = r#"
        const open = await tools.RunExt({ command: "forge req list --status proposed" });
        for (const r of open.items) {
          await tools.RunExt({ command: `forge req update ${r.id} --status accepted` });
        }
        const done = await tools.RunExt({ command: "forge req list --status accepted" });
        return done.items.map((r) => r.id);
    "#;
    stub.script([
        forge(
            "add",
            "forge req add",
            json!(json!({ "items": items }).to_string()),
        ),
        Reply::calls("batch", "RunCode", json!({ "code": code })),
        Reply::text("done"),
    ]);
    let session = run.session().await?;
    let events = run.turn(&session, "accept them all").await?;
    let program = events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "RunCode")
        .expect("the program ran");
    assert!(
        !program.2 && program.1.contains(r#"["R-001","R-002","R-003"]"#),
        "{program:?}"
    );
    for n in 1..=3 {
        let req = run
            .read_file(&format!("forge/reqs/R-00{n}.md"))
            .unwrap_or_default();
        assert!(req.contains("status: accepted"), "{req}");
    }
    run.finish().await;
    Ok(())
}

/// `/forge req list` runs without the model and stays out of the
/// conversation.
#[tokio::test(flavor = "multi_thread")]
async fn the_user_runs_forge_without_the_model() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_forge_from(stubbed("forge-slash", &stub).before_start(|world, _| {
        world.write_project_file("forge-slash", "forge/reqs/R-001.md", REQ)?;
        Ok(())
    }))
    .await?;
    let session = run.session().await?;
    let events = run.turn(&session, "/forge req list").await?;
    assert!(turn_calls(&stub).is_empty(), "{:?}", stub.calls());
    assert!(events.text.contains("Ledger totals"), "{}", events.text);

    stub.script([Reply::text("ok")]);
    run.turn(&session, "what now").await?;
    assert!(!turn_calls(&stub)[0]
        .messages_text()
        .contains("Ledger totals"));
    run.finish().await;
    Ok(())
}

/// A copy of forge whose `writes` covers only `forge/reqs/**`: a change
/// cannot be opened, because its write falls outside the scope.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_outside_the_declared_scope_fails() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("forge-scope", &stub)
        .settings(json!({ "permissions": {"mode": "acceptEdits"} }))
        .start()
        .await?;
    let narrowed = tempfile::tempdir()?;
    copy_dir(&forge_package_dir(), narrowed.path())?;
    let manifest = narrowed.path().join("plugin.toml");
    let text = std::fs::read_to_string(&manifest)?;
    std::fs::write(
        &manifest,
        text.replace(r#"writes = ["forge/**"]"#, r#"writes = ["forge/reqs/**"]"#),
    )?;
    let (archive, _) = package_dir(narrowed.path(), run.world.root(), FORGE_PACKAGE)?;
    let installed = install(&mut run, &archive).await?;
    assert_eq!(installed["success"], true, "{installed}");
    stub.script([
        forge(
            "add",
            "forge req add",
            json!({ "title": "Kept", "body": "x" }),
        ),
        forge(
            "new",
            "forge flow new",
            json!({ "title": "Escapes", "body": "x" }),
        ),
        Reply::text("done"),
    ]);
    let session = run.session().await?;
    let results = ran(&run.turn(&session, "go").await?);
    assert!(results[0].is_ok(), "{results:?}");
    let refused = results[1].as_ref().expect_err("a write outside writes");
    assert!(
        refused.contains("may write only forge/reqs/**"),
        "{refused}"
    );
    assert!(run.read_file("forge/reqs/R-001.md").is_some());
    assert!(!run.project.join("forge/changes").exists());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forge_package_carrying_bytecode_is_refused() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("forge-bytecode", &stub).start().await?;
    let compiled = tempfile::tempdir()?;
    copy_dir(&forge_package_dir(), compiled.path())?;
    std::fs::write(
        compiled.path().join("forge.js"),
        b"\x05\x02\x00\x00main\x00\x9c\xfe\x01",
    )?;
    let (archive, _) = package_dir(compiled.path(), run.world.root(), FORGE_PACKAGE)?;
    let installed = install(&mut run, &archive).await?;
    let error = installed["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("never compiled QuickJS bytecode"),
        "{installed}"
    );
    run.finish().await;
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
