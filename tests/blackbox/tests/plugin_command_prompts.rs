//! A plugin command's sub-calls put to the host when the session would ask
//! about them: the model's or a person's `RunExt` run in an attended session
//! asks as the model's own call would, with the run it came from on the
//! `prompt` frame. A program's run, a write outside `writes` and an
//! unanswered question still fail. `notes-plugin` from
//! `tests/fixtures/ext_plugin`, seen only through the RPC, the frames and the
//! project directory.

use crate::plugin_commands::{result_of, stubbed, tool_result_text, with_notes};
use blackbox::{Answer, ProviderStub, Reply};
use serde_json::{json, Value};
use test_runner::plugin_fixture::{file_url, package_ext, EXT_PACKAGE};

fn asked_events(watcher: &blackbox::Watcher) -> Vec<Value> {
    watcher
        .asked()
        .iter()
        .map(|a| a.prompt["event"].clone())
        .collect()
}

fn tool_names(asked: &[Value]) -> Vec<Value> {
    asked.iter().map(|e| e["tool_name"].clone()).collect()
}

async fn install_notes(run: &mut blackbox::LiveRun) -> anyhow::Result<()> {
    let (archive, _) = package_ext(run.world.root())?;
    let installed = run
        .client
        .plugin_install_and_enable(json!({
            "name": EXT_PACKAGE, "version": "1.0.0", "download_url": file_url(&archive)
        }))
        .await?;
    anyhow::ensure!(installed.error.is_none(), "install: {:?}", installed.error);
    Ok(())
}

fn asking_for_writes(extra: Value) -> Value {
    let mut permissions = json!({ "mode": "acceptEdits", "ask": ["Write"] });
    if let (Some(into), Some(from)) = (permissions.as_object_mut(), extra.as_object()) {
        into.extend(from.clone());
    }
    json!({ "permissions": permissions })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_an_ask_rule_covers_is_put_to_the_host_with_the_run_it_came_from(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_notes("ext-ask-write", &stub, asking_for_writes(json!({}))).await?;
    stub.script([
        Reply::calls(
            "add",
            "RunExt",
            json!({ "command": "notes add first", "input": { "body": "asked" } }),
        ),
        Reply::text("done"),
    ]);
    let session = run.session().await?;
    let watcher = run.watch_with(&session, Answer::permit_all).await?;
    let events = run.turn(&session, "take a note").await?;
    let asked = asked_events(&watcher);
    drop(watcher);

    assert_eq!(tool_names(&asked), [json!("Write")], "{asked:?}");
    let prompt = &asked[0];
    assert_eq!(
        prompt["origin"],
        json!({ "via": "RunExt:notes", "tool_use_id": "add", "command": "notes add first" }),
        "{prompt}"
    );
    assert!(
        prompt["tool_use_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("add.")),
        "{prompt}"
    );
    assert_eq!(prompt["permit_always"], false, "{prompt}");
    let results = result_of(&events, "RunExt");
    assert!(!results[0].2, "{results:?}");
    assert_eq!(run.read_file("notes/first.md").as_deref(), Some("asked"));
    run.finish().await;
    Ok(())
}

/// "Always" is not on offer for a command's sub-call: a host answering it
/// anyway lets that one call through and keeps nothing, so the next run's
/// identical call is asked again and nothing is written down for it.
#[tokio::test(flavor = "multi_thread")]
async fn an_always_answer_to_a_sub_call_allows_it_once_and_keeps_nothing() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_notes("ext-ask-always", &stub, json!({})).await?;
    stub.script([
        Reply::calls("s1", "RunExt", json!({ "command": "notes stamp" })),
        Reply::calls("s2", "RunExt", json!({ "command": "notes stamp" })),
        Reply::text("done"),
    ]);
    let session = run.session().await?;
    let watcher = run
        .watch_with(&session, |_| Some(Answer::permit_always("local")))
        .await?;
    run.turn(&session, "stamp twice").await?;
    let asked = asked_events(&watcher);
    drop(watcher);
    assert_eq!(
        tool_names(&asked),
        [json!("RunExt"), json!("Bash"), json!("Bash")],
        "the run itself is kept, its sub-call is not: {asked:?}"
    );
    assert_eq!(asked[0]["permit_always"], true, "{asked:?}");
    assert_eq!(asked[1]["permit_always"], false, "{asked:?}");
    assert_eq!(asked[2]["permit_always"], false, "{asked:?}");
    let local = run
        .read_file(".agix/settings.local.json")
        .unwrap_or_default();
    assert!(!local.contains("Bash"), "{local}");
    run.finish().await;
    Ok(())
}

/// A refusal is the sub-call's error, and the run's other calls are not put
/// to the person again: a command cannot ask its way past a no.
#[tokio::test(flavor = "multi_thread")]
async fn after_a_refusal_the_rest_of_the_run_is_not_asked_about() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_notes("ext-ask-refused", &stub, asking_for_writes(json!({}))).await?;
    stub.script([
        Reply::calls("pr", "RunExt", json!({ "command": "notes pair one two" })),
        Reply::calls("again", "RunExt", json!({ "command": "notes add three" })),
        Reply::text("done"),
    ]);
    let session = run.session().await?;
    let watcher = run
        .watch_with(&session, |_| Some(Answer::deny("not this one")))
        .await?;
    run.turn(&session, "two notes").await?;
    let asked = asked_events(&watcher);
    drop(watcher);
    assert_eq!(
        tool_names(&asked),
        [json!("Write"), json!("Write")],
        "one question for the refused run, one for the next: {asked:?}"
    );
    let (pair, _) = tool_result_text(&stub, "pr").expect("pair ran");
    assert!(
        pair.contains("one: ") && pair.contains("not this one"),
        "{pair}"
    );
    assert!(
        pair.contains("two: ") && pair.contains("already refused"),
        "{pair}"
    );
    let (again, failed) = tool_result_text(&stub, "again").expect("add ran");
    assert!(failed && again.contains("not this one"), "{again}");
    for note in ["one", "two", "three"] {
        assert!(
            run.read_file(&format!("notes/{note}.md")).is_none(),
            "{note}"
        );
    }
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_question_fails_the_command_when_it_times_out() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("ext-ask-timeout", &stub)
        .settings(asking_for_writes(json!({})))
        .options(|o| o.prompt_timeout(std::time::Duration::from_secs(1)))
        .start()
        .await?;
    install_notes(&mut run).await?;
    stub.script([
        Reply::calls("late", "RunExt", json!({ "command": "notes add late" })),
        Reply::text("done"),
    ]);
    let session = run.session().await?;
    let watcher = run.watch_with(&session, |_| None).await?;
    run.turn(&session, "a note nobody approves").await?;
    assert_eq!(tool_names(&asked_events(&watcher)), [json!("Write")]);
    drop(watcher);
    let (text, failed) = tool_result_text(&stub, "late").expect("the command ran");
    assert!(failed && text.contains("no answer"), "{text}");
    assert!(run.read_file("notes/late.md").is_none());
    run.finish_allowing_refusals().await;
    Ok(())
}

