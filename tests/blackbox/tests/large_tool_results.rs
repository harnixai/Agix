//! A tool result too long for the conversation is saved to a file in the
//! session's scratchpad, and the model is handed its head, its tail and the
//! path: the request after a long `Bash` run, reading the rest back with
//! `Read`, a server's text staying fenced through the file, `Grep` not finding
//! what an earlier call saved, an old result cleared with its path kept, a
//! sub-agent's output going under the root session, `session.delete`, what is
//! never saved, and `WebFetch` read in windows.
//!
//! The project allows the tools by rule; nothing runs under `bypassPermissions`.

use std::path::{Path, PathBuf};

use blackbox::{
    LiveConfig, LiveRun, LiveRunBuilder, McpToy, Page, PageServer, ProviderStub, Purpose, Reply,
    SeenRequest,
};
use serde_json::{json, Value};

const TURN: &str = "\"input_schema\"";

fn stubbed(name: &str, stub: &ProviderStub) -> LiveRunBuilder {
    LiveRun::builder(name)
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(stub),
        ))
        .settings(json!({
            "permission_rules": [
                { "tool": "Bash", "action": "allow" },
                { "tool": "Read", "action": "allow" },
                { "tool": "Grep", "action": "allow" },
                { "tool": "Agent", "action": "allow" }
            ]
        }))
}

fn bash(id: &str, command: &str) -> Reply {
    Reply::calls(id, "Bash", json!({ "command": command }))
}

fn results_root(run: &LiveRun) -> PathBuf {
    run.project.join(".agix/scratchpad/tool-results")
}

fn saved_path(run: &LiveRun, session: &str, tool_use_id: &str) -> PathBuf {
    let clean: String = session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    results_root(run)
        .join(clean)
        .join(format!("{tool_use_id}.txt"))
}

fn last_turn_request(stub: &ProviderStub) -> SeenRequest {
    stub.calls()
        .into_iter()
        .rfind(|c| c.purpose() == Purpose::Turn)
        .expect("a turn reached the model")
}

fn strings_in(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => out.push_str(s),
        Value::Array(a) => a.iter().for_each(|x| strings_in(x, out)),
        Value::Object(o) => o.values().for_each(|x| strings_in(x, out)),
        _ => {}
    }
}

fn result_in(request: &SeenRequest, tool_use_id: &str) -> String {
    fn find(v: &Value, id: &str, out: &mut Option<String>) {
        match v {
            Value::Object(o) => {
                if o.get("tool_use_id").and_then(Value::as_str) == Some(id) {
                    let mut text = String::new();
                    strings_in(o.get("content").unwrap_or(&Value::Null), &mut text);
                    *out = Some(text);
                }
                o.values().for_each(|x| find(x, id, out));
            }
            Value::Array(a) => a.iter().for_each(|x| find(x, id, out)),
            _ => {}
        }
    }
    let mut found = None;
    find(&request.body["messages"], tool_use_id, &mut found);
    found.unwrap_or_else(|| panic!("no result for {tool_use_id}"))
}

fn path_in(marker_line: &str) -> PathBuf {
    let rest = marker_line
        .split_once("Saved in full to ")
        .expect("a saved-to marker")
        .1;
    PathBuf::from(
        rest.split_once(" \u{2014} read the rest")
            .expect("the marker's end")
            .0,
    )
}

