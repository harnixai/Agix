//! An embedder's JavaScript library (`EmbedConfig.js_libs`) in plugin code,
//! over `Embedded::serve_stream`: a package that `uses` it installs, and its
//! command typed as `/name` calls the host's function, gets its answer, and
//! the host sees who called (`via:"user"`, the package — an installed one,
//! whatever it calls itself — the session). A
//! package that does not declare it, and a `RunCode` program, have no such
//! global. The same package is refused by an embedder without the library,
//! and a library that cannot be installed fails the start.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use blackbox::{ProviderStub, Reply};
use daemon::embed::{EmbedConfig, EmbedError, Embedded};
use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};
use test_runner::plugin_fixture::{file_url, zip_with_entries, Entry};

use base::interface::host_js::{HostCallContext, HostJsLib, PackageSource};

const USES: &str = "uses = [\"codix\"]\n[plugin]\nname = \"xflow\"\nversion = \"1.0.0\"\n\
    api_version = \"0.1\"\n[[command]]\nname = \"xrun\"\nentry = \"x.js:main\"\n\
    description = \"start a flow\"\nread_only = [\"\"]\n";

const USES_JS: &str = "async function main(argv, input, ctx) { \
    const started = await codix.flow.begin({ change: 'c1' }); \
    return { started, twice: codix.flow.twice !== undefined }; }\n";

const PLAIN: &str = "[plugin]\nname = \"plain\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\
    [[command]]\nname = \"probe\"\nentry = \"p.js:main\"\ndescription = \"probe\"\n\
    read_only = [\"\"]\n";

const PLAIN_JS: &str = "function main() { return { codix: typeof codix }; }\n";

const MAIN: &str = "HOST-LIB-MAIN";

/// What the host's function was called with, from where.
type Seen = Arc<Mutex<Vec<(HostCallContext, Value)>>>;

fn codix(seen: Seen) -> HostJsLib {
    HostJsLib::new("codix")
        .function("flow.begin", move |cx: HostCallContext, input: Value| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push((cx, input.clone()));
                Ok(json!({ "run": "r-1", "input": input }))
            }
        })
        .prelude("codix.flow.twice = async (x) => [await codix.flow.begin(x), await codix.flow.begin(x)];")
        .description("Codix flows")
}

struct Root {
    _dir: tempfile::TempDir,
    data: PathBuf,
    project: PathBuf,
}

