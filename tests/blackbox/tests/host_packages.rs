//! The host's own packages (`EmbedConfig.plugins`) as a session meets them,
//! over `Embedded::serve_stream`: a package directory inside the project is
//! written by no tool, in any permission mode, and by no sandboxed command
//! that reaches it without naming it or moves a directory above it aside.

use std::path::{Path, PathBuf};

use blackbox::{ProviderStub, Reply};
use daemon::embed::{EmbedConfig, Embedded};
use rpc_client::DaemonRpcClient;
use serde_json::json;

const MANIFEST: &str = "[plugin]\nname = \"xflow\"\nversion = \"2.0.0\"\napi_version = \"0.1\"\n\
    [[function]]\nname = \"check\"\nentry = \"x.js:check\"\ndescription = \"gate check\"\n";
const ENTRY: &str = "function check() { return 'host'; }\n";
const MARK: &str = "HOST-PACKAGE-MARK";

struct Root {
    _dir: tempfile::TempDir,
    data: PathBuf,
    project: PathBuf,
}

fn root(stub: &ProviderStub, mode: &str) -> anyhow::Result<Root> {
    let dir = tempfile::tempdir()?;
    let base = dir.path().canonicalize()?;
    let (data, project) = (base.join("data"), base.join("project"));
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(&project)?;
    let settings = json!({
        "providers": { "stub": {
            "api_type": "anthropic",
            "base_url": stub.base_url().as_str(),
            "api_key": "stub-key",
            "default_model": "claude-sonnet-4-6",
            "models": ["claude-sonnet-4-6"]
        }},
        "default_provider": "stub",
        "permission_mode": mode,
        "memory_enabled": false
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

fn host_package(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("plugin.toml"), MANIFEST)?;
    std::fs::write(dir.join("x.js"), ENTRY)?;
    Ok(())
}

async fn start(root: &Root, packages: Vec<PathBuf>) -> anyhow::Result<(Embedded, DaemonRpcClient)> {
    let mut config = EmbedConfig::new(&root.data);
    config.cwd = Some(root.project.clone());
    config.plugins = packages;
    let embedded = Embedded::start(config).await?;
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    embedded.serve_stream(theirs);
    let client =
        DaemonRpcClient::with_transport(Box::new(rpc_client::transport::stream(ours, "embed")));
    Ok((embedded, client))
}

#[tokio::test(flavor = "multi_thread")]
async fn no_tool_and_no_sandboxed_command_writes_a_host_package_in_the_project(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub, "acceptEdits")?;
    let package = root.project.join("tools/xflow");
    host_package(&package)?;
    let (embedded, mut client) = start(&root, vec![package.clone()]).await?;

    let entry = package.join("x.js");
    let elsewhere = root.project.join("notes.txt");
    stub.script_when(
        MARK,
        [
            Reply::calls(
                "r-host",
                "Read",
                json!({ "file_path": entry.to_string_lossy() }),
            ),
            Reply::calls(
                "w-host",
                "Write",
                json!({ "file_path": entry.to_string_lossy(), "content": "HACKED" }),
            ),
            Reply::calls(
                "w-project",
                "Write",
                json!({ "file_path": elsewhere.to_string_lossy(), "content": "fine" }),
            ),
            Reply::calls(
                "b-host",
                "Bash",
                json!({ "command": "cd tools && cd xflow && printf HACKED > x.js; echo rc=$?" }),
            ),
            Reply::calls(
                "b-aside",
                "Bash",
                json!({ "command": "mv tools moved; printf HACKED > moved/xflow/x.js; echo rc=$?" }),
            ),
            Reply::text("HOST-PACKAGE-DONE"),
        ],
    );
    stub.script_when("标题生成器", (0..4).map(|_| Reply::text("a title")));
    let session = client
        .session_create(json!({ "options": { "attended": false } }))
        .await?;
    let events = client
        .session_run_turn(&session, &format!("{MARK} go"), "t1", None)
        .await?;
    assert!(events.turn_complete, "{events:?}");

    let (_, written, failed) = events.tool_results[1].clone();
    assert!(
        failed,
        "the Write into the host package went through: {written}"
    );
    assert!(written.contains("package"), "{written}");
    assert_eq!(std::fs::read_to_string(&elsewhere)?, "fine");
    let (_, ran, _) = events.tool_results[3].clone();
    assert!(ran.contains("rc="), "the command did not run: {ran}");
    assert_eq!(std::fs::read_to_string(&entry)?, ENTRY, "{ran}");
    let (_, aside, _) = events.tool_results[4].clone();
    assert!(aside.contains("rc="), "the command did not run: {aside}");
    assert!(!root.project.join("moved").exists(), "{aside}");
    assert_eq!(std::fs::read_to_string(&entry)?, ENTRY, "{aside}");
    embedded.shutdown().await;
    Ok(())
}
