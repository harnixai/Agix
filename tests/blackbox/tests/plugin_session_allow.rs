//! A session narrowing the plugins it draws on with its own
//! `options.settings.plugins.allow` / `plugins.enabled`, over
//! `Embedded::serve_stream` with the host's own packages: what a package left
//! out contributes to that session — its `prompt.block`, its `tool.around` in
//! the session and in a sub-agent, its commands, agent types, skills, MCP
//! servers, functions and scene — against a session that narrows nothing,
//! and what `session.create`'s `clamped_options` and
//! `session.inspectContext` report. Only what the RPC and the model's
//! requests show.

use std::path::PathBuf;

use blackbox::{McpToy, ProviderStub, Reply, SeenRequest};
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
        "permissions": {"mode": "acceptEdits", "allow": ["Agent", "Skill"]},
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

fn write(dir: &std::path::Path, rel: &str, content: &str) -> anyhow::Result<()> {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

fn tool_description(request: &SeenRequest, name: &str) -> String {
    request.body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["name"] == name)
        .and_then(|t| t["description"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn whole_text(request: &SeenRequest) -> String {
    format!("{}\n{}", request.system_text(), request.messages_text())
}

/// What one turn of a session handing `KID-<tag>` to a sub-agent showed: the
/// turn's events, its first request, and the sub-agent's first request when
/// one was made.
struct Delegated {
    events: rpc_client::TurnEvents,
    lead: SeenRequest,
    kid: Option<SeenRequest>,
}

async fn delegate(
    stub: &ProviderStub,
    client: &mut DaemonRpcClient,
    session: &str,
    tag: &str,
    subagent_type: &str,
) -> anyhow::Result<Delegated> {
    let (mark, child) = (format!("LEAD-{tag}"), format!("KID-{tag}"));
    stub.script_when(
        &mark,
        [
            Reply::calls(
                &format!("a-{tag}"),
                "Agent",
                json!({
                    "prompt": format!("{child} look around"),
                    "description": "helper",
                    "subagent_type": subagent_type,
                }),
            ),
            Reply::text("lead done"),
        ],
    );
    stub.script_when(&child, [Reply::text("kid done")]);
    let events = client
        .session_run_turn(session, &format!("{mark} go"), tag, None)
        .await?;
    anyhow::ensure!(events.turn_complete, "{events:?}");
    let calls = stub.calls();
    let lead = calls
        .iter()
        .find(|c| c.messages_contain(&mark))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no request of the turn {tag}"))?;
    let kid = calls
        .iter()
        .find(|c| c.messages_contain(&child) && !c.messages_contain(&mark))
        .cloned();
    Ok(Delegated { events, lead, kid })
}

/// b's `explore` takes the built-in's name pool-wide, and b has a type of its
/// own. A session leaving b out lists neither of b's, its sub-agent of type
/// `explore` runs on the built-in definition again, and one of b's own type
/// is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_left_out_packages_agent_types_are_gone_and_the_built_in_comes_back() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let a = package(&root, "a", "")?;
    let b = package(
        &root,
        "b",
        "[[agent]]\nname = \"explore\"\ndescription = \"b's own explorer\"\n\
         prompt = \"agents/explore.md\"\n\
         [[agent]]\nname = \"b-helper\"\ndescription = \"b's helper type\"\n\
         prompt = \"agents/helper.md\"\n",
    )?;
    write(&b, "agents/explore.md", "PROMPT-OF-b-explore")?;
    write(&b, "agents/helper.md", "PROMPT-OF-b-helper")?;
    let (embedded, mut client) = start(&root, vec![a, b]).await?;

    let narrowed = id(&create(&mut client, Some(json!({"plugins": {"allow": ["a"]}}))).await?);
    let turn = delegate(&stub, &mut client, &narrowed, "N-EXPLORE", "explore").await?;
    let listed = tool_description(&turn.lead, "Agent");
    assert!(
        listed.contains("Read-only file search and exploration specialist"),
        "{listed}"
    );
    assert!(
        !listed.contains("b's own explorer") && !listed.contains("b-helper"),
        "{listed}"
    );
    let kid = turn.kid.expect("the sub-agent ran");
    assert!(
        kid.messages_contain("read-only file search specialist"),
        "{}",
        kid.messages_text()
    );
    assert!(!kid.messages_contain("PROMPT-OF-b-explore"));
    let turn = delegate(&stub, &mut client, &narrowed, "N-HELPER", "b-helper").await?;
    assert!(turn.kid.is_none(), "a sub-agent ran for b's type");
    let (_, answer, is_error) = turn
        .events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "Agent")
        .cloned()
        .expect("the Agent call answered");
    assert!(
        is_error && answer.contains("`b-helper`") && answer.contains("explore"),
        "{answer}"
    );

    let whole = id(&create(&mut client, None).await?);
    let turn = delegate(&stub, &mut client, &whole, "W-EXPLORE", "explore").await?;
    let listed = tool_description(&turn.lead, "Agent");
    assert!(
        listed.contains("b's own explorer") && listed.contains("b-helper"),
        "{listed}"
    );
    let kid = turn.kid.expect("the sub-agent ran");
    assert!(
        kid.messages_contain("PROMPT-OF-b-explore"),
        "{}",
        kid.messages_text()
    );
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_left_out_packages_skill_is_not_listed_offered_or_run() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let a = package(&root, "a", "")?;
    let b = package(&root, "b", "[skills]\ndir = \"skills\"\n")?;
    write(
        &b,
        "skills/b-skill/SKILL.md",
        "---\nname: b-skill\ndescription: SKILL-OF-b listed\n---\nBODY-OF-b-SKILL\n",
    )?;
    let (embedded, mut client) = start(&root, vec![a, b]).await?;

    let run_skill = |tag: &str| {
        let mark = format!("SKILL-{tag}");
        stub.script_when(
            &mark,
            [
                Reply::calls(&format!("s-{tag}"), "Skill", json!({ "skill": "b-skill" })),
                Reply::text("skill done"),
            ],
        );
        mark
    };
    let skill_commands = |listed: &Value| -> Vec<String> {
        listed["commands"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["name"].as_str().map(str::to_string))
            .filter(|n| n == "b-skill")
            .collect()
    };

    let narrowed = id(&create(&mut client, Some(json!({"plugins": {"allow": ["a"]}}))).await?);
    let listed = client
        .call("commands.list", json!({ "session_id": narrowed }))
        .await?
        .result
        .unwrap_or_default();
    assert!(skill_commands(&listed).is_empty(), "{listed}");
    let mark = run_skill("NARROWED");
    let events = client
        .session_run_turn(&narrowed, &format!("{mark} go"), "n", None)
        .await?;
    assert!(events.turn_complete, "{events:?}");
    let first = stub
        .calls()
        .into_iter()
        .find(|c| c.messages_contain(&mark))
        .expect("the turn's request");
    assert!(!whole_text(&first).contains("SKILL-OF-b"));
    let (_, ran, _) = events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "Skill")
        .cloned()
        .expect("the Skill call answered");
    assert!(!ran.contains("BODY-OF-b-SKILL"), "{ran}");

    let whole = id(&create(&mut client, None).await?);
    let listed = client
        .call("commands.list", json!({ "session_id": whole }))
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(skill_commands(&listed), ["b-skill"], "{listed}");
    let mark = run_skill("WHOLE");
    let events = client
        .session_run_turn(&whole, &format!("{mark} go"), "w", None)
        .await?;
    assert!(events.turn_complete, "{events:?}");
    let first = stub
        .calls()
        .into_iter()
        .find(|c| c.messages_contain(&mark))
        .expect("the turn's request");
    assert!(whole_text(&first).contains("SKILL-OF-b"));
    let after = stub
        .calls()
        .into_iter()
        .filter(|c| c.messages_contain(&mark))
        .nth(1)
        .expect("the request after the Skill call");
    assert!(
        after.messages_contain("BODY-OF-b-SKILL"),
        "{}",
        after.messages_text()
    );
    embedded.shutdown().await;
    Ok(())
}

