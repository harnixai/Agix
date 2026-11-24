//! Sub-agents on a different provider than their parent, with two real
//! providers behind two recording proxies: `ds` (the DeepSeek endpoint,
//! Flash) and `cc` (real Anthropic, Sonnet). The agent type's `provider:`
//! frontmatter, `session.startAgent`'s `provider` and the Agent tool's
//! `provider` input each send a sub-agent to the other provider; the task's
//! `detail.model` / `detail.provider` and `session.get` report where it ran.
//! Skipped, with the reason printed, when no second provider is configured
//! (`CLAUDE_BASE_URL` and `CLAUDE_API_KEY`).

use std::time::Duration;

use blackbox::{LiveConfig, LiveProvider, LiveRun, ModelEndpoint};
use serde_json::{json, Value};

const CASE_MAX_CALLS: usize = 30;

fn nonce() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 0xffff_ffff_ffff
    )
}

fn providers(ds: &LiveProvider, cc: &LiveProvider) -> Value {
    let provider = |id: &str, proxy: &LiveProvider| {
        let model = proxy.config().model.as_str();
        json!({
            "id": id,
            "api": "anthropic",
            "base_url": proxy.base_url().as_str(),
            "api_key_env": "ANTHROPIC_AUTH_TOKEN",
            "tiers": {"strong": model, "balanced": model, "fast": model},
        })
    };
    json!([provider("ds", ds), provider("cc", cc)])
}

/// A run whose project has both providers; `order` says which one sessions
/// prefer. Returns `None` after printing the skip reason when there is no
/// second provider.
async fn start(name: &str, order: [&str; 2]) -> anyhow::Result<Option<LiveRun>> {
    if LiveConfig::second_model_or_skip(name)?.is_none() {
        return Ok(None);
    }
    let mut run = LiveRun::builder(name)
        .max_calls(CASE_MAX_CALLS)
        .start()
        .await?;
    let second = run.second().expect("second provider is configured");
    let mut settings = LiveRun::base_settings();
    settings["providers"] = providers(&run.provider, second);
    settings["model"] = json!({ "order": order });
    settings["permissions"] = json!({"mode": "default", "allow": ["Agent"]});
    run.world.write_project_settings(name, &settings)?;
    let reloaded = run.client.config_reload(&rpc_client::Target::own()).await?;
    anyhow::ensure!(reloaded.error.is_none(), "{reloaded:?}");
    Ok(Some(run))
}

fn agent_type(provider: &str, model: &str, marker: &str) -> String {
    format!(
        "---\ndescription: Reads one file\nprovider: {provider}\nmodel: {model}\n---\n\
         {marker}. Read the file you are told to read and answer with its content only."
    )
}

fn requests_with_marker(proxy: &LiveProvider, marker: &str) -> usize {
    proxy
        .calls()
        .iter()
        .filter(|c| c.body.to_string().contains(marker))
        .count()
}

