//! An embedder that names its own engine (`EmbedConfig.engine` = `foo`), over
//! `Embedded::serve_stream`: a `RunCode` program finds the base library under
//! `foo` and not under `agix`, its errors are `FooError`, the model reads the
//! helpers as `foo.*` in the tool's description and guide, and the project's
//! settings are read from `.foo` and not from `.agix`. The product is still
//! Agix: the prompt and the `User-Agent` do not follow the engine's name.
//! `daemon.doctor` names the installed plugins whose JavaScript still calls
//! `agix.`, and says nothing of it under the default engine. A name that
//! cannot be an engine's is refused at start.

use std::path::PathBuf;

use blackbox::{ProviderStub, Reply};
use daemon::embed::{EmbedConfig, EmbedError, Embedded};
use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};
use test_runner::plugin_fixture::{
    file_url, package_ext, package_forge, EXT_PACKAGE, EXT_PACKAGE_VERSION, FORGE_PACKAGE,
    FORGE_PACKAGE_VERSION,
};

const MAIN: &str = "ENGINE-MAIN-MARK";

const PROGRAM: &str = "let e; try { foo.yaml.parse('a: [1'); } catch (x) { e = x; } \
     return [typeof agix, typeof foo, foo.yaml.parse('a: 1').a, e.name, \
     e instanceof Error, String(e).split(':')[0]];";

struct Root {
    _dir: tempfile::TempDir,
    data: PathBuf,
    project: PathBuf,
}

/// A trusted project whose `.foo` allows `RunCode` and whose `.agix` denies
/// it, and the stub provider in the data root.
fn root(stub: &ProviderStub) -> anyhow::Result<Root> {
    let dir = tempfile::tempdir()?;
    let base = dir.path().canonicalize()?;
    let (data, project) = (base.join("data"), base.join("project"));
    std::fs::create_dir_all(&data)?;
    for (dir, list) in [(".foo", "allow"), (".agix", "deny")] {
        std::fs::create_dir_all(project.join(dir))?;
        std::fs::write(
            project.join(dir).join("settings.json"),
            serde_json::to_vec(&json!({ "permissions": { list: ["RunCode"] } }))?,
        )?;
    }
    let settings = json!({
        "providers": [{
            "id": "stub",
            "api": "anthropic",
            "base_url": stub.base_url().as_str(),
            "tiers": {"strong": "claude-sonnet-4-6"},
            "models": ["claude-sonnet-4-6"]
        }],
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

fn description_of<'a>(request: &'a blackbox::SeenRequest, tool: &str) -> Option<&'a str> {
    request.body["tools"]
        .as_array()?
        .iter()
        .find(|t| t["name"] == tool)?["description"]
        .as_str()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_named_engine_is_the_global_the_errors_and_the_project_directory() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub)?;
    let mut config = EmbedConfig::new(&root.data);
    config.credentials = Some(blackbox::stub_credentials());
    config.cwd = Some(root.project.clone());
    config.engine = "foo".into();
    let embedded = Embedded::start(config).await?;
    let mut client = connect(&embedded);

    stub.script_when("标题生成器", (0..4).map(|_| Reply::text("a title")));
    stub.script_when(
        MAIN,
        [
            Reply::calls(
                "t-guide",
                "ToolSearch",
                json!({ "query": "select:RunCode" }),
            ),
            Reply::calls(
                "t-program",
                "RunCode",
                json!({ "description": "probe the library", "code": PROGRAM }),
            ),
            Reply::text("MAIN-DONE"),
        ],
    );
    let session = client.session_create(json!({})).await?;
    let events = client
        .session_run_turn(&session, &format!("{MAIN} go"), "t1", None)
        .await?;
    anyhow::ensure!(events.turn_complete, "{events:?}");
    embedded.shutdown().await;

    let (_, program, failed) = events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "RunCode")
        .expect("the program ran");
    assert!(!failed, "{program}");
    let result: Value = serde_json::Deserializer::from_str(program)
        .into_iter()
        .next()
        .and_then(Result::ok)
        .unwrap_or_else(|| panic!("the program's result does not start with JSON: {program}"));
    assert_eq!(
        result,
        json!(["undefined", "object", 1, "FooError", true, "FooError"])
    );

    let seen = stub.seen();
    let turn = seen
        .iter()
        .find(|c| c.has_tools() && c.messages_contain(MAIN))
        .expect("the main turn reached the model");
    let description = description_of(turn, "RunCode").expect("RunCode is offered");
    assert!(description.contains("foo.yaml"), "{description}");
    assert!(!description.contains("agix."), "{description}");
    let guided = seen
        .iter()
        .find(|c| c.messages_contain("foo.cli.parseArgs"))
        .expect("the RunCode guide came back to the model naming foo");
    assert!(!guided.messages_contain("agix.cli"));

    assert!(turn.system_contains("You are Agix"));
    let user_agent = format!("agix/{}", env!("CARGO_PKG_VERSION"));
    assert!(seen
        .iter()
        .all(|r| r.header("user-agent") == Some(user_agent.as_str())));
    Ok(())
}

async fn engine_name_check(engine: &str) -> anyhow::Result<Value> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub)?;
    let mut config = EmbedConfig::new(&root.data);
    config.credentials = Some(blackbox::stub_credentials());
    config.cwd = Some(root.project.clone());
    config.engine = engine.into();
    let embedded = Embedded::start(config).await?;
    let mut client = connect(&embedded);
    let packages = root.data.join("packages");
    for (archive, name) in [
        (package_forge(&packages)?.0, FORGE_PACKAGE),
        (package_ext(&packages)?.0, EXT_PACKAGE),
    ] {
        let version = if name == FORGE_PACKAGE {
            FORGE_PACKAGE_VERSION
        } else {
            EXT_PACKAGE_VERSION
        };
        let installed = client
            .plugin_install_and_enable(
                json!({ "name": name, "version": version, "download_url": file_url(&archive) }),
            )
            .await?;
        anyhow::ensure!(installed.error.is_none(), "{name}: {installed:?}");
    }
    let doctor = client.daemon_doctor().await?.result.unwrap_or_default();
    embedded.shutdown().await;
    Ok(doctor["health"]["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == "plugins.engine_name")
        .cloned()
        .unwrap_or_default())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_doctor_names_the_plugins_that_still_call_agix() -> anyhow::Result<()> {
    let check = engine_name_check("foo").await?;
    assert_eq!(check["status"], "degraded", "{check}");
    let plugins: Vec<&str> = check["details"]["plugins"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["plugin"].as_str())
        .collect();
    assert_eq!(plugins, [FORGE_PACKAGE], "{check}");
    let summary = check["summary"].as_str().unwrap_or_default();
    assert!(summary.contains("`foo.`"), "{check}");

    assert_eq!(engine_name_check("agix").await?, Value::Null);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_that_cannot_be_an_engine_is_refused_at_start() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    for name in [
        "",
        "Foo",
        "1x",
        "a-b",
        "class",
        "tools",
        "console",
        "escape",
        "claude",
        "abcdefghijklmnopq",
    ] {
        let mut config = EmbedConfig::new(dir.path());
        config.credentials = Some(blackbox::stub_credentials());
        config.cwd = Some(dir.path().to_path_buf());
        config.engine = name.into();
        match Embedded::start(config).await {
            Err(EmbedError::InvalidEngineName { name: refused, .. }) => assert_eq!(refused, name),
            Err(other) => panic!("{name:?}: {other}"),
            Ok(_) => panic!("{name:?} was taken"),
        }
    }
    Ok(())
}
