//! The package side of plugin functions (`[[function]]`), host libraries
//! (`uses`) and the host's own packages, through the embed entry's protocol
//! stream: what `plugin.install` discloses and refuses, what `plugin.list`
//! shows, and what `plugin.uninstall` / `plugin.disable` will not touch — an
//! installed package the host's shadows is still uninstalled.

use std::path::{Path, PathBuf};
use std::time::Duration;

use blackbox::ProviderStub;
use daemon::embed::{EmbedConfig, Embedded};
use serde_json::{json, Value};
use test_runner::plugin_fixture::{file_url, zip_with_entries, Entry};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const XFLOW: &str =
    "uses = []\n[plugin]\nname = \"xflow\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\
    [[function]]\nname = \"check\"\nentry = \"xflow.js:check\"\ndescription = \"gate check\"\n\
    writes = [\"reports/**\"]\ninput_schema = \"schemas/check.json\"\n\
    [[function]]\nname = \"peek\"\nentry = \"xflow.js:peek\"\ndescription = \"look only\"\n";

const XFLOW_JS: &str = "async function check(input, ctx) { return { ok: true }; }\n\
    async function peek(input, ctx) { return input; }\n";

const CHECK_SCHEMA: &str = r#"{"type": "object", "properties": {"change": {"type": "string"}}}"#;

struct Daemon {
    _dir: tempfile::TempDir,
    _stub: ProviderStub,
    root: PathBuf,
    embedded: Embedded,
    reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    next_id: u64,
}

impl Daemon {
    async fn start(host_plugins: impl FnOnce(&Path) -> Vec<PathBuf>) -> anyhow::Result<Self> {
        let stub = ProviderStub::start().await?;
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let data = root.join("data");
        let project = root.join("project");
        std::fs::create_dir_all(&data)?;
        std::fs::create_dir_all(&project)?;
        let settings = json!({
            "providers": [{
                "id": "stub",
                "api": "anthropic",
                "base_url": stub.base_url().as_str(),
                "tiers": {"strong": "claude-sonnet-4-6"},
                "models": ["claude-sonnet-4-6"]
            }]
        });
        std::fs::write(data.join("settings.json"), serde_json::to_vec(&settings)?)?;
        let mut config = EmbedConfig::new(&data);
        config.credentials = Some(blackbox::stub_credentials());
        config.cwd = Some(project);
        config.plugins = host_plugins(&root);
        let embedded = Embedded::start(config).await?;
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        embedded.serve_stream(theirs);
        let (r, w) = tokio::io::split(ours);
        Ok(Self {
            _dir: dir,
            _stub: stub,
            root,
            embedded,
            reader: BufReader::new(r),
            writer: w,
            next_id: 1,
        })
    }

    async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.writer
            .write_all(format!("{frame}\n").as_bytes())
            .await?;
        self.writer.flush().await?;
        loop {
            let mut line = String::new();
            let read =
                tokio::time::timeout(Duration::from_secs(60), self.reader.read_line(&mut line))
                    .await??;
            anyhow::ensure!(read > 0, "the connection closed");
            let frame: Value = serde_json::from_str(line.trim())?;
            if frame.get("id") == Some(&json!(id)) {
                return Ok(frame);
            }
        }
    }

    async fn install(
        &mut self,
        name: &str,
        version: &str,
        entries: &[Entry<'_>],
    ) -> anyhow::Result<Value> {
        let (archive, _) = zip_with_entries(&self.root.join("archives"), name, entries)?;
        self.call(
            "plugin.install",
            json!({ "name": name, "version": version, "download_url": file_url(&archive) }),
        )
        .await
    }

    async fn listed(&mut self) -> anyhow::Result<Vec<Value>> {
        let listed = self.call("plugin.list", json!({})).await?;
        Ok(listed["result"]["plugins"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("plugin.list: {listed}")))
    }
}

fn xflow_entries(manifest: &str) -> Vec<Entry<'_>> {
    vec![
        Entry::File("plugin.toml", manifest.as_bytes().to_vec()),
        Entry::File("xflow.js", XFLOW_JS.as_bytes().to_vec()),
        Entry::File("schemas/check.json", CHECK_SCHEMA.as_bytes().to_vec()),
    ]
}