async fn agent_task(run: &mut LiveRun, session: &str) -> anyhow::Result<Value> {
    let tasks = run
        .wait_tasks_settled(session, Duration::from_secs(300))
        .await?;
    tasks
        .into_iter()
        .find(|t| t["kind"] == "agent")
        .ok_or_else(|| anyhow::anyhow!("no agent task in the session"))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_type_naming_a_provider_runs_its_sub_agent_there() -> anyhow::Result<()> {
    let Some(mut run) = start("cross-type", ["ds", "cc"]).await? else {
        return Ok(());
    };
    let second_model = run.second().unwrap().config().model.clone();
    let (word, marker) = (format!("PELICAN-{}", nonce()), format!("ROLE-{}", nonce()));
    run.write_file("note.txt", &format!("{word}\n"))?;
    run.write_file(
        ".agix/agents/cc-reader.md",
        &agent_type("cc", &second_model, &marker),
    )?;

    let session = run.session().await?;
    let watcher = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            "Call the Agent tool once, in the foreground: subagent_type cc-reader, prompt \
             \"Read note.txt\". Do not read the file yourself. Then reply with what the \
             sub-agent answered.",
        )
        .await?;
    assert!(events.text.contains(&word), "{}", events.text);

    let cc = run.second().unwrap();
    assert!(
        requests_with_marker(cc, &marker) >= 1,
        "provider cc never saw the sub-agent"
    );
    assert!(cc.calls().iter().all(|c| c.model() == second_model));
    assert_eq!(
        requests_with_marker(&run.provider, &marker),
        0,
        "provider ds saw the sub-agent's request"
    );

    let task = agent_task(&mut run, &session).await?;
    assert_eq!(task["detail"]["provider"], "cc", "{task}");
    assert_eq!(task["detail"]["model"], second_model.as_str(), "{task}");
    let started = watcher.events("task_started");
    let started = started
        .iter()
        .find(|f| f["event"]["task"]["task_id"] == task["task_id"])
        .cloned()
        .unwrap_or_default();
    assert_eq!(started["event"]["task"]["detail"]["provider"], "cc");
    assert_eq!(
        started["event"]["task"]["detail"]["model"],
        second_model.as_str()
    );

    let sidechain = task["detail"]["agent_session_id"]
        .as_str()
        .unwrap_or_default();
    let got = run
        .client
        .session_get(sidechain)
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(got["provider"], "cc", "{got}");
    assert_eq!(got["model"], second_model.as_str(), "{got}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_started_agent_can_name_the_provider() -> anyhow::Result<()> {
    let Some(mut run) = start("cross-host", ["ds", "cc"]).await? else {
        return Ok(());
    };
    let second_model = run.second().unwrap().config().model.clone();
    let word = format!("HERON-{}", nonce());
    run.write_file("note.txt", &format!("{word}\n"))?;

    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let started = run
        .client
        .call(
            "session.startAgent",
            json!({
                "session_id": session,
                "prompt": "Read note.txt and answer with its content only.",
                "description": "reader",
                "provider": "cc",
            }),
        )
        .await?;
    assert!(started.error.is_none(), "{started:?}");

    let task = agent_task(&mut run, &session).await?;
    assert_eq!(task["status"], "done", "{task}");
    let output = run
        .client
        .call(
            "session.taskOutput",
            json!({"session_id": session, "task_id": task["task_id"]}),
        )
        .await?;
    assert!(format!("{:?}", output.result).contains(&word), "{output:?}");
    assert_eq!(task["detail"]["provider"], "cc", "{task}");

    let cc = run.second().unwrap();
    assert!(!cc.calls().is_empty(), "provider cc was never called");
    assert!(cc.calls().iter().all(|c| c.model() == second_model));
    assert!(
        run.provider.calls().is_empty(),
        "nothing but the sub-agent ran, and it ran on cc"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_tool_takes_a_provider() -> anyhow::Result<()> {
    let Some(mut run) = start("cross-tool", ["ds", "cc"]).await? else {
        return Ok(());
    };
    let word = format!("CRANE-{}", nonce());
    run.write_file("note.txt", &format!("{word}\n"))?;

    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            "Call the Agent tool once, in the foreground, with prompt \"Read note.txt and \
             answer with its content only\" and with its `provider` input set to \"cc\". Do \
             not read the file yourself. Then reply with what the sub-agent answered.",
        )
        .await?;
    let input = events
        .tool_uses
        .iter()
        .find(|(n, _)| n == "Agent")
        .map(|(_, input)| input.clone())
        .unwrap_or_default();
    assert_eq!(input["provider"], "cc", "{:?}", events.tool_uses);
    assert!(events.text.contains(&word), "{}", events.text);

    let cc = run.second().unwrap();
    assert!(!cc.calls().is_empty(), "provider cc was never called");
    let task = agent_task(&mut run, &session).await?;
    assert_eq!(task["detail"]["provider"], "cc", "{task}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_on_cc_sends_a_sub_agent_to_ds_and_usage_stays_apart() -> anyhow::Result<()> {
    let Some(mut run) = start("cross-reverse", ["cc", "ds"]).await? else {
        return Ok(());
    };
    let (ds_model, cc_model) = (
        run.config().model.clone(),
        run.second().unwrap().config().model.clone(),
    );
    let (word, marker) = (format!("OSPREY-{}", nonce()), format!("ROLE-{}", nonce()));
    run.write_file("note.txt", &format!("{word}\n"))?;
    run.write_file(
        ".agix/agents/ds-reader.md",
        &agent_type("ds", &ds_model, &marker),
    )?;

    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            "Call the Agent tool once, in the foreground: subagent_type ds-reader, prompt \
             \"Read note.txt\". Do not read the file yourself. Then reply with what the \
             sub-agent answered.",
        )
        .await?;
    assert!(events.text.contains(&word), "{}", events.text);

    let cc = run.second().unwrap();
    assert!(
        cc.calls().iter().all(|c| c.model() == cc_model),
        "the parent stayed on cc's model"
    );
    assert_eq!(
        requests_with_marker(cc, &marker),
        0,
        "provider cc saw the sub-agent's request"
    );
    let ds_calls = run.provider.calls();
    assert!(!ds_calls.is_empty(), "provider ds was never called");
    assert!(ds_calls
        .iter()
        .all(|c| c.body.to_string().contains(&marker)));
    assert!(ds_calls.iter().all(|c| c.model() == ds_model));

    let task = agent_task(&mut run, &session).await?;
    assert_eq!(task["detail"]["provider"], "ds", "{task}");
    assert_eq!(task["detail"]["model"], ds_model.as_str(), "{task}");
    let (_, ds_input, ds_output) = run.provider.spend();
    let (_, cc_input, cc_output) = run.second().unwrap().spend();
    assert_eq!(task["usage"]["input_tokens"], ds_input, "{task}");
    assert_eq!(task["usage"]["output_tokens"], ds_output, "{task}");
    assert!(
        cc_input + cc_output > 0,
        "the parent spent on cc, outside the task's usage"
    );
    run.finish().await;
    Ok(())
}