/// What the run's scope refuses is refused before anyone is asked; and a
/// program's run still has nobody to ask.
#[tokio::test(flavor = "multi_thread")]
async fn scope_refusals_and_a_programs_run_are_not_asked_about() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_notes(
        "ext-ask-unchanged",
        &stub,
        asking_for_writes(json!({ "allow": ["RunCode"], "deny": ["Write(notes/blocked.md)"] })),
    )
    .await?;
    let code = r#"await tools.RunExt({ command: "notes add coded" }); return "ok";"#;
    stub.script([
        Reply::calls("out", "RunExt", json!({ "command": "notes escape" })),
        Reply::calls("deny", "RunExt", json!({ "command": "notes add blocked" })),
        Reply::calls("ro", "RunExt", json!({ "command": "notes peek" })),
        Reply::calls("prog", "RunCode", json!({ "code": code })),
        Reply::text("done"),
    ]);
    let session = run.session().await?;
    let watcher = run.watch_with(&session, Answer::permit_all).await?;
    run.turn(&session, "try them").await?;
    let asked = asked_events(&watcher);
    drop(watcher);
    assert!(asked.is_empty(), "{asked:?}");

    let (escape, failed) = tool_result_text(&stub, "out").expect("escape ran");
    assert!(failed && escape.contains("outside that"), "{escape}");
    assert!(run.read_file("outside.txt").is_none());
    let (denied, failed) = tool_result_text(&stub, "deny").expect("add ran");
    assert!(failed && denied.contains("denied by rule"), "{denied}");
    assert!(run.read_file("notes/blocked.md").is_none());
    let (peek, failed) = tool_result_text(&stub, "ro").expect("peek ran");
    assert!(failed && peek.contains("read-only"), "{peek}");
    let (program, _) = tool_result_text(&stub, "prog").expect("the program ran");
    assert!(program.contains("cannot ask"), "{program}");
    assert!(run.read_file("notes/coded.md").is_none());
    run.finish().await;
    Ok(())
}

