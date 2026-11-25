//! `mcp.callTool` / `mcp.tools` where a host reaches past what a session
//! could: a scene the daemon makes no session in. Only what the RPC and the
//! toy server's call log show.

use std::path::Path;

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, McpToy, ProviderStub};
use serde_json::{json, Value};

const SCENE_NOT_FOUND: i32 = -32004;

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

fn toy(name: &str, calls: &Path, extra: &[&str]) -> anyhow::Result<Value> {
    let calls = calls.to_string_lossy();
    let mut args = vec!["--name", name, "--calls", &calls];
    args.extend_from_slice(extra);
    McpToy::stdio_config(&args)
}

fn lines_of(path: &Path, prefix: &str) -> usize {
    McpToy::calls(path)
        .iter()
        .filter(|(_, t)| t.starts_with(prefix))
        .count()
}

/// A registered scene that is not active gets no session: `session.create`
/// on it is `SCENE_NOT_FOUND`. A host call on it must not connect that pair
/// and call its servers either.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_on_a_scene_no_session_could_be_made_in_is_refused() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let scratch = tempfile::tempdir()?;
    let calls = scratch.path().join("calls.log");
    let mut run = stubbed("mcp-dormant-scene", &stub)
        .settings(json!({ "mcp": { "servers": { "toy": toy("toy", &calls, &[])? } } }))
        .before_start(|world, _| {
            world.write_declarative_scene("dormant", &["mcp__toy__ping"], "dormant")?;
            Ok(())
        })
        .start()
        .await?;
    let project = run.project.to_string_lossy().to_string();
    let created = run
        .client
        .call(
            "session.create",
            json!({ "scene": "dormant", "project_root": project }),
        )
        .await?;
    let refused = created.error.expect("a session on an inactive scene");
    assert_eq!(refused.code, SCENE_NOT_FOUND, "{}", refused.message);

    let resp = run
        .client
        .call(
            "mcp.callTool",
            json!({ "server": "toy", "tool": "ping", "arguments": { "message": "x" },
                    "scene": "dormant" }),
        )
        .await?;
    assert_eq!(
        lines_of(&calls, "ping"),
        0,
        "a host call reached a server of a scene no session can be made in: {resp:?}"
    );
    let error = resp.error.expect("refused like session.create");
    assert_eq!(error.code, SCENE_NOT_FOUND, "{}", error.message);
    run.finish_allowing_refusals().await;
    Ok(())
}
