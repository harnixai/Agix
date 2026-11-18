//! The `forge` example plugin used by a real model: a change driven stage by
//! stage the way a user would, from a requirement to an archived change and
//! a Rust program whose tests the harness runs itself; auto mode stopping
//! before apply; requirements kept on an MCP backend; the guard on `forge/`;
//! and a report of what forge costs against the same task without it.
//! Asserted on the project, the toy's store, the requests the proxy recorded
//! and what `/forge` (run without the model) reports.

use std::path::Path;
use std::process::Command;

use blackbox::{LiveRun, LiveRunBuilder, McpToy};
use serde_json::{json, Value};
use test_runner::plugin_fixture::{file_url, package_forge, FORGE_PACKAGE, FORGE_PACKAGE_VERSION};

const LEDGER: &str = "Build `ledger`, a command-line program in Rust, as a Cargo package named \
ledger in this project's root; standard library only, no crates. It reads a ledger file in CSV: \
the header line `date,amount,category,memo`, then one transaction per line. date is YYYY-MM-DD; \
amount is a decimal with at most two digits after the point and may be negative; memo may be \
wrapped in double quotes and then may contain commas. Commands: `ledger balance FILE` prints \
`balance: <total>`; `ledger summary FILE` prints `<category>: <total>` per category, sorted by \
category; `ledger month FILE YYYY-MM` prints that month's transactions and then \
`total: <total>`; `ledger check FILE` prints `line <n>: <reason>` for every malformed line and \
exits with status 1 if there is one, 0 otherwise. Totals have two digits after the point \
(`-3.50`). Money is counted in cents, never in floating point. Unit tests and an integration \
test that runs the binary cover every command.";

async fn with_forge(builder: LiveRunBuilder) -> anyhow::Result<LiveRun> {
    let mut run = builder.start().await?;
    let (archive, checksum) = package_forge(run.world.root())?;
    let installed = run
        .client
        .call(
            "plugin.install",
            json!({ "name": FORGE_PACKAGE, "version": FORGE_PACKAGE_VERSION,
                    "download_url": file_url(&archive), "checksum": checksum }),
        )
        .await?;
    anyhow::ensure!(installed.error.is_none(), "plugin.install: {installed:?}");
    Ok(run)
}

/// One user message; the tool calls that failed are printed, since what the
/// model did next depends on them.
async fn step(
    run: &mut LiveRun,
    session: &str,
    message: &str,
) -> anyhow::Result<rpc_client::TurnEvents> {
    let events = run.turn(session, message).await?;
    for (name, text, failed) in &events.tool_results {
        if *failed {
            let text: String = text.chars().take(400).collect();
            eprintln!("  ! {name}: {}", text.replace('\n', "\n    "));
        }
    }
    Ok(events)
}

/// The open or archived change ids under `forge/`.
fn changes(project: &Path) -> Vec<String> {
    let mut ids: Vec<String> = ["changes", "archive"]
        .iter()
        .flat_map(|d| {
            std::fs::read_dir(project.join("forge").join(d))
                .into_iter()
                .flatten()
        })
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("C-"))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// A change's stage as its file says; `done` once archived.
fn stage(project: &Path, id: &str) -> String {
    let open = std::fs::read_to_string(project.join(format!("forge/changes/{id}/change.md")))
        .unwrap_or_default();
    if open.is_empty() || open.contains("archived: true") {
        return if project
            .join(format!("forge/archive/{id}/change.md"))
            .exists()
        {
            "done".into()
        } else {
            "none".into()
        };
    }
    open.lines()
        .find_map(|l| l.strip_prefix("stage: "))
        .unwrap_or("none")
        .trim()
        .to_string()
}

/// What `/forge <args>` prints: run as the user would, without the model.
async fn forge(run: &mut LiveRun, session: &str, args: &str) -> anyhow::Result<Value> {
    let events = run.turn(session, &format!("/forge {args}")).await?;
    serde_json::from_str(events.text.trim())
        .map_err(|e| anyhow::anyhow!("/forge {args} printed {:?}: {e}", events.text))
}

