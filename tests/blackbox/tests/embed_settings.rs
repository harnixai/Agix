//! An embedder that gives settings of its own (`EmbedConfig.settings`, with
//! `max_tokens` written in), over `Embedded::serve_stream`: they hold over the
//! settings files of the daemon's own project and of another one — the
//! permission mode a session reports and the `max_tokens` its turns ask for —
//! and `config.get` reports them as the `cli` tier; `app.session_cap` there
//! bounds the sessions in memory. A key no setting reads, an old key, a value
//! that does not fit or settings that are not an object are refused at start,
//! naming the key.

use std::path::{Path, PathBuf};

use blackbox::{ProviderStub, Reply};
use daemon::embed::{EmbedConfig, EmbedError, Embedded};
use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};

struct Root {
    _dir: tempfile::TempDir,
    data: PathBuf,
    project: PathBuf,
    other: PathBuf,
}

/// Two projects, and a data root whose settings route to `stub` and ask for
/// 999 output tokens.
fn root(stub: &ProviderStub) -> anyhow::Result<Root> {
    let dir = tempfile::tempdir()?;
    let base = dir.path().canonicalize()?;
    let (data, project, other) = (base.join("data"), base.join("project"), base.join("other"));
    for dir in [&data, &project, &other] {
        std::fs::create_dir_all(dir)?;
    }
    let settings = json!({
        "providers": [{
            "id": "stub",
            "api": "anthropic",
            "base_url": stub.base_url().as_str(),
            "tiers": {"strong": "claude-sonnet-4-6"},
            "models": ["claude-sonnet-4-6"]
        }],
        "model": {"max_tokens": 999},
        "memory": {"enabled": false},
        "permissions": {"mode": "acceptEdits"}
    });
    std::fs::write(data.join("settings.json"), serde_json::to_vec(&settings)?)?;
    Ok(Root {
        _dir: dir,
        data,
        project,
        other,
    })
}

fn config(root: &Root, settings: Value) -> EmbedConfig {
    let mut config = EmbedConfig::new(&root.data);
    config.credentials = Some(blackbox::stub_credentials());
    config.cwd = Some(root.project.clone());
    config.settings = Some(settings);
    config
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

/// The mode a new session on `project` reports, and the `max_tokens` its
/// turn asked for.
async fn session_on(
    client: &mut DaemonRpcClient,
    stub: &ProviderStub,
    project: &Path,
) -> anyhow::Result<(Value, Vec<Value>)> {
    let session = client
        .session_create(json!({"project_root": project}))
        .await?;
    let resumed = call(client, "session.resume", json!({"session_id": session})).await?;
    let needle = format!("EMBED-SETTINGS-{}", project.display());
    stub.script_when(&needle, [Reply::text("ok")]);
    let events = client
        .session_run_turn(&session, &needle, "t1", None)
        .await?;
    anyhow::ensure!(events.turn_complete, "{events:?}");
    let asked = stub
        .calls()
        .iter()
        .filter(|r| r.body.to_string().contains(&needle))
        .map(|r| r.body["max_tokens"].clone())
        .collect();
    Ok((resumed["permission"]["mode"].clone(), asked))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_embedders_settings_hold_over_every_pairs_files() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    stub.script_when("标题生成器", (0..4).map(|_| Reply::text("a title")));
    let root = root(&stub)?;
    std::fs::create_dir_all(root.other.join(".agix"))?;
    std::fs::write(
        root.other.join(".agix/settings.json"),
        r#"{"model": {"max_tokens": 4321}}"#,
    )?;
    let mut config = config(
        &root,
        json!({"permissions": {"mode": "plan"}, "model": {"max_tokens": 1}}),
    );
    config.max_tokens = Some(1234);
    let embedded = Embedded::start(config).await?;
    let mut client = connect(&embedded);

    for project in [&root.project, &root.other] {
        let (mode, asked) = session_on(&mut client, &stub, project).await?;
        assert_eq!(mode, "plan", "{}", project.display());
        assert_eq!(asked, [json!(1234)], "{}", project.display());
    }
    let cli = call(
        &mut client,
        "config.get",
        json!({"scene": "code", "tier": "cli"}),
    )
    .await?;
    assert_eq!(
        cli["settings"],
        json!({"permissions": {"mode": "plan"}, "model": {"max_tokens": 1234}})
    );
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_no_setting_takes_are_refused_at_start() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let root = root(&stub)?;
    for (settings, key, says) in [
        (
            json!({"turn": {"max_model_calz": 1}}),
            "turn.max_model_calz",
            "no setting reads",
        ),
        (
            json!({"permission_mode": "plan"}),
            "permission_mode",
            "permissions.mode",
        ),
        (
            json!({"turn": {"max_model_calls": "abc"}}),
            "turn.max_model_calls",
            "invalid type",
        ),
        (json!(["plan"]), "", "not a JSON object"),
    ] {
        match Embedded::start(config(&root, settings.clone())).await {
            Err(EmbedError::InvalidSetting {
                key: refused,
                reason,
            }) => {
                assert_eq!(refused, key, "{settings}");
                assert!(reason.contains(says), "{settings}: {reason}");
            }
            Err(other) => panic!("{settings}: {other}"),
            Ok(_) => panic!("{settings} was taken"),
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn app_session_cap_in_the_embedders_settings_holds_the_engine_to_one_session(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    stub.script_when("标题生成器", (0..2).map(|_| Reply::text("a title")));
    let root = root(&stub)?;
    let embedded = Embedded::start(config(&root, json!({"app": {"session_cap": 1}}))).await?;
    let mut client = connect(&embedded);

    let mut sessions = Vec::new();
    for turn in ["EMBED-CAP-ONE", "EMBED-CAP-TWO"] {
        let session = client.session_create(json!({})).await?;
        stub.script_when(turn, [Reply::text("ok")]);
        let events = client.session_run_turn(&session, turn, "t1", None).await?;
        anyhow::ensure!(events.turn_complete, "{events:?}");
        sessions.push(session);
    }
    for (session, status) in sessions.iter().zip(["inactive", "active"]) {
        let got = call(&mut client, "session.get", json!({"session_id": session})).await?;
        assert_eq!(got["status"], status, "{got}");
    }
    embedded.shutdown().await;
    Ok(())
}
