//! Sub-agents on a different provider than their parent, with two real
//! providers behind two recording proxies: `ds` (the DeepSeek endpoint,
//! Flash) and `cc` (real Anthropic, Sonnet). The agent type's `provider:`
//! frontmatter, `session.startAgent`'s `provider` and the Agent tool's
//! `provider` input each send a sub-agent to the other provider; the task's
//! `detail.model` / `detail.provider` and `session.get` report where it ran.
//! Also here: a team member and two concurrent sub-agents on different
//! providers, a sub-agent that fails over from `ds` to `cc` mid-run,
//! `model.order` and an untrusted project restricting who may name `cc`, and
//! the main session's own naming and compaction calls on real Anthropic.
//! Skipped, with the reason printed, when no second provider is configured
//! (`CLAUDE_BASE_URL` and `CLAUDE_API_KEY`).

use std::time::Duration;

use blackbox::{LiveConfig, LiveProvider, LiveRun, LiveRunBuilder, ModelEndpoint, Purpose, Reply};
use serde_json::{json, Value};

const CASE_MAX_CALLS: usize = 30;

fn nonce() -> String {
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        % 0xffff_ffff_ffff;
    format!(
        "{:x}{:x}",
        clock,
        CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
    start_with(name, &order, json!({}), |b| b).await
}

/// [`start`] with `extra` settings keys replacing the defaults, and `tweak`
/// applied to the builder.
async fn start_with(
    name: &str,
    order: &[&str],
    extra: Value,
    tweak: impl FnOnce(LiveRunBuilder) -> LiveRunBuilder,
) -> anyhow::Result<Option<LiveRun>> {
    if LiveConfig::second_model_or_skip(name)?.is_none() {
        return Ok(None);
    }
    let mut run = tweak(LiveRun::builder(name).max_calls(CASE_MAX_CALLS))
        .start()
        .await?;
    let second = run.second().expect("second provider is configured");
    let mut settings = LiveRun::base_settings();
    settings["providers"] = providers(&run.provider, second);
    settings["model"] = json!({ "order": order });
    settings["permissions"] = json!({"mode": "default", "allow": ["Agent"]});
    for (key, value) in extra.as_object().into_iter().flatten() {
        settings[key] = value.clone();
    }
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

fn agent_type_on(provider: &str, marker: &str) -> String {
    format!(
        "---\ndescription: Reads one file\nprovider: {provider}\n---\n\
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

fn agent_tasks(tasks: &[Value]) -> Vec<&Value> {
    tasks.iter().filter(|t| t["kind"] == "agent").collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_team_member_whose_type_names_cc_runs_there() -> anyhow::Result<()> {
    let Some(mut run) = start_with(
        "cross-member",
        &["ds", "cc"],
        json!({
            "team": {"enabled": true},
            "permissions": {"mode": "default", "allow": ["Agent", "TeamCreate"]},
        }),
        |b| b,
    )
    .await?
    else {
        return Ok(());
    };
    let cc_model = run.second().unwrap().config().model.clone();
    let (word, marker) = (format!("HERON-{}", nonce()), format!("ROLE-{}", nonce()));
    run.write_file("note.txt", &format!("{word}\n"))?;
    run.write_file(
        ".agix/agents/cc-member.md",
        &agent_type("cc", &cc_model, &marker),
    )?;

    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            "Use the TeamCreate tool (load it with ToolSearch if needed) to run a team named \
             duo, with permission_mode default and one stage named read with one agent \
             labelled reader, agent_type cc-member, prompt \"Read note.txt and answer with its \
             content only.\" Do not read note.txt yourself. When the team returns, reply with \
             the reader's answer.",
        )
        .await?;
    assert!(
        events.tool_uses.iter().any(|(n, _)| n == "TeamCreate"),
        "{:?}",
        events.tool_uses
    );
    assert!(events.text.contains(&word), "{}", events.text);

    let cc = run.second().unwrap();
    assert!(
        requests_with_marker(cc, &marker) >= 1,
        "provider cc never saw the member"
    );
    assert!(cc.calls().iter().all(|c| c.model() == cc_model));
    assert_eq!(requests_with_marker(&run.provider, &marker), 0);

    let tasks = run
        .wait_tasks_settled(&session, Duration::from_secs(300))
        .await?;
    let member = tasks
        .iter()
        .find(|t| t["kind"] == "teammate")
        .ok_or_else(|| anyhow::anyhow!("no teammate task: {tasks:?}"))?;
    assert_eq!(member["detail"]["provider"], "cc", "{member}");
    assert_eq!(member["detail"]["model"], cc_model.as_str(), "{member}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn two_sub_agents_in_one_message_run_on_their_own_providers() -> anyhow::Result<()> {
    let Some(mut run) = start("cross-pair", ["ds", "cc"]).await? else {
        return Ok(());
    };
    let (ds_model, cc_model) = (
        run.config().model.clone(),
        run.second().unwrap().config().model.clone(),
    );
    let (left, right) = (format!("LEFT-{}", nonce()), format!("RIGHT-{}", nonce()));
    let ds_marker = format!("ROLE-DS-{}", nonce());
    let cc_marker = format!("ROLE-CC-{}", nonce());
    run.write_file("left.txt", &format!("{left}\n"))?;
    run.write_file("right.txt", &format!("{right}\n"))?;
    run.write_file(
        ".agix/agents/ds-reader.md",
        &agent_type("ds", &ds_model, &ds_marker),
    )?;
    run.write_file(
        ".agix/agents/cc-reader.md",
        &agent_type("cc", &cc_model, &cc_marker),
    )?;

    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            "In a single message, call the Agent tool twice, in the foreground: subagent_type \
             ds-reader with prompt \"Read left.txt\", and subagent_type cc-reader with prompt \
             \"Read right.txt\". Do not read the files yourself. Then reply with both answers.",
        )
        .await?;
    assert!(
        events.text.contains(&left) && events.text.contains(&right),
        "{}",
        events.text
    );

    let cc = run.second().unwrap();
    assert!(requests_with_marker(cc, &cc_marker) >= 1);
    assert_eq!(requests_with_marker(cc, &ds_marker), 0);
    assert!(requests_with_marker(&run.provider, &ds_marker) >= 1);
    assert_eq!(requests_with_marker(&run.provider, &cc_marker), 0);

    let tasks = run
        .wait_tasks_settled(&session, Duration::from_secs(300))
        .await?;
    let agents = agent_tasks(&tasks);
    assert_eq!(agents.len(), 2, "{tasks:?}");
    let placed: std::collections::BTreeSet<(String, String)> = agents
        .iter()
        .map(|t| {
            (
                t["detail"]["provider"].as_str().unwrap_or("").to_string(),
                t["detail"]["model"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let expected = [("ds".to_string(), ds_model), ("cc".to_string(), cc_model)];
    assert_eq!(placed, expected.into_iter().collect(), "{tasks:?}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_whose_first_provider_fails_finishes_on_the_next() -> anyhow::Result<()> {
    let Some(mut run) = start("cross-failover", ["ds", "cc"]).await? else {
        return Ok(());
    };
    let cc_model = run.second().unwrap().config().model.clone();
    let (word, marker) = (format!("EGRET-{}", nonce()), format!("ROLE-{}", nonce()));
    run.write_file("note.txt", &format!("{word}\n"))?;
    run.write_file(
        ".agix/agents/flaky-reader.md",
        &format!(
            "---\ndescription: Reads one file\n---\n\
             {marker}. Read the file you are told to read and answer with its content only."
        ),
    )?;
    run.provider.inject_failures_when(
        &marker,
        50,
        Reply::status(
            501,
            r#"{"type":"error","error":{"type":"server_error","message":"down"}}"#,
        ),
    );

    let session = run.session().await?;
    let watcher = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            "Call the Agent tool once, in the foreground: subagent_type flaky-reader, prompt \
             \"Read note.txt\". Do not read the file yourself. Then reply with what the \
             sub-agent answered.",
        )
        .await?;
    assert!(events.text.contains(&word), "{}", events.text);

    let cc = run.second().unwrap();
    assert!(
        requests_with_marker(cc, &marker) >= 1,
        "provider cc never took over"
    );
    assert!(cc.calls().iter().all(|c| c.model() == cc_model));
    let first: Vec<_> = run
        .provider
        .exchanges()
        .into_iter()
        .filter(|e| e.request.body.to_string().contains(&marker))
        .collect();
    assert!(!first.is_empty(), "ds never saw the sub-agent first");
    assert!(first.iter().all(|e| e.status == 501), "ds answered it");

    let task = agent_task(&mut run, &session).await?;
    assert_eq!(task["status"], "done", "{task}");
    assert_eq!(task["detail"]["provider"], "cc", "{task}");
    assert_eq!(task["detail"]["model"], cc_model.as_str(), "{task}");
    let moved = watcher
        .events("task_updated")
        .into_iter()
        .filter(|f| {
            f["event"]["task"]["task_id"] == task["task_id"]
                && f["event"]["task"]["detail"]["provider"] == "cc"
        })
        .count();
    assert!(moved >= 1, "no task_updated moved it to cc");
    let switched = watcher
        .frames()
        .iter()
        .filter(|f| {
            f["event"]["kind"] == "provider_switched"
                || f["event"]["event"]["kind"] == "provider_switched"
        })
        .count();
    eprintln!("[live cross-failover] provider_switched frames: {switched}");
    run.finish_allowing_refusals().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_outside_the_order_is_the_hosts_to_name() -> anyhow::Result<()> {
    let Some(mut run) = start_with("cross-outside", &["ds"], json!({}), |b| b).await? else {
        return Ok(());
    };
    let (cc_model, word) = (
        run.second().unwrap().config().model.clone(),
        format!("TERN-{}", nonce()),
    );
    run.write_file("note.txt", &format!("{word}\n"))?;

    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run
        .turn(
            &session,
            "Call the Agent tool once, in the foreground, with prompt \"Read note.txt and \
             answer with its content only\" and with its `provider` input set to \"cc\". Do \
             not read the file yourself. If the tool returns an error, reply with the word \
             REFUSED and do not retry.",
        )
        .await?;
    let refused = events
        .tool_results
        .iter()
        .any(|(name, text, is_error)| name == "Agent" && *is_error && text.contains("model.order"));
    assert!(refused, "{:?}", events.tool_results);
    assert!(
        run.second().unwrap().calls().is_empty(),
        "cc was called although the order leaves it out"
    );

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
    let started_id = started.result.unwrap_or_default()["task_id"].clone();
    let tasks = run
        .wait_tasks_settled(&session, Duration::from_secs(300))
        .await?;
    let task = agent_tasks(&tasks)
        .into_iter()
        .find(|t| t["task_id"] == started_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("host-started agent not listed: {tasks:?}"))?;
    assert_eq!(task["status"], "done", "{task}");
    assert_eq!(task["detail"]["provider"], "cc", "{task}");
    assert_eq!(task["detail"]["model"], cc_model.as_str(), "{task}");
    assert!(!run.second().unwrap().calls().is_empty());
    run.finish_allowing_refusals().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_projects_agent_type_stays_on_ds_until_trusted() -> anyhow::Result<()> {
    if LiveConfig::second_model_or_skip("cross-untrusted")?.is_none() {
        return Ok(());
    }
    let mut run = LiveRun::builder("cross-untrusted")
        .max_calls(CASE_MAX_CALLS)
        .untrusted()
        .start()
        .await?;
    let second = run.second().expect("second provider is configured");
    run.world.write_global_settings(&json!({
        "providers": providers(&run.provider, second),
        "model": {"order": ["ds", "cc"]},
        "permissions": {"allow": ["Agent"]},
    }))?;
    let reloaded = run.client.config_reload(&rpc_client::Target::own()).await?;
    anyhow::ensure!(reloaded.error.is_none(), "{reloaded:?}");
    let (word, marker) = (format!("KITE-{}", nonce()), format!("ROLE-{}", nonce()));
    run.write_file("note.txt", &format!("{word}\n"))?;
    run.write_file(".agix/agents/x.md", &agent_type_on("cc", &marker))?;

    let status = run
        .client
        .call(
            "project.status",
            json!({ "project_root": run.project.to_string_lossy() }),
        )
        .await?
        .result
        .unwrap_or_default();
    assert!(
        status["requires_trust"]
            .as_array()
            .is_some_and(|needs| needs.contains(&json!("agents/x"))),
        "{status}"
    );

    let ask = "Call the Agent tool once, in the foreground: subagent_type x, prompt \"Read \
               note.txt\". Do not read the file yourself. Then reply with what the sub-agent \
               answered.";
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run.turn(&session, ask).await?;
    assert!(events.text.contains(&word), "{}", events.text);
    assert_eq!(
        requests_with_marker(run.second().unwrap(), &marker),
        0,
        "an untrusted type sent its sub-agent to cc"
    );
    assert!(requests_with_marker(&run.provider, &marker) >= 1);
    let task = agent_task(&mut run, &session).await?;
    assert_eq!(task["detail"]["provider"], "ds", "{task}");

    let trusted = run
        .client
        .call(
            "project.trust",
            json!({ "project_root": run.project.to_string_lossy(), "trusted": true }),
        )
        .await?;
    anyhow::ensure!(trusted.error.is_none(), "{:?}", trusted.error);
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let events = run.turn(&session, ask).await?;
    assert!(events.text.contains(&word), "{}", events.text);
    assert!(
        requests_with_marker(run.second().unwrap(), &marker) >= 1,
        "once trusted, the type's provider applies"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn naming_and_compaction_calls_on_anthropic_are_well_formed() -> anyhow::Result<()> {
    let Some(mut run) = start_with("cross-side-calls", &["cc", "ds"], json!({}), |b| {
        b.options(|o| o.scene("harnix"))
    })
    .await?
    else {
        return Ok(());
    };
    let session = run.session().await?;
    run.turn(
        &session,
        "We are planning a database migration for the billing service. Reply with the word \
         NOTED and nothing else.",
    )
    .await?;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let got = run
            .client
            .session_get(&session)
            .await?
            .result
            .unwrap_or_default();
        if got["name"].as_str().is_some_and(|n| !n.is_empty()) {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the session was never named: {got}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    run.turn(
        &session,
        "List three risks of such a migration, one line each.",
    )
    .await?;
    let compacted = run
        .client
        .call("session.compact", json!({ "session_id": session }))
        .await?;
    assert!(compacted.error.is_none(), "{compacted:?}");

    let exchanges = run.second().unwrap().exchanges();
    for purpose in [Purpose::Title, Purpose::Compact] {
        let of: Vec<_> = exchanges
            .iter()
            .filter(|e| e.request.purpose() == purpose)
            .collect();
        assert!(!of.is_empty(), "no {purpose:?} call reached cc");
        assert!(of.iter().all(|e| e.status == 200), "{purpose:?} failed");
    }
    assert!(
        exchanges.iter().all(|e| e.status == 200),
        "cc answered a request with an error: {:?}",
        exchanges.iter().map(|e| e.status).collect::<Vec<_>>()
    );
    for e in &exchanges {
        let body = &e.request.body;
        assert_ne!(
            body["thinking"]["type"],
            "disabled",
            "{:?}",
            e.request.purpose()
        );
        let breakpoints = body.to_string().matches("\"cache_control\"").count();
        assert!(breakpoints <= 4, "{breakpoints} cache breakpoints");
    }
    run.finish_allowing_refusals().await;
    Ok(())
}