fn root(stub: &ProviderStub) -> anyhow::Result<Root> {
    let dir = tempfile::tempdir()?;
    let base = dir.path().canonicalize()?;
    let (data, project) = (base.join("data"), base.join("project"));
    std::fs::create_dir_all(project.join(".agix"))?;
    std::fs::create_dir_all(&data)?;
    std::fs::write(
        project.join(".agix").join("settings.json"),
        serde_json::to_vec(&json!({
            "permissions": {"allow": ["RunCode"]}
        }))?,
    )?;
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

async fn start(
    root: &Root,
    js_libs: Vec<HostJsLib>,
) -> anyhow::Result<(Embedded, DaemonRpcClient)> {
    let mut config = EmbedConfig::new(&root.data);
    config.credentials = Some(blackbox::stub_credentials());
    config.cwd = Some(root.project.clone());
    config.js_libs = js_libs;
    let embedded = Embedded::start(config).await?;
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    embedded.serve_stream(theirs);
    let client =
        DaemonRpcClient::with_transport(Box::new(rpc_client::transport::stream(ours, "embed")));
    Ok((embedded, client))
}

async fn install(
    client: &mut DaemonRpcClient,
    root: &Root,
    name: &str,
    files: [(&str, &str); 2],
) -> anyhow::Result<Value> {
    let entries: Vec<Entry<'_>> = files
        .iter()
        .map(|(path, text)| Entry::File(path, text.as_bytes().to_vec()))
        .collect();
    let (archive, _) = zip_with_entries(&root.data.join("archives"), name, &entries)?;
    let answer = client
        .plugin_install_and_enable(
            json!({ "name": name, "version": "1.0.0", "download_url": file_url(&archive) }),
        )
        .await?;
    Ok(serde_json::to_value(&answer)?)
}

/// The JSON a command's or program's result starts with.
fn leading_json(text: &str) -> Value {
    serde_json::Deserializer::from_str(text)
        .into_iter()
        .next()
        .and_then(Result::ok)
        .unwrap_or_else(|| panic!("the result does not start with JSON: {text}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_uses_the_hosts_library_calls_it_and_others_do_not_have_it(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub)?;
    let seen = Seen::default();
    let (embedded, mut client) = start(&root, vec![codix(seen.clone())]).await?;

    let installed = install(
        &mut client,
        &root,
        "xflow",
        [("plugin.toml", USES), ("x.js", USES_JS)],
    )
    .await?;
    assert_eq!(installed["result"]["success"], true, "{installed}");
    let capabilities = installed["result"]["disclosure"]["capabilities"].to_string();
    assert!(
        capabilities.contains("uses host library `codix` (Codix flows)"),
        "{capabilities}"
    );
    let plain = install(
        &mut client,
        &root,
        "plain",
        [("plugin.toml", PLAIN), ("p.js", PLAIN_JS)],
    )
    .await?;
    assert_eq!(plain["result"]["success"], true, "{plain}");

    stub.script_when("标题生成器", (0..8).map(|_| Reply::text("a title")));
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
                json!({ "description": "probe", "code": "return { codix: typeof codix };" }),
            ),
            Reply::text("MAIN-DONE"),
        ],
    );
    let session = client.session_create(json!({})).await?;
    let typed = client
        .session_run_turn(&session, "/xrun", "t1", None)
        .await?;
    anyhow::ensure!(typed.turn_complete, "{typed:?}");
    assert_eq!(
        leading_json(&typed.text),
        json!({ "started": { "run": "r-1", "input": { "change": "c1" } }, "twice": true }),
        "{}",
        typed.text
    );
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        let (cx, input) = &seen[0];
        assert_eq!(input, &json!({ "change": "c1" }));
        assert_eq!(cx.via, "user");
        assert_eq!(cx.plugin, "xflow");
        assert_eq!(
            cx.source,
            PackageSource::Installed,
            "a package anyone may install"
        );
        assert_eq!(cx.plugin_version, "1.0.0");
        assert_eq!(cx.entry, "xrun");
        assert_eq!(cx.session_id.as_deref(), Some(session.as_str()));
        assert_eq!(cx.cwd, root.project);
        assert_eq!(cx.agent_depth, 0);
    }

    let probed = client
        .session_run_turn(&session, "/probe", "t2", None)
        .await?;
    assert_eq!(
        leading_json(&probed.text),
        json!({ "codix": "undefined" }),
        "{}",
        probed.text
    );

    let modelled = client
        .session_run_turn(&session, &format!("{MAIN} go"), "t3", None)
        .await?;
    anyhow::ensure!(modelled.turn_complete, "{modelled:?}");
    let (_, program, failed) = modelled
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "RunCode")
        .expect("the program ran");
    assert!(!failed, "{program}");
    assert_eq!(leading_json(program), json!({ "codix": "undefined" }));
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "nothing else called the host"
    );
    embedded.shutdown().await;

    let (embedded, mut client) = start(&root, Vec::new()).await?;
    let refused = install(
        &mut client,
        &root,
        "xflow",
        [("plugin.toml", USES), ("x.js", USES_JS)],
    )
    .await?;
    assert!(
        refused["error"]["message"].as_str().is_some_and(
            |m| m.contains("needs host library `codix`, which this host does not provide")
        ),
        "{refused}"
    );
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_library_that_cannot_be_installed_fails_the_start() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let ok = || HostJsLib::new("codix").function("flow.begin", |_, v| async move { Ok(v) });
    let cases: Vec<(&str, &str, Vec<HostJsLib>)> = vec![
        ("agix", "agix", vec![HostJsLib::new("agix")]),
        ("foo", "foo", vec![HostJsLib::new("foo")]),
        ("agix", "tools", vec![HostJsLib::new("tools")]),
        ("agix", "Codix", vec![HostJsLib::new("Codix")]),
        ("agix", "codix", vec![ok(), ok()]),
        (
            "agix",
            "codix",
            vec![ok().function("flow..start", |_, v| async move { Ok(v) })],
        ),
        (
            "agix",
            "codix",
            vec![ok().function("flow.begin", |_, v| async move { Ok(v) })],
        ),
    ];
    for (engine, namespace, js_libs) in cases {
        let mut config = EmbedConfig::new(dir.path());
        config.credentials = Some(blackbox::stub_credentials());
        config.cwd = Some(dir.path().to_path_buf());
        config.engine = engine.into();
        config.js_libs = js_libs;
        match Embedded::start(config).await {
            Err(EmbedError::InvalidHostLibrary {
                namespace: refused,
                reason,
            }) => {
                assert_eq!(refused, namespace, "{reason}")
            }
            Err(other) => panic!("{engine}/{namespace}: {other}"),
            Ok(_) => panic!("{engine}/{namespace} was taken"),
        }
    }
    Ok(())
}