/// Every call any model in the run made straight at `forge/` — not through
/// the command — that came back without an error, from the requests the
/// proxy recorded (sub-agents' included).
fn direct_forge_access(run: &LiveRun) -> Vec<String> {
    let mut uses = std::collections::BTreeMap::new();
    let mut failed = std::collections::BTreeMap::new();
    for call in run.provider.calls() {
        for message in call.body["messages"].as_array().into_iter().flatten() {
            for block in message["content"].as_array().into_iter().flatten() {
                match block["type"].as_str() {
                    Some("tool_use") => {
                        uses.insert(
                            block["id"].as_str().unwrap_or_default().to_string(),
                            (block["name"].clone(), block["input"].clone()),
                        );
                    }
                    Some("tool_result") => {
                        failed.insert(
                            block["tool_use_id"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                            block["is_error"] == true,
                        );
                    }
                    _ => {}
                }
            }
        }
    }
    uses.into_iter()
        .filter(|(id, (name, input))| {
            let direct = ["Read", "Write", "Edit", "Glob", "Grep", "Bash"]
                .contains(&name.as_str().unwrap_or_default());
            let named = ["file_path", "path", "pattern", "command"]
                .iter()
                .filter_map(|k| input[*k].as_str())
                .any(|v| {
                    !v.contains("/plugins/cache/")
                        && (v.contains("forge/") || v == "forge" || v.ends_with("/forge"))
                });
            direct && named && failed.get(id) == Some(&false)
        })
        .map(|(_, (name, input))| format!("{name} {input}"))
        .collect()
}

fn rust_lines(project: &Path) -> usize {
    fn walk(dir: &Path, n: &mut usize) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, n);
            } else if path.extension().is_some_and(|e| e == "rs") {
                *n += std::fs::read_to_string(&path)
                    .unwrap_or_default()
                    .lines()
                    .count();
            }
        }
    }
    let mut n = 0;
    walk(&project.join("src"), &mut n);
    walk(&project.join("tests"), &mut n);
    n
}

