//! A session narrowing the plugins it draws on with its own
//! `options.settings.plugins.allow` / `plugins.enabled`, over
//! `Embedded::serve_stream` with the host's own packages: what a package left
//! out contributes to that session — its `prompt.block`, its `tool.around` in
//! the session and in a sub-agent, its commands — against a session that
//! narrows nothing, and what `session.create`'s `clamped_options` and
//! `session.inspectContext` report. Only what the RPC and the model's
//! requests show.

use std::path::PathBuf;

use blackbox::{ProviderStub, Reply};
use daemon::embed::{EmbedConfig, Embedded};
use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};

struct Root {
    _dir: tempfile::TempDir,
    data: PathBuf,
    project: PathBuf,
    packages: PathBuf,
}

fn root(stub: &ProviderStub, plugins: Value) -> anyhow::Result<Root> {
    let dir = tempfile::tempdir()?;
    let base = dir.path().canonicalize()?;
    let (data, project, packages) = (base.join("data"), base.join("project"), base.join("host"));
    for d in [&data, &project, &packages] {
        std::fs::create_dir_all(d)?;
    }
    let model = "claude-sonnet-4-6";
    let settings = json!({
        "providers": [{
            "id": "stub",
            "api": "anthropic",
            "base_url": stub.base_url().as_str(),
            "tiers": {"strong": model, "balanced": model, "fast": model},
            "models": [model]
        }],
        "permissions": {"mode": "acceptEdits", "allow": ["Agent"]},
        "sandbox": {"network": {"mode": "deny_all"}},
        "memory": { "enabled": false },
        "plugins": plugins,
    });
    std::fs::write(data.join("settings.json"), serde_json::to_vec(&settings)?)?;
    std::fs::write(
        data.join("settings.local.json"),
        serde_json::to_vec(&json!({ "app": { "trusted_projects": [project] } }))?,
    )?;
    for name in ["a", "b"] {
        std::fs::write(
            project.join(format!("guard-{name}.txt")),
            format!("GUARD-{name}-CONTENT"),
        )?;
    }
    Ok(Root {
        _dir: dir,
        data,
        project,
        packages,
    })
}

/// A package `name` with a `prompt.block`, a `tool.around` that refuses a
/// `Read` of `guard-<name>.txt`, a command `cmd-<name>` and a command `same`,
/// and `extra` manifest lines.
fn package(root: &Root, name: &str, extra: &str) -> anyhow::Result<PathBuf> {
    let dir = root.packages.join(name);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "[plugin]\nname = \"{name}\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n{extra}\n\
             [[command]]\nname = \"cmd-{name}\"\nentry = \"cmd.js:main\"\n\
             description = \"own command of {name}\"\n\
             [[command]]\nname = \"same\"\nentry = \"cmd.js:main\"\n\
             description = \"same from {name}\"\n\
             [[script]]\npoint = \"prompt.block\"\nentry = \"block.js:onBlock\"\n\
             timeout_ms = 2000\n\
             [[script]]\npoint = \"tool.around\"\nentry = \"guard.js:onAround\"\n\
             timeout_ms = 2000\ncalls_per_turn = 1000\n"
        ),
    )?;
    std::fs::write(dir.join("cmd.js"), "function main() { return 'ran'; }\n")?;
    std::fs::write(
        dir.join("block.js"),
        format!(
            "function onBlock() {{\n  return {{ name: \"{name}.block\", order: 470, \
             content: \"BLOCK-OF-{name}\" }};\n}}\n"
        ),
    )?;
    std::fs::write(
        dir.join("guard.js"),
        format!(
            "function onAround(call) {{\n  const path = String((call.input && \
             call.input.file_path) || \"\");\n  if (call.tool === \"Read\" && \
             path.endsWith(\"/guard-{name}.txt\")) {{\n    return {{ action: \"deny\", \
             reason: \"denied-by-{name}\" }};\n  }}\n  return null;\n}}\n"
        ),
    )?;
    Ok(dir)
}

async fn start(root: &Root, packages: Vec<PathBuf>) -> anyhow::Result<(Embedded, DaemonRpcClient)> {
    let mut config = EmbedConfig::new(&root.data);
    config.credentials = Some(blackbox::stub_credentials());
    config.cwd = Some(root.project.clone());
    config.plugins = packages;
    let embedded = Embedded::start(config).await?;
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    embedded.serve_stream(theirs);
    let client =
        DaemonRpcClient::with_transport(Box::new(rpc_client::transport::stream(ours, "embed")));
    Ok((embedded, client))
}

/// `session.create`'s whole answer for a session with `settings` as its own.
async fn create(client: &mut DaemonRpcClient, settings: Option<Value>) -> anyhow::Result<Value> {
    let mut options = json!({ "attended": false, "auto_name": false });
    if let Some(settings) = settings {
        options["settings"] = settings;
    }
    let resp = client
        .call("session.create", json!({ "options": options }))
        .await?;
    anyhow::ensure!(resp.error.is_none(), "session.create: {:?}", resp.error);
    Ok(resp.result.unwrap_or_default())
}

