//! Bringing another tool's project configuration in: `import.list` names what
//! a project carries (Cursor rules, a Codex-style `AGENTS.md`), `import.run`
//! converts it, and what it wrote is what the next session's model is told. A
//! daemon that starts in an undecided project announces it as `import_detected`
//! to whoever subscribes, however late.

use std::time::{Duration, Instant};

use blackbox::{LiveConfig, LiveRun, LiveRunBuilder, Mode, ProviderStub, Reply};
use rpc_client::Target;
use serde_json::Value;

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

fn sources(listed: &Value) -> Vec<String> {
    listed["result"]["sources"]
        .as_array()
        .map(|all| {
            all.iter()
                .filter_map(|s| s["source"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn cursor_rules_import_into_agents_md_and_reach_the_model() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("import-cursor", &stub)
        .before_start(|_, project| {
            std::fs::write(project.join(".cursorrules"), "CURSOR-LEGACY-RULE: use tabs\n")?;
            std::fs::create_dir_all(project.join(".cursor/rules"))?;
            std::fs::write(
                project.join(".cursor/rules/style.mdc"),
                "---\ndescription: style guide\nalwaysApply: true\n---\nCURSOR-MDC-RULE: run rustfmt\n",
            )?;
            Ok(())
        })
        .start()
        .await?;

    let listed = run.client.import_list(&Target::own()).await?;
    let found = sources(&serde_json::to_value(&listed)?);
    assert_eq!(found, ["cursor"], "{listed:?}");

    let ran = run.client.import_run(&Target::own(), "cursor").await?;
    let result = ran.result.clone().unwrap_or_default();
    assert_eq!(result["source"], "cursor", "{ran:?}");
    assert!(
        result["actions"].as_array().is_some_and(|a| a.len() >= 2),
        "{result}"
    );

    let agents = run.read_file("AGENTS.md").expect("AGENTS.md was written");
    assert!(agents.contains("CURSOR-LEGACY-RULE"), "{agents}");
    assert!(agents.contains(".agix/rules/style.md"), "{agents}");
    let rule = run
        .read_file(".agix/rules/style.md")
        .expect("the rule file");
    assert!(rule.contains("CURSOR-MDC-RULE"), "{rule}");

    stub.script_when("标题生成器", [Reply::text("title")]);
    let session = run.session().await?;
    stub.script([Reply::text("noted")]);
    run.turn(&session, "hello").await?;
    let call = stub
        .calls()
        .into_iter()
        .find(|c| c.has_tools())
        .expect("a turn call");
    assert!(
        call.messages_contain("CURSOR-LEGACY-RULE") || call.system_contains("CURSOR-LEGACY-RULE"),
        "the imported rule never reached the model"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_codex_style_project_imports_by_gaining_a_skills_directory() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("import-codex", &stub)
        .before_start(|_, project| {
            std::fs::write(project.join("AGENTS.md"), "# Notes\nCODEX-STYLE-NOTES\n")?;
            Ok(())
        })
        .start()
        .await?;

    let listed = run.client.import_list(&Target::own()).await?;
    assert_eq!(
        sources(&serde_json::to_value(&listed)?),
        ["codex"],
        "{listed:?}"
    );
    let ran = run.client.import_run(&Target::own(), "codex").await?;
    assert_eq!(
        ran.result.clone().unwrap_or_default()["source"],
        "codex",
        "{ran:?}"
    );
    assert!(run.project.join(".agents/skills").is_dir());
    let agents = run.read_file("AGENTS.md").unwrap_or_default();
    assert!(
        agents.contains("CODEX-STYLE-NOTES"),
        "the project's own file was rewritten"
    );

    let listed = run.client.import_list(&Target::own()).await?;
    assert!(
        sources(&serde_json::to_value(&listed)?).is_empty(),
        "{listed:?}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real daemon process"]
async fn an_undecided_project_is_announced_to_a_subscriber_that_connects_after_startup(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("import-detected", &stub)
        .mode(Mode::Spawned)
        .before_start(|_, project| {
            std::fs::write(project.join(".cursorrules"), "be brief\n")?;
            Ok(())
        })
        .start()
        .await?;
    // A client can only connect after the daemon is answering, which is
    // after startup detection has had its chance to run.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let events = run.daemon_events().await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let detected = loop {
        if let Some(e) = events
            .all()
            .into_iter()
            .find(|e| e["kind"] == "import_detected")
        {
            break e;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "no import_detected: {:?}",
            events.all()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(detected["sources"][0]["source"], "cursor", "{detected}");

    run.client.import_run(&Target::own(), "cursor").await?;
    run.restart().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let again = run.daemon_events().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        again.all().iter().all(|e| e["kind"] != "import_detected"),
        "an answered question was announced again: {:?}",
        again.all()
    );
    run.finish().await;
    Ok(())
}