fn named<'a>(plugins: &'a [Value], name: &str, source: &str) -> &'a Value {
    plugins
        .iter()
        .find(|p| p["name"] == name && p["source"] == source)
        .unwrap_or_else(|| panic!("no {source} package `{name}`: {plugins:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_installed_package_discloses_and_lists_its_functions() -> anyhow::Result<()> {
    let mut daemon = Daemon::start(|_| Vec::new()).await?;
    let installed = daemon
        .install("xflow", "1.0.0", &xflow_entries(XFLOW))
        .await?;
    let disclosure = &installed["result"]["disclosure"];
    assert_eq!(
        disclosure["functions"],
        json!(["xflow.check", "xflow.peek"]),
        "{installed}"
    );
    let capabilities = disclosure["capabilities"].as_array().unwrap();
    assert!(
        capabilities.contains(&json!(
            "exposes function `xflow.check` to the host; may write reports/**"
        )),
        "{capabilities:?}"
    );
    assert!(
        capabilities.contains(&json!(
            "exposes function `xflow.peek` to the host; writes no files"
        )),
        "{capabilities:?}"
    );
    assert_eq!(
        disclosure["model_visible"],
        json!([]),
        "a function is not the model's"
    );

    let plugins = daemon.listed().await?;
    let xflow = named(&plugins, "xflow", "installed");
    assert_eq!(xflow["enabled"], false, "{xflow}");
    assert_eq!(
        xflow["functions"],
        json!([
            {"name": "check", "description": "gate check", "writes": ["reports/**"],
             "has_input_schema": true},
            {"name": "peek", "description": "look only", "writes": [], "has_input_schema": false},
        ]),
        "{xflow}"
    );
    assert_eq!(xflow["commands"], json!([]), "{xflow}");

    let broken = XFLOW
        .replace("schemas/check.json", "schemas/none.json")
        .replace("1.0.0", "1.0.1");
    let refused = daemon
        .install("xflow", "1.0.1", &xflow_entries(&broken))
        .await?;
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("input_schema `schemas/none.json`")),
        "{refused}"
    );
    daemon.embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_package_that_uses_a_library_the_host_lacks_is_not_installed() -> anyhow::Result<()> {
    let mut daemon = Daemon::start(|_| Vec::new()).await?;
    let manifest = XFLOW.replace("uses = []", "uses = [\"codix\"]");
    let refused = daemon
        .install("xflow", "1.0.0", &xflow_entries(&manifest))
        .await?;
    assert!(
        refused["error"]["message"].as_str().is_some_and(
            |m| m.contains("needs host library `codix`, which this host does not provide")
        ),
        "{refused}"
    );
    assert!(
        daemon.listed().await?.iter().all(|p| p["name"] != "xflow"),
        "a refused package is not in the cache"
    );
    daemon.embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_package_is_listed_as_the_hosts_and_beyond_uninstall_and_the_one_it_shadows_is_not(
) -> anyhow::Result<()> {
    let mut daemon = Daemon::start(|root| {
        let dir = root.join("host-xflow");
        std::fs::create_dir_all(dir.join("schemas")).unwrap();
        std::fs::write(dir.join("plugin.toml"), XFLOW).unwrap();
        std::fs::write(dir.join("xflow.js"), XFLOW_JS).unwrap();
        std::fs::write(dir.join("schemas/check.json"), CHECK_SCHEMA).unwrap();
        vec![dir]
    })
    .await?;

    let plugins = daemon.listed().await?;
    let host = named(&plugins, "xflow", "host");
    assert_eq!(host["enabled"], true, "{host}");
    assert!(
        host["root"].as_str().unwrap().ends_with("host-xflow"),
        "{host}"
    );
    assert_eq!(host["functions"][0]["name"], "check", "{host}");

    let mine = XFLOW.replace(
        "version = \"1.0.0\"",
        "version = \"1.0.0\"\ndescription = \"mine\"",
    );
    let installed = daemon
        .install("xflow", "1.0.0", &xflow_entries(&mine))
        .await?;
    assert_eq!(installed["result"]["success"], true, "{installed}");
    let plugins = daemon.listed().await?;
    let shadowed = named(&plugins, "xflow", "installed");
    assert_eq!(shadowed["shadowed_by"], "host", "{shadowed}");
    assert_eq!(shadowed["enabled"], false, "{shadowed}");
    assert_eq!(named(&plugins, "xflow", "host")["enabled"], true);

    let removed = daemon
        .call("plugin.uninstall", json!({"name": "xflow"}))
        .await?;
    assert_eq!(removed["result"]["success"], true, "{removed}");
    let plugins = daemon.listed().await?;
    assert_eq!(plugins.len(), 1, "only the host's is left: {plugins:?}");

    for (method, params) in [
        ("plugin.uninstall", json!({"name": "xflow"})),
        ("plugin.disable", json!({"name": "xflow"})),
    ] {
        let refused = daemon.call(method, params).await?;
        assert!(
            refused["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("`xflow` is the host's own package")),
            "{method}: {refused}"
        );
    }
    assert_eq!(
        named(&daemon.listed().await?, "xflow", "host")["enabled"],
        true
    );
    daemon.embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_package_that_does_not_load_fails_the_start() -> anyhow::Result<()> {
    let started = Daemon::start(|root| {
        let dir = root.join("host-broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            XFLOW.replace("uses = []", "uses = [\"codix\"]"),
        )
        .unwrap();
        std::fs::write(dir.join("xflow.js"), XFLOW_JS).unwrap();
        vec![dir]
    })
    .await;
    let e = format!("{:#}", started.err().expect("the start fails"));
    assert!(e.contains("needs host library `codix`"), "{e}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_installed_package_whose_library_is_gone_is_listed_but_not_loaded() -> anyhow::Result<()>
{
    let mut daemon = Daemon::start(|root| {
        let dir = root.join("data/plugins/cache/xflow/1.0.0");
        std::fs::create_dir_all(dir.join("schemas")).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            XFLOW.replace("uses = []", "uses = [\"codix\"]"),
        )
        .unwrap();
        std::fs::write(dir.join("xflow.js"), XFLOW_JS).unwrap();
        std::fs::write(dir.join("schemas/check.json"), CHECK_SCHEMA).unwrap();
        std::fs::write(dir.join(".agix-installed"), "").unwrap();
        Vec::new()
    })
    .await?;
    let plugins = daemon.listed().await?;
    let xflow = named(&plugins, "xflow", "installed");
    assert_eq!(xflow["enabled"], false, "{xflow}");
    assert_eq!(
        xflow["disabled_reason"], "needs host library `codix`, which this host does not provide",
        "{xflow}"
    );

    let doctor = daemon.call("daemon.doctor", json!({})).await?;
    let check = doctor["result"]["health"]["checks"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["name"] == "plugins.refused"))
        .cloned()
        .unwrap_or_else(|| panic!("no plugins.refused: {doctor}"));
    assert_eq!(check["status"], "degraded", "{check}");
    assert!(
        check["details"]["refused"]
            .to_string()
            .contains("needs host library `codix`"),
        "{check}"
    );
    daemon.embedded.shutdown().await;
    Ok(())
}
