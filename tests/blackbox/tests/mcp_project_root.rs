//! Which project an MCP server learns it serves, against a scripted model. One
//! daemon serves several projects and starts a plugin's server in the
//! plugin's directory, so the server can only know the session's project from
//! the client: the `roots` it answers with and, for a stdio server,
//! `AGIX_PROJECT_ROOT`. The toy's `where` reports both and its working
//! directory.

use std::path::{Path, PathBuf};

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, McpToy, ProviderStub, Purpose, Reply};
use serde_json::{json, Value};

const PLUGIN_WHERE: &str = "mcp__kit-mcp-tools__where";

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

fn package_dir(world: &blackbox::World, plugin: &str) -> PathBuf {
    world
        .global_root()
        .join("plugins")
        .join("cache")
        .join(plugin)
        .join("1.0.0")
}

/// A package under the world's global plugin tier declaring one MCP server
/// (`<plugin>-mcp-<server>`).
fn install_plugin_server(
    world: &blackbox::World,
    plugin: &str,
    server: &str,
    config: &Value,
) -> anyhow::Result<()> {
    let dir = package_dir(world, plugin);
    std::fs::create_dir_all(dir.join("mcp"))?;
    std::fs::write(dir.join("mcp").join("server.json"), config.to_string())?;
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "[plugin]\nname = \"{plugin}\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\
             description = \"declares an MCP server\"\n\n\
             [[mcp]]\nname = \"{server}\"\nkind = \"native\"\nconfig = \"mcp/server.json\"\n"
        ),
    )?;
    Ok(())
}

fn with_plugin_toy(builder: LiveRunBuilder) -> anyhow::Result<LiveRunBuilder> {
    let toy = McpToy::stdio_config(&["--name", "kit"])?;
    Ok(builder.before_start(move |world, _| install_plugin_server(world, "kit", "tools", &toy)))
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn file_uri(path: &Path) -> String {
    url::Url::from_file_path(path)
        .unwrap_or_else(|()| panic!("{} is not absolute", path.display()))
        .to_string()
}

/// What `where` reported in this turn, parsed.
fn where_report(events: &rpc_client::TurnEvents, tool: &str) -> Value {
    let (_, text, failed) = events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == tool)
        .unwrap_or_else(|| panic!("no {tool} result in {:?}", events.tool_results));
    assert!(!failed, "{tool} failed: {text}");
    let body = blackbox::inside_fence(text);
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{tool} answered {body:?}: {e}"))
}

fn assert_serves(report: &Value, project: &Path, what: &str) {
    let project = canonical(project);
    assert_eq!(
        report["roots"],
        json!([file_uri(&project)]),
        "{what}: roots {report}"
    );
    assert_eq!(
        report["env"],
        json!(project.to_string_lossy()),
        "{what}: AGIX_PROJECT_ROOT {report}"
    );
}

/// A plugin's server runs in the plugin's directory and still learns the
/// session's project; so does a server declared in settings, at either
/// protocol generation (an older one asks with its own `roots/list`).
#[tokio::test(flavor = "multi_thread")]
async fn a_plugins_server_and_a_settings_server_learn_the_sessions_project() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let toy = McpToy::stdio_config(&["--name", "own"])?;
    let older = McpToy::stdio_config(&["--name", "older", "--max-protocol", "2025-11-25"])?;
    let mut run = with_plugin_toy(stubbed("mcp-root", &stub))?
        .settings(json!({ "mcp": { "servers": { "toy": toy, "older": older } } }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("w1", PLUGIN_WHERE, json!({})),
        Reply::calls("w2", "mcp__toy__where", json!({})),
        Reply::calls("w3", "mcp__older__where", json!({})),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "where are you").await?;

    let plugin = where_report(&events, PLUGIN_WHERE);
    assert_serves(&plugin, &run.project, "the plugin's server");
    assert_eq!(
        plugin["cwd"].as_str().map(|c| canonical(Path::new(c))),
        Some(canonical(&package_dir(&run.world, "kit"))),
        "the plugin's server still starts in its package: {plugin}"
    );
    assert_serves(
        &where_report(&events, "mcp__toy__where"),
        &run.project,
        "the settings server",
    );
    assert_serves(
        &where_report(&events, "mcp__older__where"),
        &run.project,
        "the older settings server",
    );
    run.finish().await;
    Ok(())
}

/// One daemon, two projects: each session's server reports its own.
#[tokio::test(flavor = "multi_thread")]
async fn two_sessions_on_two_projects_each_see_their_own() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_plugin_toy(stubbed("mcp-root-a", &stub))?
        .before_start(|world, _| world.trust_project(&world.project("mcp-root-b")?))
        .start()
        .await?;
    let other = run.world.project("mcp-root-b")?;

    let first = run.session().await?;
    let _first_host = run.watch(&first, true).await?;
    stub.script([
        Reply::calls("a1", PLUGIN_WHERE, json!({})),
        Reply::text("a"),
    ]);
    let in_first = run.turn(&first, "where, first").await?;

    let second = run
        .session_with(json!({ "project_root": other.to_string_lossy() }))
        .await?;
    let _second_host = run.watch(&second, true).await?;
    stub.script([
        Reply::calls("b1", PLUGIN_WHERE, json!({})),
        Reply::text("b"),
    ]);
    let in_second = run.turn(&second, "where, second").await?;

    assert_serves(
        &where_report(&in_first, PLUGIN_WHERE),
        &run.project,
        "first",
    );
    assert_serves(&where_report(&in_second, PLUGIN_WHERE), &other, "second");
    run.finish().await;
    Ok(())
}