/// b's MCP server reaches neither a session leaving b out nor its sub-agent
/// whose type names the server.
#[tokio::test(flavor = "multi_thread")]
async fn a_left_out_packages_mcp_server_is_in_no_request_of_the_session() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let a = package(&root, "a", "")?;
    let b = package(
        &root,
        "b",
        "[[mcp]]\nname = \"toy\"\nkind = \"native\"\nconfig = \"mcp/server.json\"\n",
    )?;
    write(
        &b,
        "mcp/server.json",
        &McpToy::stdio_config(&[])?.to_string(),
    )?;
    write(
        &root.project,
        ".agix/agents/granted.md",
        "---\ndescription: uses b's server\nmcp_servers: [b-mcp-toy]\n---\nYou ping.\n",
    )?;
    let (embedded, mut client) = start(&root, vec![a, b]).await?;
    let offers_toy = |request: &SeenRequest| {
        request
            .tool_names()
            .iter()
            .any(|n| n.starts_with("mcp__") && n.contains("toy"))
    };
    let resource_tools = |request: &SeenRequest| {
        request
            .tool_names()
            .into_iter()
            .filter(|n| n == "ListMcpResources" || n == "ReadMcpResource")
            .count()
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let status = client.call("mcp.status", json!({})).await?.result;
        let status = status.unwrap_or_default();
        if status["servers"][0]["state"] == "connected" {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "b's server did not connect: {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let whole = id(&create(&mut client, None).await?);
    let turn = delegate(&stub, &mut client, &whole, "W-MCP", "granted").await?;
    assert!(offers_toy(&turn.lead), "{:?}", turn.lead.tool_names());
    assert_eq!(
        resource_tools(&turn.lead),
        2,
        "{:?}",
        turn.lead.tool_names()
    );
    let kid = turn.kid.expect("the sub-agent ran");
    assert!(offers_toy(&kid), "{:?}", kid.tool_names());

    let narrowed = id(&create(&mut client, Some(json!({"plugins": {"allow": ["a"]}}))).await?);
    let turn = delegate(&stub, &mut client, &narrowed, "N-MCP", "granted").await?;
    assert!(!offers_toy(&turn.lead), "{:?}", turn.lead.tool_names());
    assert_eq!(
        resource_tools(&turn.lead),
        0,
        "{:?}",
        turn.lead.tool_names()
    );
    let kid = turn.kid.expect("the sub-agent ran");
    assert!(!offers_toy(&kid), "{:?}", kid.tool_names());
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_left_out_packages_function_is_not_listed_or_called_for_the_session() -> anyhow::Result<()>
{
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let a = package(&root, "a", "")?;
    let b = package(
        &root,
        "b",
        "[[function]]\nname = \"check\"\nentry = \"f.js:check\"\ndescription = \"b check\"\n",
    )?;
    write(&b, "f.js", "async function check() { return 'b-ran'; }\n")?;
    let (embedded, mut client) = start(&root, vec![a, b]).await?;
    let names = |resp: daemon::rpc::RpcResponse| -> Vec<String> {
        resp.result.unwrap_or_default()["functions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f["name"].as_str().map(str::to_string))
            .collect()
    };
    let check = "b.check".to_string();

    let narrowed = id(&create(&mut client, Some(json!({"plugins": {"allow": ["a"]}}))).await?);
    let whole = id(&create(&mut client, None).await?);

    let resp = client
        .call("plugin.functions", json!({ "session_id": narrowed }))
        .await?;
    assert!(resp.error.is_none(), "{:?}", resp.error);
    assert!(!names(resp).contains(&check));
    let resp = client
        .call("plugin.functions", json!({ "session_id": whole }))
        .await?;
    assert!(names(resp).contains(&check));
    let resp = client
        .call("plugin.functions", json!({ "cwd": root.project }))
        .await?;
    assert!(names(resp).contains(&check));

    let refused = client
        .call(
            "plugin.call",
            json!({ "function": "b.check", "session_id": narrowed }),
        )
        .await?;
    let error = refused.error.expect("the call is refused");
    assert_eq!(error.code, -32602, "{}", error.message);
    assert!(
        error.message.contains("plugin `b`") && error.message.contains(&narrowed),
        "{}",
        error.message
    );
    let ran = client
        .call(
            "plugin.call",
            json!({ "function": "b.check", "session_id": whole }),
        )
        .await?;
    assert!(ran.error.is_none(), "{:?}", ran.error);
    let ran = ran.result.unwrap_or_default();
    assert_eq!(ran["ok"], true, "{ran}");
    assert_eq!(ran["value"], "b-ran", "{ran}");
    embedded.shutdown().await;
    Ok(())
}

/// b's scene, `plugin:b`, is refused to a session leaving b out: as the
/// scene it is created in, and as the scene an agent type sends its
/// sub-agent into.
#[tokio::test(flavor = "multi_thread")]
async fn a_left_out_packages_scene_is_entered_neither_at_create_nor_by_a_sub_agent(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let a = package(&root, "a", "")?;
    let b = package(&root, "b", "[scene]\ndir = \"scene\"\n")?;
    write(
        &b,
        "scene/scene.json",
        r#"{"id": "plugin:b", "name": "B", "tools": ["Read"]}"#,
    )?;
    write(&b, "scene/prompt.md", "SCENE-OF-b")?;
    write(
        &root.project,
        ".agix/agents/inb.md",
        "---\ndescription: works in b's scene\nscene: plugin:b\n---\nYou read.\n",
    )?;
    let (embedded, mut client) = start(&root, vec![a, b]).await?;

    let refused = client
        .call(
            "session.create",
            json!({
                "scene": "plugin:b",
                "options": {
                    "attended": false,
                    "auto_name": false,
                    "settings": {"plugins": {"allow": ["a"]}},
                },
            }),
        )
        .await?;
    let error = refused.error.expect("the scene is refused");
    assert_eq!(error.code, -32602, "{}", error.message);
    let data = error.data.clone().unwrap_or_default().to_string();
    assert!(data.contains("plugin:b"), "{} {data}", error.message);
    let entered = client
        .call(
            "session.create",
            json!({ "scene": "plugin:b", "options": { "attended": false, "auto_name": false } }),
        )
        .await?;
    assert!(entered.error.is_none(), "{:?}", entered.error);

    let narrowed = id(&create(&mut client, Some(json!({"plugins": {"allow": ["a"]}}))).await?);
    let turn = delegate(&stub, &mut client, &narrowed, "N-SCENE", "inb").await?;
    assert!(turn.kid.is_none(), "the sub-agent ran in plugin:b");
    let (_, answer, _) = turn
        .events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "Agent")
        .cloned()
        .expect("the Agent call answered");
    assert!(answer.contains("plugin:b"), "{answer}");

    let whole = id(&create(&mut client, None).await?);
    let turn = delegate(&stub, &mut client, &whole, "W-SCENE", "inb").await?;
    let kid = turn.kid.expect("the sub-agent ran");
    assert!(kid.system_contains("SCENE-OF-b"), "{}", kid.system_text());
    embedded.shutdown().await;
    Ok(())
}

/// The first request of a one-reply turn of `session`.
async fn one_turn(
    stub: &ProviderStub,
    client: &mut DaemonRpcClient,
    session: &str,
    tag: &str,
) -> anyhow::Result<SeenRequest> {
    let mark = format!("ONE-{tag}");
    stub.script_when(&mark, [Reply::text("done")]);
    let events = client
        .session_run_turn(session, &format!("{mark} go"), tag, None)
        .await?;
    anyhow::ensure!(events.turn_complete, "{events:?}");
    stub.calls()
        .into_iter()
        .find(|c| c.messages_contain(&mark))
        .ok_or_else(|| anyhow::anyhow!("no request of the turn {tag}"))
}

/// b, installed rather than the host's, has a server the pair connected
/// for a first session. Once b is disabled its server reaches no session
/// created after: one narrowed to a, nor one narrowing nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_packages_mcp_server_reaches_no_later_session() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let a = package(&root, "a", "")?;
    let staged = package(
        &root,
        "b",
        "[[mcp]]\nname = \"toy\"\nkind = \"native\"\nconfig = \"mcp/server.json\"\n",
    )?;
    write(
        &staged,
        "mcp/server.json",
        &McpToy::stdio_config(&[])?.to_string(),
    )?;
    let installed = root.data.join("plugins/cache/b/1.0.0");
    std::fs::create_dir_all(installed.parent().unwrap_or(&root.data))?;
    std::fs::rename(&staged, &installed)?;
    write(&installed, ".agix-installed", "")?;
    let (embedded, mut client) = start(&root, vec![a]).await?;
    let offers_toy = |request: &SeenRequest| {
        request
            .tool_names()
            .iter()
            .any(|n| n.starts_with("mcp__") && n.contains("toy"))
    };

    let first = id(&create(&mut client, None).await?);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let status = client.call("mcp.status", json!({})).await?.result;
        let status = status.unwrap_or_default();
        if status["servers"][0]["state"] == "connected" {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "b's server did not connect: {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let seen = one_turn(&stub, &mut client, &first, "FIRST").await?;
    assert!(offers_toy(&seen), "{:?}", seen.tool_names());

    let disabled = client.call("plugin.disable", json!({"name": "b"})).await?;
    assert!(disabled.error.is_none(), "{:?}", disabled.error);

    let narrowed = id(&create(&mut client, Some(json!({"plugins": {"allow": ["a"]}}))).await?);
    let seen = one_turn(&stub, &mut client, &narrowed, "NARROWED").await?;
    assert!(!offers_toy(&seen), "{:?}", seen.tool_names());
    let whole = id(&create(&mut client, None).await?);
    let seen = one_turn(&stub, &mut client, &whole, "WHOLE").await?;
    assert!(!offers_toy(&seen), "{:?}", seen.tool_names());
    embedded.shutdown().await;
    Ok(())
}

/// A function name nobody has is answered with the names there are: for a
/// session leaving b out, not b's.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_function_names_only_what_the_session_draws_on() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let functions = |name: &str| {
        format!(
            "[[function]]\nname = \"check\"\nentry = \"f.js:check\"\n\
             description = \"{name} check\"\n"
        )
    };
    let a = package(&root, "a", &functions("a"))?;
    write(&a, "f.js", "async function check() { return 'a-ran'; }\n")?;
    let b = package(&root, "b", &functions("b"))?;
    write(&b, "f.js", "async function check() { return 'b-ran'; }\n")?;
    let (embedded, mut client) = start(&root, vec![a, b]).await?;
    let narrowed = id(&create(&mut client, Some(json!({"plugins": {"allow": ["a"]}}))).await?);
    let whole = id(&create(&mut client, None).await?);
    let unknown = |session: &str| json!({ "function": "c.none", "session_id": session });

    let error = client
        .call("plugin.call", unknown(&narrowed))
        .await?
        .error
        .expect("no such function");
    assert!(error.message.contains("a.check"), "{}", error.message);
    assert!(!error.message.contains("b.check"), "{}", error.message);
    let error = client
        .call("plugin.call", unknown(&whole))
        .await?
        .error
        .expect("no such function");
    assert!(error.message.contains("b.check"), "{}", error.message);
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_says_a_session_can_narrow_its_plugins() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let (embedded, mut client) = start(&root, Vec::new()).await?;
    let ping = client.call("daemon.ping", json!({})).await?;
    let ping = ping.result.unwrap_or_default();
    assert!(
        ping["capabilities"]
            .as_array()
            .is_some_and(|c| c.iter().any(|c| c == "session_plugins")),
        "{ping}"
    );
    embedded.shutdown().await;
    Ok(())
}

