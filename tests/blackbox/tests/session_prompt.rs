//! A session's own system prompt, `prompt.*` of its own settings: given at
//! creation, changed mid-session, kept across a rebuild, and never written
//! into a settings file; `prompt.append` as one text or as a list of named
//! blocks, each reported on its own.

use blackbox::{Daemon, DaemonOptions, ProviderStub, Reply, World};
use serde_json::{json, Value};

#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_prompt_is_its_own_and_changes_from_its_next_turn() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("prompted")?;
    world.write_project_settings(
        "prompted",
        &json!({ "memory": { "enabled": false }, "prompt": { "append": "FROM-SETTINGS" } }),
    )?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let root = project.to_string_lossy().to_string();

    let session = client
        .session_create(json!({
            "project_root": root,
            "options": { "settings": { "prompt": { "append": "AT-CREATION" } } }
        }))
        .await?;
    let other = client
        .session_create(json!({ "project_root": root }))
        .await?;

    provider.script([Reply::text("one")]);
    client
        .session_run_turn(&session, "first", "t1", None)
        .await?;
    let first = provider.calls()[0].system_text();
    assert!(first.contains("AT-CREATION"), "{first}");
    assert!(
        !first.contains("FROM-SETTINGS"),
        "the session's append replaces the file's"
    );

    provider.script([Reply::text("untouched")]);
    client.session_run_turn(&other, "other", "o1", None).await?;
    assert!(
        provider.calls()[1].system_text().contains("FROM-SETTINGS"),
        "another session must keep the settings' prompt"
    );

    let set = client
        .call(
            "session.setPrompt",
            json!({ "session_id": session, "override": "ONLY-THIS" }),
        )
        .await?;
    let result = set.result.clone().unwrap_or_default();
    assert_eq!(result["override"], json!("ONLY-THIS"), "{set:?}");
    assert_eq!(result["append"], json!("AT-CREATION"), "{set:?}");

    provider.script([Reply::text("two")]);
    client
        .session_run_turn(&session, "second", "t2", None)
        .await?;
    assert!(provider.calls()[2].system_text().starts_with("ONLY-THIS"));

    client
        .call(
            "session.setPrompt",
            json!({ "session_id": session, "append": null, "override": null }),
        )
        .await?;
    client
        .call("session.close", json!({ "session_id": session }))
        .await?;
    client
        .call("session.resume", json!({ "session_id": session }))
        .await?;
    provider.script([Reply::text("three")]);
    client
        .session_run_turn(&session, "third", "t3", None)
        .await?;
    let back = provider.calls()[3].system_text();
    assert!(
        back.contains("FROM-SETTINGS"),
        "clearing gives the settings' prompt back"
    );
    assert!(
        !back.contains("AT-CREATION"),
        "a cleared field stays cleared after a resume"
    );

    let settings_file = project.join(".agix").join("settings.json");
    let on_disk = std::fs::read_to_string(&settings_file)?;
    assert!(
        !on_disk.contains("ONLY-THIS"),
        "the settings file was rewritten"
    );

    let bad = client
        .call("session.setPrompt", json!({ "session_id": session }))
        .await?;
    assert_eq!(bad.error.map(|e| e.code), Some(-32602));

    daemon.stop().await;
    Ok(())
}

/// The managed layer's `prompt.append` stays in the prompt whatever replaced
/// the base prompt below it: the project's `prompt.override`, then the
/// session's own from `session.setPrompt`.
#[tokio::test(flavor = "multi_thread")]
async fn the_managed_append_stays_under_any_lower_override() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("managed-append")?;
    world.trust_project(&project)?;
    world.write_project_settings(
        "managed-append",
        &json!({ "memory": { "enabled": false }, "prompt": { "override": "PROJECT-OVERRIDE" } }),
    )?;
    let managed = tempfile::tempdir()?;
    let managed_file = managed.path().join("managed-settings.json");
    std::fs::write(
        &managed_file,
        json!({ "prompt": { "append": "MANAGED-APPEND" } }).to_string(),
    )?;
    let provider = ProviderStub::start().await?;
    let options = DaemonOptions::new(project.clone())
        .mode(blackbox::Mode::Spawned)
        .arg("--managed-settings")
        .arg(managed_file.to_string_lossy().to_string());
    let daemon = Daemon::start(&world, &provider, options).await?;
    let mut client = daemon.connect().await?;
    let session = client
        .session_create(json!({
            "project_root": project.to_string_lossy(),
            "options": { "auto_name": false }
        }))
        .await?;

    provider.script([Reply::text("one")]);
    client
        .session_run_turn(&session, "first", "t1", None)
        .await?;
    let first = provider.calls()[0].system_text();
    assert!(first.starts_with("PROJECT-OVERRIDE"), "{first}");
    assert!(first.contains("MANAGED-APPEND"), "{first}");

    let set = client
        .call(
            "session.setPrompt",
            json!({ "session_id": session, "override": "SESSION-OVERRIDE" }),
        )
        .await?;
    assert!(set.error.is_none(), "{set:?}");
    provider.script([Reply::text("two")]);
    client
        .session_run_turn(&session, "second", "t2", None)
        .await?;
    let second = provider.calls()[1].system_text();
    assert!(second.starts_with("SESSION-OVERRIDE"), "{second}");
    assert!(second.contains("MANAGED-APPEND"), "{second}");

    daemon.stop().await;
    Ok(())
}