/// A session with no project (in `harnix`, a scene that allows one) offers
/// the server no root and no variable — not the daemon's own directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_without_a_project_offers_no_root() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_plugin_toy(stubbed("mcp-root-none", &stub))?
        .before_start(|world, _| {
            world.write_global_settings(&json!({
                "permissions": {"allow": [PLUGIN_WHERE]}
            }))
        })
        .start()
        .await?;
    let session = run
        .scene_session("harnix", json!({ "project_root": null }))
        .await?;
    stub.script([
        Reply::calls("n1", PLUGIN_WHERE, json!({})),
        Reply::text("none"),
    ]);
    let events = run.turn(&session, "where, nowhere").await?;
    let report = where_report(&events, PLUGIN_WHERE);
    assert_eq!(report["roots"], json!([]), "{report}");
    assert_eq!(report["env"], Value::Null, "{report}");
    run.finish().await;
    Ok(())
}

/// A sub-agent granted the server calls it on the same connection, so it is
/// told the same project.
#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_granted_the_server_sees_the_same_root() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_plugin_toy(stubbed("mcp-root-child", &stub))?
        .settings(json!({
            "permissions": {"allow": ["Agent", PLUGIN_WHERE]}
        }))
        .before_start(|world, _| {
            world.write_project_file(
                "mcp-root-child",
                ".agix/agents/locator.md",
                "---\ndescription: asks the kit server where it is\nmcp_servers: [kit-mcp-tools]\n\
                 ---\nYou locate.\n",
            )?;
            Ok(())
        })
        .start()
        .await?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls(
                "p1",
                "Agent",
                json!({
                    "prompt": "CHILD-MARK ask where",
                    "description": "locate",
                    "subagent_type": "locator"
                }),
            ),
            Reply::text("parent done"),
        ],
    );
    stub.script_when(
        "CHILD-MARK",
        [
            Reply::calls("c1", PLUGIN_WHERE, json!({})),
            Reply::text("child done"),
        ],
    );
    run.turn(&session, "PARENT-MARK delegate").await?;

    let result = stub
        .calls()
        .into_iter()
        .filter(|c| c.purpose() == Purpose::Turn && c.messages_contain("CHILD-MARK"))
        .find_map(|c| {
            c.body["messages"]
                .as_array()?
                .iter()
                .filter_map(|m| m["content"].as_array())
                .flatten()
                .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == "c1")
                .cloned()
        })
        .expect("the child's where result reached its model");
    let text = match &result["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<String>(),
        other => panic!("tool result content {other}"),
    };
    let report: Value = serde_json::from_str(blackbox::inside_fence(&text))
        .unwrap_or_else(|e| panic!("the child's where answered {text:?}: {e}"));
    assert_serves(&report, &run.project, "the sub-agent");
    run.finish().await;
    Ok(())
}

/// The project opened through a symlink and by its own path is reported as
/// one directory, resolved, either way.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_project_opened_through_a_symlink_reports_its_resolved_root() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_plugin_toy(stubbed("mcp-root-link", &stub))?
        .start()
        .await?;
    let link = run.world.root().join("linked-project");
    std::os::unix::fs::symlink(&run.project, &link)?;

    let direct = run.session().await?;
    let _direct_host = run.watch(&direct, true).await?;
    stub.script([
        Reply::calls("d1", PLUGIN_WHERE, json!({})),
        Reply::text("d"),
    ]);
    let by_path = where_report(&run.turn(&direct, "where, direct").await?, PLUGIN_WHERE);

    let linked = run
        .session_with(json!({ "project_root": link.to_string_lossy() }))
        .await?;
    let _linked_host = run.watch(&linked, true).await?;
    stub.script([
        Reply::calls("l1", PLUGIN_WHERE, json!({})),
        Reply::text("l"),
    ]);
    let by_link = where_report(&run.turn(&linked, "where, linked").await?, PLUGIN_WHERE);

    assert_serves(&by_path, &run.project, "by its path");
    assert_serves(&by_link, &run.project, "through the link");
    run.finish().await;
    Ok(())
}

/// A remote server is not handed this machine's paths: over HTTP the client
/// offers no roots, at either protocol generation, while a stdio server in
/// the same session still learns the project.
#[tokio::test(flavor = "multi_thread")]
async fn a_remote_server_is_offered_no_roots() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let remote = McpToy::spawn_http(&["--name", "remote"]).await?;
    let older = McpToy::spawn_http(&["--name", "older", "--max-protocol", "2025-11-25"]).await?;
    let local = McpToy::stdio_config(&["--name", "local"])?;
    let mut run = stubbed("mcp-root-remote", &stub)
        .settings(json!({ "mcp": { "servers": {
            "remote": remote.config("streamable_http"),
            "older": older.config("streamable_http"),
            "local": local,
        } } }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script([
        Reply::calls("r1", "mcp__remote__where", json!({})),
        Reply::calls("r2", "mcp__older__where", json!({})),
        Reply::calls("l1", "mcp__local__where", json!({})),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "where are you").await?;
    for tool in ["mcp__remote__where", "mcp__older__where"] {
        let report = where_report(&events, tool);
        assert_eq!(report["roots"], Value::Null, "{tool}: {report}");
        assert!(
            !report
                .to_string()
                .contains(&*canonical(&run.project).to_string_lossy()),
            "{tool} learned the project: {report}"
        );
    }
    assert_serves(
        &where_report(&events, "mcp__local__where"),
        &run.project,
        "the stdio server",
    );
    run.finish().await;
    remote.stop().await;
    older.stop().await;
    Ok(())
}