fn cargo(project: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO"))
        .args(args)
        .current_dir(project)
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .expect("cargo runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// The main case: one user message per stage — `/forge-propose` with the
/// requirement, then `/forge-next <change>` until the change is archived —
/// in the default permission mode with the host approving.
#[tokio::test(flavor = "multi_thread")]
async fn live_forge_manual_flow_builds_a_rust_program() -> anyhow::Result<()> {
    let mut run = with_forge(LiveRun::builder("forge-manual")).await?;
    let session = run.session().await?;
    let host = run.watch(&session, true).await?;

    step(&mut run, &session, &format!("/forge-propose {LEDGER}")).await?;
    let id = changes(&run.project)
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("propose opened no change"))?;
    let mut stages = vec![stage(&run.project, &id)];
    for _ in 0..8 {
        if stage(&run.project, &id) == "done" {
            break;
        }
        step(&mut run, &session, &format!("/forge-next {id}")).await?;
        stages.push(stage(&run.project, &id));
    }
    eprintln!("[live forge-manual] stages after each turn: {stages:?}");
    assert_eq!(stage(&run.project, &id), "done", "{stages:?}");

    let (built, log) = cargo(&run.project, &["build", "--quiet"]);
    assert!(built, "cargo build: {log}");
    let (tested, log) = cargo(&run.project, &["test", "--quiet"]);
    assert!(tested, "cargo test: {log}");
    let lines = rust_lines(&run.project);
    eprintln!("[live forge-manual] {lines} lines of Rust");
    assert!((150..=3000).contains(&lines), "{lines} lines of Rust");

    let shown = forge(&mut run, &session, &format!("flow show {id}")).await?;
    assert_eq!(shown["stage"], "done", "{shown}");
    let entries: Vec<&Value> = ["add", "modify"]
        .iter()
        .flat_map(|k| shown["delta"][*k].as_array().into_iter().flatten())
        .collect();
    assert!(!entries.is_empty(), "the change has spec entries: {shown}");
    let specs: String = std::fs::read_dir(run.project.join("forge/specs"))?
        .flatten()
        .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
        .collect();
    for entry in &entries {
        let sid = entry["id"].as_str().unwrap_or_default();
        assert!(
            specs.contains(&format!("## {sid} ")),
            "{sid} is not in the specs"
        );
        for criterion in entry["criteria"].as_array().into_iter().flatten() {
            assert_eq!(
                criterion["verified"]["pass"], true,
                "{sid} {} has no passing record: {shown}",
                criterion["id"]
            );
        }
    }
    for req in shown["reqs"].as_array().into_iter().flatten() {
        let req = forge(
            &mut run,
            &session,
            &format!("req get {}", req.as_str().unwrap_or_default()),
        )
        .await?;
        assert_eq!(req["status"], "done", "{req}");
    }
    assert!(run
        .project
        .join(format!("forge/archive/{id}/delta.md"))
        .exists());

    let direct = direct_forge_access(&run);
    assert!(
        direct.is_empty(),
        "the model got at forge/ directly: {direct:?}"
    );
    let asked = host.asked().len();
    eprintln!("[live forge-manual] {asked} prompts approved by the host");
    drop(host);
    run.finish().await;
    Ok(())
}

/// With `flow.auto` and `stop_before: [apply]`, one `/forge-propose` carries
/// the change through spec and plan and stops before apply.
#[tokio::test(flavor = "multi_thread")]
async fn live_forge_auto_mode_stops_before_apply() -> anyhow::Result<()> {
    let mut run = with_forge(LiveRun::builder("forge-auto").before_start(|world, _| {
        world.write_project_file(
            "forge-auto",
            "forge/config.yaml",
            "flow:\n  auto: true\n  stop_before: [apply]\n",
        )?;
        Ok(())
    }))
    .await?;
    let session = run.session().await?;
    let host = run.watch(&session, true).await?;
    step(
        &mut run,
        &session,
        "/forge-propose A Rust library crate `slug` in this project's root with one function \
         `slugify(&str) -> String`: lowercase ASCII letters and digits are kept, every other run \
         of characters becomes a single `-`, and no `-` at either end.",
    )
    .await?;
    let id = changes(&run.project).into_iter().next().expect("a change");
    assert_eq!(stage(&run.project, &id), "apply", "stopped before apply");
    let shown = forge(&mut run, &session, &format!("flow show {id}")).await?;
    let plan = shown["plan"].as_array().cloned().unwrap_or_default();
    assert!(!plan.is_empty(), "{shown}");
    assert!(
        plan.iter().all(|p| p["done"] == false),
        "nothing applied yet: {shown}"
    );
    assert!(
        !shown["delta"]["add"]
            .as_array()
            .unwrap_or(&vec![])
            .is_empty(),
        "{shown}"
    );
    drop(host);
    run.finish().await;
    Ok(())
}

/// Requirements on an MCP backend (the toy's `--store`): `/forge-req` records
/// them there, not in `forge/reqs`.
#[tokio::test(flavor = "multi_thread")]
async fn live_forge_requirements_live_on_the_remote_backend() -> anyhow::Result<()> {
    let store = tempfile::tempdir()?;
    let toy = McpToy::stdio_config(&[
        "--name",
        "reqdb",
        "--store",
        &store.path().to_string_lossy(),
    ])?;
    let allow: Vec<Value> = ["items_query", "items_get", "items_put"]
        .iter()
        .map(|t| json!({ "tool": format!("mcp__reqdb__{t}"), "action": "allow" }))
        .collect();
    let mut run = with_forge(
        LiveRun::builder("forge-remote")
            .settings(json!({ "mcp_servers": { "reqdb": toy }, "permission_rules": allow }))
            .before_start(|world, _| {
                world.write_project_file(
                    "forge-remote",
                    "forge/config.yaml",
                    "reqs: { backend: mcp, server: reqdb }\n",
                )?;
                Ok(())
            }),
    )
    .await?;
    let session = run.session().await?;
    let host = run.watch(&session, true).await?;
    step(
        &mut run,
        &session,
        "/forge-req Record three requirements: importing a bank CSV export (high priority), \
         a monthly report (medium), dark mode (low). Then mark the import one accepted and \
         list what is recorded.",
    )
    .await?;
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(store.path().join("req.json"))?)?;
    let items = stored.as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 3, "{stored}");
    assert_eq!(
        items.iter().filter(|r| r["status"] == "accepted").count(),
        1,
        "{stored}"
    );
    assert!(
        !run.project.join("forge/reqs").exists(),
        "requirements stay remote"
    );
    let listed = forge(&mut run, &session, "req list").await?;
    assert_eq!(
        listed["items"].as_array().map(Vec::len),
        Some(3),
        "{listed}"
    );
    drop(host);
    run.finish().await;
    Ok(())
}