fn clamped(created: &Value) -> Value {
    created
        .get("clamped_options")
        .cloned()
        .unwrap_or_else(|| json!([]))
}

fn id(created: &Value) -> String {
    created["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

async fn inspect(client: &mut DaemonRpcClient, session: &str) -> anyhow::Result<Value> {
    let resp = client
        .call("session.inspectContext", json!({ "session_id": session }))
        .await?;
    anyhow::ensure!(resp.error.is_none(), "inspectContext: {:?}", resp.error);
    Ok(resp.result.unwrap_or_default())
}

fn plugin_names(report: &Value) -> Vec<String> {
    report["plugins"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["name"].as_str().map(str::to_string))
        .collect()
}

fn system_text(report: &Value) -> String {
    report["system"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `(name, description)` of each plugin command `session` lists.
async fn plugin_commands(
    client: &mut DaemonRpcClient,
    session: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let resp = client
        .call("commands.list", json!({ "session_id": session }))
        .await?;
    anyhow::ensure!(resp.error.is_none(), "commands.list: {:?}", resp.error);
    Ok(resp.result.unwrap_or_default()["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["source"] == "plugin")
        .map(|c| {
            (
                c["name"].as_str().unwrap_or_default().to_string(),
                c["description"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect())
}

/// One turn of `session` in which the model reads `guard-b.txt` and has a
/// sub-agent read it too; what the session's `Read` answered and the
/// sub-agent's next request, which carries what its `Read` answered.
async fn read_guarded_b(
    stub: &ProviderStub,
    client: &mut DaemonRpcClient,
    root: &Root,
    session: &str,
    tag: &str,
) -> anyhow::Result<(String, String)> {
    let guarded = root.project.join("guard-b.txt");
    let (mark, child) = (format!("MAIN-{tag}"), format!("KID-{tag}"));
    let child_read = format!("r-child-{tag}");
    stub.script_when(
        &mark,
        [
            Reply::calls(
                "r-main",
                "Read",
                json!({ "file_path": guarded.to_string_lossy() }),
            ),
            Reply::calls(
                "a-main",
                "Agent",
                json!({ "prompt": format!("{child} read it"), "description": "reader" }),
            ),
            Reply::text("main done"),
        ],
    );
    stub.script_when(
        &child,
        [
            Reply::calls(
                &child_read,
                "Read",
                json!({ "file_path": guarded.to_string_lossy() }),
            ),
            Reply::text("child done"),
        ],
    );
    let events = client
        .session_run_turn(session, &format!("{mark} go"), tag, None)
        .await?;
    anyhow::ensure!(events.turn_complete, "{events:?}");
    let main = events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "Read")
        .map(|(_, content, _)| content.clone())
        .unwrap_or_default();
    let child = stub
        .calls()
        .into_iter()
        .map(|c| c.messages_text())
        .find(|text| text.contains(&child) && text.contains(&child_read))
        .unwrap_or_default();
    Ok((main, child))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_allowing_one_package_gets_nothing_from_the_other() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let packages = vec![package(&root, "a", "")?, package(&root, "b", "")?];
    let (embedded, mut client) = start(&root, packages).await?;

    let narrowed = create(&mut client, Some(json!({"plugins": {"allow": ["a"]}}))).await?;
    assert_eq!(clamped(&narrowed), json!([]), "{narrowed}");
    let narrowed = id(&narrowed);
    let report = inspect(&mut client, &narrowed).await?;
    assert_eq!(plugin_names(&report), ["a"], "{report}");
    let system = system_text(&report);
    assert!(system.contains("BLOCK-OF-a"), "{system}");
    assert!(!system.contains("BLOCK-OF-b"), "{system}");
    let commands = plugin_commands(&mut client, &narrowed).await?;
    let names: Vec<&str> = commands.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"cmd-a"), "{commands:?}");
    assert!(!names.contains(&"cmd-b"), "{commands:?}");

    let whole = id(&create(&mut client, None).await?);
    let report = inspect(&mut client, &whole).await?;
    assert_eq!(plugin_names(&report), ["a", "b"], "{report}");
    let system = system_text(&report);
    assert!(
        system.contains("BLOCK-OF-a") && system.contains("BLOCK-OF-b"),
        "{system}"
    );
    let commands = plugin_commands(&mut client, &whole).await?;
    let names: Vec<&str> = commands.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"cmd-a") && names.contains(&"cmd-b"),
        "{commands:?}"
    );

    let (main, child) = read_guarded_b(&stub, &mut client, &root, &narrowed, "NARROWED").await?;
    assert!(
        main.contains("GUARD-b-CONTENT"),
        "b's tool.around ran: {main}"
    );
    assert!(
        child.contains("GUARD-b-CONTENT") && !child.contains("denied-by-b"),
        "b's tool.around ran in the sub-agent: {child}"
    );
    let (main, child) = read_guarded_b(&stub, &mut client, &root, &whole, "WHOLE").await?;
    assert!(
        main.contains("denied-by-b"),
        "b's tool.around did not run: {main}"
    );
    assert!(
        child.contains("denied-by-b"),
        "b's tool.around did not run in the sub-agent: {child}"
    );
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_the_pool_lacks_is_reported_and_the_rest_apply() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let packages = vec![package(&root, "a", "")?, package(&root, "b", "")?];
    let (embedded, mut client) = start(&root, packages).await?;

    let created = create(
        &mut client,
        Some(json!({"plugins": {"allow": ["a", "ghost"]}})),
    )
    .await?;
    assert_eq!(
        clamped(&created),
        json!([{
            "field": "settings.plugins.allow",
            "requested": ["a", "ghost"],
            "applied": ["a"],
        }]),
        "{created}"
    );
    let report = inspect(&mut client, &id(&created)).await?;
    assert_eq!(plugin_names(&report), ["a"], "{report}");

    let created = create(&mut client, Some(json!({"plugins": {"allow": ["ghost"]}}))).await?;
    let report = inspect(&mut client, &id(&created)).await?;
    assert_eq!(
        plugin_names(&report),
        Vec::<String>::new(),
        "a list of nothing the pool has is no plugin, not every plugin: {report}"
    );
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_list_is_held_to_what_the_pool_lets_load() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({"allow": ["a", "b"]}))?;
    let packages = vec![
        package(&root, "a", "")?,
        package(&root, "b", "")?,
        package(&root, "c", "")?,
    ];
    let (embedded, mut client) = start(&root, packages).await?;

    let created = create(&mut client, Some(json!({"plugins": {"allow": ["a", "c"]}}))).await?;
    assert_eq!(
        clamped(&created),
        json!([{
            "field": "settings.plugins.allow",
            "requested": ["a", "c"],
            "applied": ["a"],
        }]),
        "{created}"
    );
    let report = inspect(&mut client, &id(&created)).await?;
    assert_eq!(plugin_names(&report), ["a"], "{report}");
    assert!(!system_text(&report).contains("BLOCK-OF-c"));
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn enabled_false_leaves_a_session_no_host_package() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let packages = vec![package(&root, "a", "")?];
    let (embedded, mut client) = start(&root, packages).await?;
    let listed = client.call("plugin.list", json!({})).await?;
    assert!(
        listed
            .result
            .unwrap_or_default()
            .to_string()
            .contains("\"host\""),
        "a is the host's own package"
    );

    let created = create(&mut client, Some(json!({"plugins": {"enabled": false}}))).await?;
    assert_eq!(clamped(&created), json!([]), "{created}");
    let session = id(&created);
    let report = inspect(&mut client, &session).await?;
    assert_eq!(plugin_names(&report), Vec::<String>::new(), "{report}");
    assert!(!system_text(&report).contains("BLOCK-OF-a"));
    assert_eq!(plugin_commands(&mut client, &session).await?, []);
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dependency_of_an_allowed_package_comes_along() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let packages = vec![
        package(&root, "a", "[dependencies]\nb = \"^1.0\"\n")?,
        package(&root, "b", "")?,
        package(&root, "c", "")?,
    ];
    let (embedded, mut client) = start(&root, packages).await?;

    let created = create(&mut client, Some(json!({"plugins": {"allow": ["a"]}}))).await?;
    assert_eq!(clamped(&created), json!([]), "{created}");
    let report = inspect(&mut client, &id(&created)).await?;
    assert_eq!(plugin_names(&report), ["a", "b"], "{report}");
    assert_eq!(
        report["plugins"][1]["required_by"],
        json!(["a"]),
        "{report}"
    );
    assert!(
        report["plugins"][0].get("required_by").is_none(),
        "{report}"
    );
    let system = system_text(&report);
    assert!(system.contains("BLOCK-OF-b"), "{system}");
    assert!(!system.contains("BLOCK-OF-c"), "{system}");
    embedded.shutdown().await;
    Ok(())
}

/// Two packages declare `same`; the first by name has it pool-wide. A
/// session that leaves that one out gets the other's.
#[tokio::test(flavor = "multi_thread")]
async fn leaving_out_a_commands_owner_hands_the_name_to_the_other() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let packages = vec![package(&root, "a", "")?, package(&root, "b", "")?];
    let (embedded, mut client) = start(&root, packages).await?;

    let description = |commands: &[(String, String)]| {
        commands
            .iter()
            .find(|(name, _)| name == "same")
            .map(|(_, d)| d.clone())
    };
    let whole = id(&create(&mut client, None).await?);
    let commands = plugin_commands(&mut client, &whole).await?;
    assert_eq!(
        description(&commands).as_deref(),
        Some("same from a"),
        "{commands:?}"
    );
    let only_b = id(&create(&mut client, Some(json!({"plugins": {"allow": ["b"]}}))).await?);
    let commands = plugin_commands(&mut client, &only_b).await?;
    assert_eq!(
        description(&commands).as_deref(),
        Some("same from b"),
        "{commands:?}"
    );
    embedded.shutdown().await;
    Ok(())
}
