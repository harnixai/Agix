//! A session's own settings, `session.create`'s `options.settings`: by default
//! a value looser than the settings' is held to them and reported in
//! `clamped_options`, a key decided for the project and scene or the process
//! refuses the call; in force for the session and its sub-agents, and still
//! in force after a restart — a session recorded in the 0.3 or 0.4.0 shape too.
//! Its reasoning and prompt are keys of it, set at creation or by
//! `session.setModel` / `session.setPrompt`, and the option names they
//! replaced are refused with the new ones.

use std::io::Write as _;

use blackbox::{
    LiveConfig, LiveRun, LiveRunBuilder, Page, PageServer, ProviderStub, Reply, Watcher,
};
use serde_json::{json, Value};

const ROOT: &str = "ROOT-MARK";

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name).config(LiveConfig::for_endpoint(
        &stub.base_url(),
        blackbox::ModelEndpoint::model(stub),
    ))
}

async fn create(run: &mut LiveRun, options: Value) -> anyhow::Result<daemon::rpc::RpcResponse> {
    let mut options = options;
    options["auto_name"] = json!(false);
    let project = run.project.to_string_lossy().to_string();
    run.client
        .call(
            "session.create",
            json!({ "project_root": project, "options": options }),
        )
        .await
}

async fn session(run: &mut LiveRun, options: Value) -> anyhow::Result<String> {
    let created = create(run, options).await?;
    anyhow::ensure!(created.error.is_none(), "{created:?}");
    Ok(created.result.unwrap_or_default()["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

fn write(id: &str, path: &str) -> Reply {
    Reply::calls(id, "Write", json!({ "file_path": path, "content": "x" }))
}

fn prompted_tools(watcher: &Watcher) -> Vec<String> {
    watcher
        .asked()
        .iter()
        .map(|a| {
            a.prompt["event"]["tool_name"]
                .as_str()
                .unwrap_or("?")
                .to_string()
        })
        .collect()
}

fn refused(events: &rpc_client::TurnEvents, tool: &str) -> bool {
    events
        .tool_results
        .iter()
        .any(|(name, _, is_error)| name == tool && *is_error)
}

async fn session_count(run: &mut LiveRun) -> anyhow::Result<usize> {
    Ok(
        run.client.session_list().await?.result.unwrap_or_default()["sessions"]
            .as_array()
            .map_or(0, Vec::len),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_in_plan_writes_nothing_and_one_asking_wider_is_held_and_told(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-plan", &stub)
        .settings(json!({ "permissions": { "mode": "acceptEdits" } }))
        .start()
        .await?;
    let planning = session(
        &mut run,
        json!({ "settings": { "permissions": { "mode": "plan" } } }),
    )
    .await?;
    let _host = run.watch(&planning, true).await?;
    stub.script([write("w1", "planned.txt"), Reply::text("done")]);
    let events = run.turn(&planning, "write it").await?;
    assert!(refused(&events, "Write"), "{:?}", events.tool_results);
    assert!(run.read_file("planned.txt").is_none());

    let created = create(
        &mut run,
        json!({ "settings": { "permissions": { "mode": "bypassPermissions" } } }),
    )
    .await?;
    let result = created.result.unwrap_or_default();
    assert_eq!(
        result["clamped_options"],
        json!([{
            "field": "settings.permissions.mode",
            "requested": "bypassPermissions",
            "applied": "acceptEdits",
        }]),
        "{result}"
    );
    let wide = result["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": wide }))
        .await?;
    assert_eq!(
        resumed.result.unwrap_or_default()["permission"]["mode"],
        "acceptEdits"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_deny_rule_stops_a_write_and_its_allow_rules_wait_for_the_settings(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-rules", &stub)
        .settings(json!({ "permissions": { "mode": "acceptEdits" } }))
        .start()
        .await?;
    let denying = session(
        &mut run,
        json!({ "settings": { "permissions": { "deny": ["Write"] } } }),
    )
    .await?;
    let _host = run.watch(&denying, true).await?;
    stub.script([write("w1", "denied.txt"), Reply::text("done")]);
    let events = run.turn(&denying, "write it").await?;
    assert!(refused(&events, "Write"), "{:?}", events.tool_results);
    assert!(run.read_file("denied.txt").is_none());

    let created = create(
        &mut run,
        json!({ "settings": { "permissions": { "mode": "default", "allow": ["Write"] } } }),
    )
    .await?;
    let result = created.result.unwrap_or_default();
    assert_eq!(
        result["clamped_options"],
        json!([{ "field": "settings.permissions.allow", "requested": ["Write"], "applied": [] }]),
        "{result}"
    );
    let asking = result["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let host = run
        .watch_with(&asking, |_| Some(blackbox::Answer::deny("NOT-NOW")))
        .await?;
    let outside = run.world.root().join("outside");
    std::fs::create_dir_all(&outside)?;
    let target = outside.join("asked.txt").to_string_lossy().into_owned();
    stub.script([write("w2", &target), Reply::text("done")]);
    let events = run.turn(&asking, "write it").await?;
    assert_eq!(
        prompted_tools(&host),
        ["Write"],
        "the held allow let it run: {:?} {:?}",
        events.tool_results,
        host.frames()
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_the_settings_let_loosen_gets_its_allow_rules() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-loosen", &stub)
        .settings(json!({ "permissions": { "mode": "default", "client_may_loosen": true } }))
        .start()
        .await?;
    let created = create(
        &mut run,
        json!({ "settings": { "permissions": { "allow": ["Write"] } } }),
    )
    .await?;
    let result = created.result.unwrap_or_default();
    assert!(result.get("clamped_options").is_none(), "{result}");
    let allowed = result["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let host = run.watch(&allowed, false).await?;
    let outside = run.world.root().join("outside");
    std::fs::create_dir_all(&outside)?;
    let target = outside.join("allowed.txt");
    stub.script([write("w1", &target.to_string_lossy()), Reply::text("done")]);
    let events = run.turn(&allowed, "write it").await?;
    assert!(prompted_tools(&host).is_empty(), "{:?}", host.frames());
    assert!(!refused(&events, "Write"), "{:?}", events.tool_results);
    assert!(target.exists());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_without_delegation_has_no_agent_tool_and_a_sub_agent_keeps_its_limits(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-agents", &stub)
        .settings(json!({ "permissions": { "mode": "acceptEdits", "allow": ["Agent"] } }))
        .start()
        .await?;
    let alone = session(
        &mut run,
        json!({ "settings": { "agents": { "max_depth": 0 } } }),
    )
    .await?;
    stub.script([Reply::text("hi")]);
    run.turn(&alone, "hello").await?;
    let tools = stub.calls()[0].tool_names();
    assert!(!tools.iter().any(|t| t == "Agent"), "{tools:?}");

    let parent = session(
        &mut run,
        json!({ "settings": {
            "agents": { "max_depth": 1 },
            "permissions": { "deny": ["Write"] },
        } }),
    )
    .await?;
    let _host = run.watch(&parent, true).await?;
    let before = stub.call_count();
    stub.script_when(
        ROOT,
        [
            Reply::calls(
                "a1",
                "Agent",
                json!({ "description": "writer", "prompt": "write the file" }),
            ),
            Reply::text("ROOT-DONE"),
        ],
    );
    stub.script([write("w1", "child.txt"), Reply::text("child done")]);
    let events = run.turn(&parent, &format!("{ROOT} delegate")).await?;
    assert!(events.text.contains("ROOT-DONE"), "{events:?}");
    let calls = stub.calls();
    let root_tools = calls[before].tool_names();
    assert!(root_tools.iter().any(|t| t == "Agent"), "{root_tools:?}");
    let child: Vec<_> = calls[before..]
        .iter()
        .filter(|c| !c.messages_contain(ROOT))
        .collect();
    assert!(!child.is_empty());
    assert!(
        child
            .iter()
            .all(|c| !c.tool_names().iter().any(|t| t == "Agent")),
        "the sub-agent may delegate past max_depth 1"
    );
    assert!(
        child
            .iter()
            .any(|c| c.messages_text().contains("Denied") || c.messages_text().contains("denied")),
        "the sub-agent's write was not refused"
    );
    assert!(run.read_file("child.txt").is_none());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_denies_the_network_fetches_nothing() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    pages.route("/doc", Page::text("LOCAL-PAGE"));
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-network", &stub).start().await?;
    let s = session(
        &mut run,
        json!({ "settings": { "sandbox": { "network": { "mode": "deny_all" } } } }),
    )
    .await?;
    let _host = run.watch(&s, true).await?;
    stub.script([
        Reply::calls("f1", "WebFetch", json!({ "url": pages.url("/doc") })),
        Reply::text("done"),
    ]);
    let events = run.turn(&s, "fetch").await?;
    let fetched = events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "WebFetch")
        .expect("a WebFetch result");
    assert!(
        fetched.2 && !fetched.1.contains("LOCAL-PAGE"),
        "{fetched:?}"
    );
    assert_eq!(pages.hits("/doc"), 0, "a refused fetch reached the server");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn what_only_a_settings_file_may_set_refuses_the_session() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-withheld", &stub).start().await?;
    let before = session_count(&mut run).await?;
    for (settings, field) in [
        (
            json!({ "providers": [{ "id": "evil", "api_type": "anthropic", "base_url": "https://evil.invalid" }] }),
            "settings.providers",
        ),
        (
            json!({ "hooks": { "PreToolUse": [{ "type": "command", "command": "true" }] } }),
            "settings.hooks",
        ),
        (
            json!({ "mcp": { "servers": { "x": { "command": "true" } } } }),
            "settings.mcp",
        ),
        (json!({ "turnn": {} }), "settings.turnn"),
    ] {
        let created = create(&mut run, json!({ "settings": settings })).await?;
        let error = created.error.expect("refused");
        assert_eq!(error.code, -32602, "{error:?}");
        let invalid = &error.data.unwrap_or_default()["invalid_options"];
        assert_eq!(invalid[0]["field"], field, "{invalid}");
    }
    assert_eq!(
        session_count(&mut run).await?,
        before,
        "a session was created"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sessions_thinking_is_a_setting_of_its_own_and_the_old_names_are_refused(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-thinking", &stub).start().await?;
    let off = session(
        &mut run,
        json!({ "settings": { "model": { "thinking": "off" } } }),
    )
    .await?;
    stub.script([Reply::text("hi")]);
    run.turn(&off, "hello").await?;
    let calls = stub.calls();
    assert_eq!(
        calls[0].body["thinking"]["type"], "disabled",
        "{}",
        calls[0].body
    );

    for (options, field, now) in [
        (
            json!({ "thinking_mode": "off" }),
            "thinking_mode",
            "settings.model.thinking",
        ),
        (
            json!({ "thinking": "off" }),
            "thinking",
            "settings.model.thinking",
        ),
        (
            json!({ "effort": "high" }),
            "effort",
            "settings.model.effort",
        ),
        (
            json!({ "prompt": { "append": "x" } }),
            "prompt",
            "settings.prompt.append / settings.prompt.override",
        ),
        (
            json!({ "turn": { "token_budget": 10 } }),
            "turn",
            "settings.turn",
        ),
        (
            json!({ "permission_mode": "plan" }),
            "permission_mode",
            "settings.permissions.mode",
        ),
        (
            json!({ "permission_rules": [] }),
            "permission_rules",
            "settings.permissions",
        ),
    ] {
        let created = create(&mut run, options).await?;
        let error = created.error.expect("refused");
        assert_eq!(error.code, -32602, "{error:?}");
        let invalid = &error.data.unwrap_or_default()["invalid_options"];
        assert_eq!(invalid[0]["field"], field, "{invalid}");
        assert!(
            invalid[0]["error"]
                .as_str()
                .unwrap_or_default()
                .contains(now),
            "{invalid}"
        );
    }
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_own_settings_hold_after_a_restart() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-restart", &stub)
        .settings(json!({ "permissions": { "mode": "acceptEdits" } }))
        .start()
        .await?;
    let s = session(
        &mut run,
        json!({ "settings": { "permissions": { "deny": ["Write"] } } }),
    )
    .await?;
    stub.script([Reply::text("hello")]);
    run.turn(&s, "hi").await?;

    run.restart().await?;
    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": s }))
        .await?;
    assert!(resumed.error.is_none(), "{resumed:?}");
    let _host = run.watch(&s, true).await?;
    stub.script([write("w1", "after.txt"), Reply::text("done")]);
    let events = run.turn(&s, "write it").await?;
    assert!(refused(&events, "Write"), "{:?}", events.tool_results);
    assert!(run.read_file("after.txt").is_none());
    run.finish().await;
    Ok(())
}

fn effort_of(call: &blackbox::SeenRequest) -> &Value {
    &call.body["output_config"]["effort"]
}

/// What a session's own settings say about its reasoning holds for the
/// sub-agents it starts, not only for its own conversation.
#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_own_effort_and_thinking_reach_its_sub_agents() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-effort-agents", &stub)
        .settings(json!({
            "model": { "effort": "low" },
            "permissions": { "mode": "acceptEdits", "allow": ["Agent"] },
        }))
        .start()
        .await?;
    let parent = session(
        &mut run,
        json!({ "settings": { "model": { "effort": "high", "thinking": "off" } } }),
    )
    .await?;
    let _host = run.watch(&parent, true).await?;
    let before = stub.call_count();
    stub.script_when(
        ROOT,
        [
            Reply::calls(
                "a1",
                "Agent",
                json!({ "description": "helper", "prompt": "say done" }),
            ),
            Reply::text("ROOT-DONE"),
        ],
    );
    stub.script([Reply::text("child done")]);
    let events = run.turn(&parent, &format!("{ROOT} delegate")).await?;
    assert!(events.text.contains("ROOT-DONE"), "{events:?}");
    let calls = stub.calls();
    let child: Vec<_> = calls[before..]
        .iter()
        .filter(|c| c.messages_contain("say done") && !c.messages_contain(ROOT))
        .collect();
    assert!(!child.is_empty(), "the sub-agent made no call");
    let root = calls[before..].iter().filter(|c| c.messages_contain(ROOT));
    for call in child.into_iter().chain(root) {
        assert_eq!(effort_of(call), "high", "{}", call.body);
        assert_eq!(call.body["thinking"]["type"], "disabled", "{}", call.body);
    }
    run.finish().await;
    Ok(())
}

/// `session.setModel`'s effort and `session.setPrompt` write the session's
/// own settings: in force from the next turn, reported as the session's, and
/// still in force after the daemon restarts.
#[tokio::test(flavor = "multi_thread")]
async fn what_set_model_and_set_prompt_change_holds_after_a_restart() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-set-restart", &stub)
        .settings(json!({ "model": { "effort": "low" } }))
        .start()
        .await?;
    let s = session(&mut run, json!({})).await?;
    let set = run
        .client
        .call(
            "session.setModel",
            json!({ "session_id": s, "effort": "max" }),
        )
        .await?;
    let set = set.result.unwrap_or_default();
    assert_eq!(set["effort"], "max", "{set}");
    assert!(set.get("clamped_options").is_none(), "{set}");
    let set = run
        .client
        .call(
            "session.setPrompt",
            json!({ "session_id": s, "append": "SET-PROMPT-MARK" }),
        )
        .await?;
    let set = set.result.unwrap_or_default();
    assert_eq!(set["append"], "SET-PROMPT-MARK", "{set}");
    assert!(set.get("clamped_options").is_none(), "{set}");

    stub.script([Reply::text("one")]);
    run.turn(&s, "first").await?;
    let first = stub.calls().pop().expect("a call");
    assert_eq!(effort_of(&first), "max", "{}", first.body);
    assert!(first.system_text().contains("SET-PROMPT-MARK"));

    run.restart().await?;
    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": s }))
        .await?;
    assert!(resumed.error.is_none(), "{resumed:?}");
    let got = run.client.session_get(&s).await?.result.unwrap_or_default();
    assert_eq!(got["reasoning"]["effort"], "max", "{got}");
    assert_eq!(got["reasoning"]["effort_source"], "session", "{got}");
    stub.script([Reply::text("two")]);
    run.turn(&s, "second").await?;
    let second = stub.calls().pop().expect("a call");
    assert_eq!(effort_of(&second), "max", "{}", second.body);
    assert!(second.system_text().contains("SET-PROMPT-MARK"));

    let back = run
        .client
        .call(
            "session.setModel",
            json!({ "session_id": s, "effort": null }),
        )
        .await?;
    assert_eq!(back.result.unwrap_or_default()["effort"], Value::Null);
    stub.script([Reply::text("three")]);
    run.turn(&s, "third").await?;
    let third = stub.calls().pop().expect("a call");
    assert_eq!(
        effort_of(&third),
        "low",
        "`null` gives the settings' effort back"
    );
    run.finish().await;
    Ok(())
}

fn log_of(root: &std::path::Path, session: &str) -> Option<std::path::PathBuf> {
    let name = format!("{session}.jsonl");
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = log_of(&path, session) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == name.as_str()) {
            return Some(path);
        }
    }
    None
}

/// A session a 0.3 engine created with `permission_rules` and `turn` has its
/// options in that shape in its log; brought back, it is held to them still.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_recorded_in_the_0_3_shape_keeps_its_limits() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-0-3", &stub)
        .settings(json!({ "permissions": { "mode": "acceptEdits" } }))
        .start()
        .await?;
    let s = session(&mut run, json!({})).await?;
    stub.script([Reply::text("hello")]);
    run.turn(&s, "hi").await?;
    run.restart().await?;

    let log = log_of(run.world.root(), &s).expect("the session's log");
    let recorded = history::entry::EnvelopedEntry::new(
        base::session::SessionId::parse(&s)?,
        history::entry::LogEntry::Extension {
            ns: "agix.session".into(),
            event: "spec".into(),
            payload: json!({ "options": {
                "auto_name": false,
                "permission_rules": [
                    { "source": "session", "behavior": "deny", "tool_name": "Write" }
                ],
                "turn": { "max_model_calls": 2 },
            } }),
        },
    );
    let mut line = serde_json::to_string(&recorded)?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)?
        .write_all(line.as_bytes())?;

    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": s }))
        .await?;
    assert!(resumed.error.is_none(), "{resumed:?}");
    let _host = run.watch(&s, true).await?;
    stub.script([
        write("w1", "old.txt"),
        write("w2", "old.txt"),
        Reply::text("done"),
    ]);
    let events = run.turn(&s, "write it").await?;
    assert!(refused(&events, "Write"), "{:?}", events.tool_results);
    assert!(run.read_file("old.txt").is_none());
    assert_eq!(
        events.turn_complete_event.clone().unwrap_or_default()["stop_reason"],
        "max_turns",
        "{events:?}"
    );
    run.finish().await;
    Ok(())
}

/// A session 0.4.0 created with `options.effort`, `options.prompt` and a
/// thinking mode set since has them beside its settings in its log; brought
/// back, it keeps them as its own settings.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_recorded_by_0_4_0_keeps_its_effort_thinking_and_prompt() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("session-0-4-0", &stub)
        .settings(json!({ "model": { "effort": "low" } }))
        .start()
        .await?;
    let s = session(&mut run, json!({})).await?;
    stub.script([Reply::text("hello")]);
    run.turn(&s, "hi").await?;
    run.restart().await?;

    let log = log_of(run.world.root(), &s).expect("the session's log");
    let recorded = history::entry::EnvelopedEntry::new(
        base::session::SessionId::parse(&s)?,
        history::entry::LogEntry::Extension {
            ns: "agix.session".into(),
            event: "spec".into(),
            payload: json!({
                "options": {
                    "auto_name": false,
                    "effort": "high",
                    "prompt": { "append": "RECORDED-PROMPT-MARK", "override": null },
                },
                "thinking_mode": "off",
            }),
        },
    );
    let mut line = serde_json::to_string(&recorded)?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)?
        .write_all(line.as_bytes())?;

    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": s }))
        .await?;
    assert!(resumed.error.is_none(), "{resumed:?}");
    stub.script([Reply::text("again")]);
    run.turn(&s, "again").await?;
    let call = stub.calls().pop().expect("a call");
    assert_eq!(effort_of(&call), "high", "{}", call.body);
    assert_eq!(call.body["thinking"]["type"], "disabled", "{}", call.body);
    assert!(call.system_text().contains("RECORDED-PROMPT-MARK"));
    run.finish().await;
    Ok(())
}