fn result_of<'a>(events: &'a rpc_client::TurnEvents, tool: &str) -> &'a (String, String, bool) {
    events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == tool)
        .unwrap_or_else(|| panic!("no {tool} result in {:?}", events.tool_results))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_bash_output_is_saved_and_the_model_sees_its_head_tail_and_path(
) -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("spill-bash", &stub).start().await?;
    let session = run.session().await?;
    stub.script_when(TURN, [bash("b1", "seq 1 50000"), Reply::text("done")]);
    run.turn(&session, "count").await?;

    let shown = result_in(&last_turn_request(&stub), "b1");
    assert!(
        shown.starts_with("[Output too long for one result: "),
        "{shown}"
    );
    assert!(
        shown.contains("1\n2\n") && shown.contains("50000"),
        "{shown}"
    );
    assert!(shown.chars().count() < 4_000, "{}", shown.chars().count());

    let path = path_in(shown.lines().next().unwrap());
    assert_eq!(path, saved_path(&run, &session, "b1"));
    let saved = std::fs::read_to_string(&path)?;
    assert_eq!(saved.lines().count(), 50_000, "the whole output is there");
    assert!(saved.starts_with("1\n2\n") && saved.trim_end().ends_with("50000"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_saved_output_reads_back_with_read_offsets() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("spill-read", &stub).start().await?;
    let session = run.session().await?;
    let path = saved_path(&run, &session, "b1");
    stub.script_when(
        TURN,
        [
            bash("b1", "seq 1 50000"),
            Reply::calls(
                "r1",
                "Read",
                json!({ "file_path": path.to_string_lossy(), "offset": 40000, "limit": 5 }),
            ),
            Reply::text("done"),
        ],
    );
    let events = run.turn(&session, "count, then read the middle").await?;
    let (_, text, failed) = result_of(&events, "Read");
    assert!(
        !failed && text.contains("40000") && text.contains("40004"),
        "{text}"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saved_mcp_result_read_back_is_still_fenced() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("spill-fence", &stub)
        .settings(json!({
            "permission_rules": [{ "tool": "Read", "action": "allow" }],
            "mcp_servers": { "toy": McpToy::stdio_config(&[])? }
        }))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    let path = saved_path(&run, &session, "m1");
    stub.script_when(
        TURN,
        [
            Reply::calls("m1", "mcp__toy__big", json!({ "bytes": 40_000 })),
            Reply::calls(
                "r1",
                "Read",
                json!({ "file_path": path.to_string_lossy(), "limit": 3 }),
            ),
            Reply::text("done"),
        ],
    );
    let events = run
        .turn(&session, "ask the server, then read it back")
        .await?;
    let shown = result_in(&last_turn_request(&stub), "m1");
    assert!(
        shown.starts_with("[Output too long for one result: "),
        "{shown}"
    );
    assert!(
        shown.contains("<external-content"),
        "the preview keeps its fence"
    );
    let (_, read, failed) = result_of(&events, "Read");
    assert!(!failed && read.starts_with("<external-content"), "{read}");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn grep_over_the_project_does_not_find_saved_outputs() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("spill-grep", &stub).start().await?;
    let session = run.session().await?;
    stub.script_when(
        TURN,
        [
            bash("b1", "seq 1 50000"),
            Reply::calls("g1", "Grep", json!({ "pattern": "^31337$" })),
            Reply::text("done"),
        ],
    );
    let events = run.turn(&session, "count, then search").await?;
    assert!(saved_path(&run, &session, "b1").exists());
    let (_, found, _) = result_of(&events, "Grep");
    assert!(
        !found.contains("b1.txt") && !found.contains("tool-results"),
        "{found}"
    );
    assert!(
        std::fs::read_to_string(
            saved_path(&run, &session, "b1")
                .parent()
                .unwrap()
                .join(".ignore")
        )?
        .trim()
            == "*"
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cleared_old_result_still_names_its_saved_file() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("spill-cleared", &stub).start().await?;
    let session = run.session().await?;
    let line = "x".repeat(1_990);
    let mut replies = vec![bash("b1", "seq 1 50000")];
    for i in 0..11 {
        let file = run.write_file(&format!("big{i}.txt"), &format!("{line}\n").repeat(24))?;
        replies.push(Reply::calls(
            &format!("r{i}"),
            "Read",
            json!({ "file_path": file.to_string_lossy() }),
        ));
    }
    replies.push(Reply::text("done"));
    stub.script_when(TURN, replies);
    run.turn(&session, "count, then read a lot").await?;

    let path = saved_path(&run, &session, "b1");
    let shown = result_in(&last_turn_request(&stub), "b1");
    assert_eq!(
        shown,
        format!(
            "[Old tool result content cleared; full output still at {}]",
            path.display()
        )
    );
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agents_long_output_is_saved_under_the_root_session() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("spill-agent", &stub).start().await?;
    let session = run.session().await?;
    stub.script_when(
        TURN,
        [
            Reply::calls(
                "a1",
                "Agent",
                json!({ "prompt": "count to fifty thousand", "description": "count" }),
            ),
            bash("s1", "seq 1 50000"),
            Reply::text("counted"),
            Reply::text("done"),
        ],
    );
    run.turn(&session, "delegate the counting").await?;
    assert!(saved_path(&run, &session, "s1").exists());
    let dirs = std::fs::read_dir(results_root(&run))?.count();
    assert_eq!(dirs, 1, "one directory, the root session's");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_session_deletes_its_saved_outputs() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("spill-delete", &stub).start().await?;
    let session = run.session().await?;
    stub.script_when(TURN, [bash("b1", "seq 1 50000"), Reply::text("done")]);
    run.turn(&session, "count").await?;
    let path = saved_path(&run, &session, "b1");
    assert!(path.exists());

    let response = run
        .client
        .call("session.delete", json!({ "session_id": session }))
        .await?;
    assert!(response.error.is_none(), "{response:?}");
    assert!(!path.parent().unwrap().exists());
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_short_output_and_a_read_are_never_saved() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("spill-short", &stub).start().await?;
    let session = run.session().await?;
    let long_file = run.write_file("long.txt", &format!("{}\n", "y".repeat(1_500)).repeat(40))?;
    stub.script_when(
        TURN,
        [
            bash("b1", "head -c 29000 /dev/zero | tr '\\0' x"),
            Reply::calls(
                "r1",
                "Read",
                json!({ "file_path": long_file.to_string_lossy() }),
            ),
            Reply::text("done"),
        ],
    );
    let events = run.turn(&session, "print a lot").await?;
    let (_, printed, _) = result_of(&events, "Bash");
    assert!(printed.contains(&"x".repeat(28_000)), "kept whole");
    assert!(!results_root(&run).exists(), "nothing was saved");
    run.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn webfetch_reads_a_long_page_in_windows() -> anyhow::Result<()> {
    let pages = PageServer::start().await?;
    pages.route("/long", Page::huge(120_000, &[(60_000, "MID-MARK")]));
    let stub = ProviderStub::start().await?;
    let mut run = stubbed("spill-fetch", &stub)
        .settings(json!({
            "sandbox": PageServer::reachable(),
            "permission_rules": [{ "tool": "WebFetch", "action": "allow" }]
        }))
        .start()
        .await?;
    let session = run.session().await?;
    let fetch = |id: &str, extra: Value| {
        let mut input = json!({ "url": pages.url("/long") });
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        Reply::calls(id, "WebFetch", input)
    };
    stub.script_when(TURN, [fetch("f1", json!({})), Reply::text("one")]);
    let first = run.turn(&session, "fetch it").await?;
    let (_, text, failed) = result_of(&first, "WebFetch");
    assert!(!failed && !text.contains("MID-MARK"), "{text}");
    assert!(text.contains("of 120000]"), "{text}");
    let next: usize = text
        .rsplit("offset=")
        .next()
        .and_then(|t| t.split(']').next())
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("no continuation offset in {text}"));
    assert!((49_000..=50_000).contains(&next), "{next}");

    stub.script_when(
        TURN,
        [fetch("f2", json!({ "offset": next })), Reply::text("two")],
    );
    let second = run.turn(&session, "go on").await?;
    let (_, text, failed) = result_of(&second, "WebFetch");
    assert!(!failed && text.contains("MID-MARK"), "{text}");
    assert!(
        text.contains(&format!("[characters {next}\u{2013}")),
        "{text}"
    );
    assert_eq!(pages.hits("/long"), 2, "reading on fetches again");
    run.finish().await;
    Ok(())
}
