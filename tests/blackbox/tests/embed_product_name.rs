//! An embedder that names its own product (`EmbedConfig.product_name` =
//! `Codix`), over `Embedded::serve_stream`: the model is told it is Codix
//! and is never shown `Agix`, every request to the model and to an MCP
//! authorization server goes out as `codix/<version>`, the client the
//! authorization server registers is called Codix, and a sub-agent's
//! worktree is on a `codix/worktree-` branch, and `scene.list` calls the
//! built-in coding scene "Codix Code". Without the field nothing of that
//! changes from Agix. A name that cannot be a product's is refused at start.

use std::path::{Path, PathBuf};

use blackbox::oauth::McpServer;
use blackbox::{ProviderStub, Reply};
use daemon::embed::{EmbedConfig, EmbedError, Embedded};
use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};

const MAIN: &str = "PRODUCT-MAIN-MARK";
const SUB: &str = "PRODUCT-SUB-MARK";

struct Root {
    _dir: tempfile::TempDir,
    data: PathBuf,
    project: PathBuf,
}

fn git(project: &Path, args: &[&str]) -> anyhow::Result<String> {
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
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A trusted git project, the stub provider, and an MCP server that only
/// answers to a token its authorization server issued.
fn root(stub: &ProviderStub, mcp: &McpServer) -> anyhow::Result<Root> {
    let dir = tempfile::tempdir()?;
    let base = dir.path().canonicalize()?;
    let (data, project) = (base.join("data"), base.join("project"));
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(&project)?;
    std::fs::write(project.join("README.md"), "# project\n")?;
    git(&project, &["init", "-q", "-b", "main"])?;
    git(&project, &["add", "README.md"])?;
    git(
        &project,
        &[
            "-c",
            "user.name=tester",
            "-c",
            "user.email=tester@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "initial",
        ],
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
        "memory_enabled": false,
        "mcp_servers": { "docs": { "type": "streamable_http", "url": mcp.url } },
        "permission_rules": [
            { "tool": "Agent", "action": "allow" },
            { "tool": "Write", "action": "allow" }
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

fn connect(embedded: &Embedded) -> DaemonRpcClient {
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    embedded.serve_stream(theirs);
    DaemonRpcClient::with_transport(Box::new(rpc_client::transport::stream(ours, "embed")))
}

async fn call(client: &mut DaemonRpcClient, method: &str, params: Value) -> anyhow::Result<Value> {
    let response = client.call(method, params).await?;
    anyhow::ensure!(response.error.is_none(), "{method}: {response:?}");
    Ok(response.result.unwrap_or_default())
}

/// What one embedded run under `product` (`None`: the field left alone)
/// showed the outside world.
struct Seen {
    branches: String,
    stub: Vec<blackbox::SeenRequest>,
    as_user_agents: Vec<String>,
    client_names: Vec<String>,
    scene_names: Vec<(String, String)>,
}

async fn run(product: Option<&str>, project_dir: &str) -> anyhow::Result<Seen> {
    let stub = ProviderStub::start().await?;
    let (mcp, auth) = McpServer::start().await;
    let root = root(&stub, &mcp)?;
    let mut config = EmbedConfig::new(&root.data);
    config.cwd = Some(root.project.clone());
    config.project_dir = project_dir.into();
    if let Some(name) = product {
        config.product_name = name.into();
    }
    let embedded = Embedded::start(config).await?;
    let mut client = connect(&embedded);
    let scenes = call(&mut client, "scene.list", json!({})).await?;
    let mut scene_names: Vec<(String, String)> = scenes["scenes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            let text = |k: &str| s[k].as_str().unwrap_or_default().to_string();
            (text("scene"), text("name"))
        })
        .collect();
    scene_names.sort();

    let mut state = Value::Null;
    for _ in 0..600 {
        state = call(&mut client, "mcp.status", json!({})).await?["servers"][0]["state"].clone();
        if state == "auth_required" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::ensure!(
        state == "auth_required",
        "docs never asked for sign-in: {state}"
    );
    let started = call(&mut client, "mcp.auth.start", json!({ "server": "docs" })).await?;
    anyhow::ensure!(started["authorize_url"].is_string(), "{started}");

    stub.script_when("标题生成器", (0..4).map(|_| Reply::text("a title")));
    stub.script_when(
        MAIN,
        [
            Reply::calls("t-guide", "ToolSearch", json!({ "query": "select:Bash" })),
            Reply::calls(
                "t-agent",
                "Agent",
                json!({ "prompt": format!("{SUB} work"), "description": "worker", "worktree": "probe" }),
            ),
            Reply::text("MAIN-DONE"),
        ],
    );
    let made = root.project.join(".wkts/probe/made.txt");
    stub.script_when(
        SUB,
        [
            Reply::calls(
                "s-made",
                "Write",
                json!({ "file_path": made.to_string_lossy(), "content": "made" }),
            ),
            Reply::text("SUB-DONE"),
        ],
    );
    let session = client.session_create(json!({})).await?;
    let events = client
        .session_run_turn(&session, &format!("{MAIN} go"), "t1", None)
        .await?;
    anyhow::ensure!(events.turn_complete, "{events:?}");
    let branches = git(&root.project, &["branch", "--list"])?;
    embedded.shutdown().await;

    let (as_user_agents, client_names) = {
        let state = auth.state.lock().unwrap();
        (state.user_agents.clone(), state.client_names.clone())
    };
    Ok(Seen {
        branches,
        stub: stub.seen(),
        as_user_agents,
        client_names,
        scene_names,
    })
}

fn the_turn(seen: &Seen) -> &blackbox::SeenRequest {
    seen.stub
        .iter()
        .find(|c| c.has_tools() && c.messages_contain(MAIN))
        .expect("the main turn reached the model")
}

fn after_the_guide(seen: &Seen) -> &blackbox::SeenRequest {
    seen.stub
        .iter()
        .find(|c| c.messages_contain("Generated with"))
        .expect("the Bash guide came back to the model")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_named_product_is_what_the_model_the_servers_and_git_see() -> anyhow::Result<()> {
    let seen = run(Some("Codix"), ".codix").await?;
    let user_agent = format!("codix/{}", env!("CARGO_PKG_VERSION"));

    assert!(the_turn(&seen).system_contains("You are Codix"));
    assert!(after_the_guide(&seen).messages_contain("Generated with Codix"));
    for request in &seen.stub {
        assert_eq!(request.header("user-agent"), Some(user_agent.as_str()));
        let body = request.body.to_string().to_lowercase();
        assert!(
            !body.contains("agix"),
            "a request to the model names Agix: {}",
            &body[body.find("agix").unwrap().saturating_sub(120)..][..240.min(body.len())]
        );
    }
    assert_eq!(seen.client_names, ["Codix"]);
    assert!(!seen.as_user_agents.is_empty());
    assert!(
        seen.as_user_agents.iter().all(|ua| *ua == user_agent),
        "{:?}",
        seen.as_user_agents
    );
    assert!(
        seen.branches.contains("codix/worktree-probe"),
        "{}",
        seen.branches
    );
    assert!(!seen.branches.contains("agix"), "{}", seen.branches);
    assert!(
        seen.scene_names
            .contains(&("code".to_string(), "Codix Code".to_string())),
        "{:?}",
        seen.scene_names
    );
    assert!(
        seen.scene_names
            .iter()
            .all(|(_, name)| !name.contains("Agix")),
        "{:?}",
        seen.scene_names
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_product_name_the_engine_is_agix() -> anyhow::Result<()> {
    let seen = run(None, ".agix").await?;
    let user_agent = format!("agix/{}", env!("CARGO_PKG_VERSION"));

    assert!(the_turn(&seen).system_contains("You are Agix"));
    assert!(after_the_guide(&seen)
        .messages_contain("Generated with [Agix](https://github.com/openatta/agix)"));
    for request in &seen.stub {
        assert_eq!(request.header("user-agent"), Some(user_agent.as_str()));
    }
    assert_eq!(seen.client_names, ["Agix"]);
    assert!(seen.as_user_agents.iter().all(|ua| *ua == user_agent));
    assert!(
        seen.branches.contains("agix/worktree-probe"),
        "{}",
        seen.branches
    );
    assert!(
        seen.scene_names
            .contains(&("code".to_string(), "Agix Code".to_string())),
        "{:?}",
        seen.scene_names
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_that_cannot_be_a_product_is_refused_at_start() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    for name in ["", "1x", "-x", "a b", "a/b", "a.b", "Ägix"] {
        let mut config = EmbedConfig::new(dir.path());
        config.cwd = Some(dir.path().to_path_buf());
        config.product_name = name.into();
        match Embedded::start(config).await {
            Err(EmbedError::InvalidProductName { name: refused, .. }) => assert_eq!(refused, name),
            Err(other) => panic!("{name:?}: {other}"),
            Ok(_) => panic!("{name:?} was taken"),
        }
    }
    Ok(())
}