/// Asked to open a file under `forge/`, the model is refused if it tries,
/// with the command to use, and gets the content through the command.
#[tokio::test(flavor = "multi_thread")]
async fn live_forge_direct_reads_are_refused_and_the_command_answers() -> anyhow::Result<()> {
    let word = format!(
        "harbor-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
            % 100_000
    );
    let req = format!(
        "---\nid: R-001\ntitle: Export\nstatus: proposed\npriority: high\nspecs: []\nchanges: []\n\
         ---\nThe code word is {word}.\n"
    );
    let mut run = with_forge(
        LiveRun::builder("forge-guard").before_start(move |world, _| {
            world.write_project_file("forge-guard", "forge/reqs/R-001.md", &req)?;
            Ok(())
        }),
    )
    .await?;
    let session = run.session().await?;
    let host = run.watch(&session, true).await?;
    let events = step(
        &mut run,
        &session,
        "Open the file forge/reqs/R-001.md with the Read tool and tell me the code word \
             written in it.",
    )
    .await?;
    let refused: Vec<&(String, String, bool)> = events
        .tool_results
        .iter()
        .filter(|(name, text, failed)| name == "Read" && *failed && text.contains("forge/"))
        .collect();
    eprintln!("[live forge-guard] direct reads refused: {}", refused.len());
    for (_, text, _) in &refused {
        assert!(text.contains("forge req get"), "{text}");
    }
    assert!(events.text.contains(&word), "{}", events.text);
    assert!(direct_forge_access(&run).is_empty());
    drop(host);
    run.finish().await;
    Ok(())
}

/// No assertion: the same small task with forge (auto mode, nowhere to
/// stop) and without it, the model calls and prompt tokens of each printed.
#[tokio::test(flavor = "multi_thread")]
async fn live_forge_cost_against_no_forge() -> anyhow::Result<()> {
    const TASK: &str = "A Rust library crate `roman` in this project's root: `to_roman(u32) -> \
        Option<String>` for 1..=3999 (None otherwise) and `from_roman(&str) -> Option<u32>` that \
        accepts only canonical numerals; unit tests for both; cargo test passes.";
    let mut report = Vec::new();
    for with in [true, false] {
        let name = if with {
            "forge-cost-with"
        } else {
            "forge-cost-without"
        };
        let mut run = if with {
            with_forge(LiveRun::builder(name).before_start(move |world, _| {
                world.write_project_file(name, "forge/config.yaml", "flow:\n  auto: true\n")?;
                Ok(())
            }))
            .await?
        } else {
            LiveRun::builder(name).start().await?
        };
        let session = run.session().await?;
        let host = run.watch(&session, true).await?;
        let message = if with {
            format!("/forge-propose {TASK}")
        } else {
            TASK.to_string()
        };
        step(&mut run, &session, &message).await?;
        let (calls, input, output) = run.provider.spend();
        let tested = cargo(&run.project, &["test", "--quiet"]).0;
        report.push(format!(
            "{name}: {calls} model calls, {input} prompt tokens, {output} output tokens, \
             cargo test {}",
            if tested { "passes" } else { "fails" }
        ));
        drop(host);
        run.finish_allowing_refusals().await;
    }
    eprintln!("[live forge-cost]\n  {}", report.join("\n  "));
    Ok(())
}