/// Nobody attends the session: the write the ask rule covers fails, unasked.
#[tokio::test(flavor = "multi_thread")]
async fn an_unattended_session_still_refuses_unasked() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_notes("ext-ask-unattended", &stub, asking_for_writes(json!({}))).await?;
    stub.script([
        Reply::calls("u", "RunExt", json!({ "command": "notes add quiet" })),
        Reply::text("done"),
    ]);
    let session = run
        .session_with(json!({ "options": { "attended": false } }))
        .await?;
    let watcher = run.watch_with(&session, Answer::permit_all).await?;
    run.turn(&session, "a quiet note").await?;
    let asked = asked_events(&watcher);
    drop(watcher);
    assert!(asked.is_empty(), "{asked:?}");
    let (text, failed) = tool_result_text(&stub, "u").expect("add ran");
    assert!(failed && text.contains("nobody is attending"), "{text}");
    assert!(run.read_file("notes/quiet.md").is_none());
    run.finish_allowing_refusals().await;
    Ok(())
}

/// Typing `/notes add …` approves the run, not what the session asks about
/// inside it.
#[tokio::test(flavor = "multi_thread")]
async fn a_typed_command_asks_about_a_write_an_ask_rule_covers() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_notes("ext-ask-typed", &stub, asking_for_writes(json!({}))).await?;
    let session = run.session().await?;
    let watcher = run.watch_with(&session, Answer::permit_all).await?;
    run.turn(&session, "/notes add typed").await?;
    let asked = asked_events(&watcher);
    drop(watcher);
    assert_eq!(tool_names(&asked), [json!("Write")], "{asked:?}");
    assert_eq!(asked[0]["origin"]["via"], "RunExt:notes", "{asked:?}");
    assert_eq!(run.read_file("notes/typed.md").as_deref(), Some("(empty)"));
    run.finish().await;
    Ok(())
}

/// What the session's mode asks about — a `Bash` that changes something, in
/// `default` — is asked about too, after the run itself.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_s_shell_call_is_asked_about_as_the_model_s_would_be() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = with_notes("ext-ask-bash", &stub, json!({})).await?;
    stub.script([
        Reply::calls("st", "RunExt", json!({ "command": "notes stamp" })),
        Reply::text("done"),
    ]);
    let session = run.session().await?;
    let watcher = run.watch_with(&session, Answer::permit_all).await?;
    run.turn(&session, "stamp it").await?;
    let asked = asked_events(&watcher);
    drop(watcher);
    assert_eq!(
        tool_names(&asked),
        [json!("RunExt"), json!("Bash")],
        "{asked:?}"
    );
    assert_eq!(asked[1]["origin"]["command"], "notes stamp", "{asked:?}");
    let (text, failed) = tool_result_text(&stub, "st").expect("stamp ran");
    assert!(!failed, "{text}");
    assert!(run.read_file("notes/stamped.md").is_some());
    run.finish().await;
    Ok(())
}

/// A sub-agent's command asks through the parent's host, as the sub-agent's
/// own calls do, and says which run it came from.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_in_a_sub_agent_asks_through_the_parent() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("ext-ask-child", &stub)
        .settings(asking_for_writes(json!({ "allow": ["Agent"] })))
        .start()
        .await?;
    install_notes(&mut run).await?;
    let session = run.session().await?;
    stub.script_when(
        "PARENT-MARK",
        [
            Reply::calls(
                "p1",
                "Agent",
                json!({ "prompt": "CHILD-MARK note", "description": "note" }),
            ),
            Reply::text("parent done"),
        ],
    );
    stub.script_when(
        "CHILD-MARK",
        [
            Reply::calls("c1", "RunExt", json!({ "command": "notes add kid" })),
            Reply::text("child done"),
        ],
    );
    let watcher = run.watch_with(&session, Answer::permit_all).await?;
    run.turn(&session, "PARENT-MARK delegate").await?;
    let asked = asked_events(&watcher);
    drop(watcher);
    assert_eq!(tool_names(&asked), [json!("Write")], "{asked:?}");
    assert_eq!(asked[0]["origin"]["via"], "RunExt:notes", "{asked:?}");
    assert!(run.read_file("notes/kid.md").is_some());
    run.finish().await;
    Ok(())
}