fn host_blocks(report: &Value) -> Vec<(String, Value, u64, String)> {
    report["system"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["origin"].get("host").is_some())
        .map(|b| {
            (
                b["name"].as_str().unwrap_or_default().to_string(),
                b["origin"].clone(),
                b["tokens"].as_u64().unwrap_or_default(),
                b["text"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn sent_system(provider: &ProviderStub, call: usize) -> Vec<String> {
    provider.calls()[call].body["system"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["text"].as_str().map(str::to_string))
        .collect()
}

async fn inspect(client: &mut rpc_client::DaemonRpcClient, session: &str) -> anyhow::Result<Value> {
    let report = client
        .call("session.inspectContext", json!({ "session_id": session }))
        .await?;
    report
        .result
        .ok_or_else(|| anyhow::anyhow!("inspectContext refused: {:?}", report.error))
}

/// `prompt.append` given as a list is one block per entry, each with the
/// host's name, its own origin and its own token count, in the list's order;
/// a session's list replaces the settings file's whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_listed_append_is_one_block_per_entry_each_named_and_counted() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("listed")?;
    world.write_project_settings(
        "listed",
        &json!({ "memory": { "enabled": false },
                 "prompt": { "append": [ { "name": "project", "text": "FROM-THE-FILE" } ] } }),
    )?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let root = project.to_string_lossy().to_string();
    let beta = "BETA-BLOCK and a good deal more text than the first one carries ".repeat(8);

    let session = client
        .session_create(json!({
            "project_root": root,
            "options": { "auto_name": false, "settings": { "prompt": { "append": [
                { "name": "harnix:ext", "text": "ALPHA-BLOCK" },
                { "name": "user", "text": beta },
            ] } } }
        }))
        .await?;
    let report = inspect(&mut client, &session).await?;
    let blocks = host_blocks(&report);
    let named: Vec<(&str, &Value)> = blocks.iter().map(|(n, o, ..)| (n.as_str(), o)).collect();
    assert_eq!(
        named,
        [
            ("host.harnix:ext", &json!({ "host": "harnix:ext" })),
            ("host.user", &json!({ "host": "user" })),
        ],
        "{report}"
    );
    assert_eq!(blocks[0].3, "ALPHA-BLOCK");
    assert!(blocks[0].2 > 0 && blocks[1].2 > blocks[0].2, "{report}");
    let names: Vec<&str> = report["system"]
        .as_array()
        .expect("system")
        .iter()
        .filter_map(|b| b["name"].as_str())
        .collect();
    let at = names.iter().position(|n| *n == "host.harnix:ext").unwrap();
    assert_eq!(names[at + 1], "host.user", "{names:?}");
    assert!(
        !report.to_string().contains("FROM-THE-FILE"),
        "the session's list replaces the file's whole: {report}"
    );

    provider.script([Reply::text("one")]);
    client
        .session_run_turn(&session, "first", "t1", None)
        .await?;
    let sent = sent_system(&provider, 0);
    let alpha = sent.iter().position(|t| t == "ALPHA-BLOCK").expect("sent");
    assert_eq!(sent.get(alpha + 1), Some(&beta), "{sent:?}");

    let plain = client
        .session_create(json!({ "project_root": root, "options": { "auto_name": false } }))
        .await?;
    let report = inspect(&mut client, &plain).await?;
    let file: Vec<(String, Value, String)> = host_blocks(&report)
        .into_iter()
        .map(|(n, o, _, t)| (n, o, t))
        .collect();
    assert_eq!(
        file,
        [(
            "host.project".to_string(),
            json!({ "host": "project" }),
            "FROM-THE-FILE".to_string()
        )],
        "{report}"
    );

    daemon.stop().await;
    Ok(())
}

/// A text append stays one `config.prompt_append` block; `session.setPrompt`
/// turns it into a list from the next turn on; a list that does not hold
/// together is refused at creation and by `setPrompt`, leaving the prompt as
/// it was.
#[tokio::test(flavor = "multi_thread")]
async fn set_prompt_takes_a_list_and_a_list_that_does_not_hold_is_refused() -> anyhow::Result<()> {
    let world = World::new()?;
    let project = world.project("relisted")?;
    world.write_project_settings("relisted", &json!({ "memory": { "enabled": false } }))?;
    let provider = ProviderStub::start().await?;
    let daemon = Daemon::start(&world, &provider, DaemonOptions::new(project.clone())).await?;
    let mut client = daemon.connect().await?;
    let root = project.to_string_lossy().to_string();

    for (append, says) in [
        (
            json!([{ "name": "a", "text": "x" }, { "name": "a", "text": "y" }]),
            "given twice",
        ),
        (json!([{ "name": "a b", "text": "x" }]), "letters, digits"),
        (json!([{ "name": "", "text": "x" }]), "name is empty"),
        (
            json!([{ "name": "a", "text": "" }]),
            "text of \"a\" is empty",
        ),
    ] {
        let created = client
            .call(
                "session.create",
                json!({ "project_root": root,
                        "options": { "settings": { "prompt": { "append": append } } } }),
            )
            .await?;
        let error = created.error.expect("refused");
        assert_eq!(error.code, -32602, "{error:?}");
        let invalid = &error.data.unwrap_or_default()["invalid_options"];
        assert_eq!(invalid[0]["field"], "settings.prompt.append", "{invalid}");
        assert!(
            invalid[0]["error"]
                .as_str()
                .unwrap_or_default()
                .contains(says),
            "{append}: {invalid}"
        );
    }

    let session = client
        .session_create(json!({
            "project_root": root,
            "options": { "auto_name": false,
                         "settings": { "prompt": { "append": "PLAIN-APPEND" } } }
        }))
        .await?;
    let report = inspect(&mut client, &session).await?;
    let plain: Vec<(Value, Value)> = report["system"]
        .as_array()
        .expect("system")
        .iter()
        .filter(|b| b["text"] == "PLAIN-APPEND")
        .map(|b| (b["name"].clone(), b["origin"].clone()))
        .collect();
    assert_eq!(
        plain,
        [(json!("config.prompt_append"), json!("config"))],
        "{report}"
    );

    let list = json!([{ "name": "later", "text": "LATER-BLOCK" },
                      { "name": "last", "text": "LAST-BLOCK" }]);
    let set = client
        .call(
            "session.setPrompt",
            json!({ "session_id": session, "append": list }),
        )
        .await?;
    assert_eq!(
        set.result.clone().unwrap_or_default()["append"],
        list,
        "{set:?}"
    );

    let refused = client
        .call(
            "session.setPrompt",
            json!({ "session_id": session,
                    "append": [{ "name": "x", "text": "DUP-1" }, { "name": "x", "text": "DUP-2" }] }),
        )
        .await?;
    assert_eq!(
        refused.error.map(|e| e.code),
        Some(-32602),
        "{:?}",
        refused.result
    );

    provider.script([Reply::text("one")]);
    client
        .session_run_turn(&session, "first", "t1", None)
        .await?;
    let sent = sent_system(&provider, 0);
    let later = sent.iter().position(|t| t == "LATER-BLOCK").expect("sent");
    assert_eq!(
        sent.get(later + 1).map(String::as_str),
        Some("LAST-BLOCK"),
        "{sent:?}"
    );
    let all = sent.join("\n");
    assert!(
        !all.contains("PLAIN-APPEND") && !all.contains("DUP-"),
        "{all}"
    );

    let report = inspect(&mut client, &session).await?;
    let names: Vec<String> = host_blocks(&report).into_iter().map(|b| b.0).collect();
    assert_eq!(names, ["host.later", "host.last"], "{report}");

    daemon.stop().await;
    Ok(())
}