fn settings_server(root: &Root, key: &str) -> anyhow::Result<()> {
    let path = root.data.join("settings.json");
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    settings["mcp"] = json!({ "servers": { key: McpToy::stdio_config(&[])? } });
    std::fs::write(path, serde_json::to_vec(&settings)?)?;
    Ok(())
}

async fn shadow_status(settings_key: &str) -> anyhow::Result<Value> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, json!({}))?;
    let b = package(
        &root,
        "b",
        "[[mcp]]\nname = \"toy\"\nkind = \"native\"\nconfig = \"mcp/server.json\"\n",
    )?;
    write(
        &b,
        "mcp/server.json",
        &McpToy::stdio_config(&[])?.to_string(),
    )?;
    settings_server(&root, settings_key)?;
    let (embedded, mut client) = start(&root, vec![b]).await?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        let status = client.call("mcp.status", json!({})).await?.result;
        let status = status.unwrap_or_default();
        let servers = status["servers"].as_array().cloned().unwrap_or_default();
        if servers.iter().any(|s| s["state"] == "connected") {
            break status;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "nothing connected: {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    embedded.shutdown().await;
    Ok(status)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_package_server_with_a_settings_key_is_warned_about() -> anyhow::Result<()> {
    let status = shadow_status("b-mcp-toy").await?;
    let named: Vec<&Value> = status["servers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["name"] == "b-mcp-toy")
        .collect();
    assert_eq!(named.len(), 1, "{status}");
    assert_eq!(named[0]["state"], "connected", "{status}");
    let warnings = status["warnings"].as_array().expect("warnings");
    assert_eq!(warnings.len(), 1, "{status}");
    let warning = warnings[0].as_str().unwrap();
    assert!(warning.contains("`b`") && warning.contains("`b-mcp-toy`"));
    assert!(warning.contains("replaces the settings one"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn differing_package_and_settings_keys_give_no_warning() -> anyhow::Result<()> {
    let status = shadow_status("elsewhere").await?;
    assert!(status["warnings"].is_null(), "{status}");
    Ok(())
}
