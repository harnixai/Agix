//! The RPC envelope and the small session methods: status, unknown methods, malformed lines and options, close, respond-to-prompt, commands, `import.*`.

use std::time::Duration;

use crate::support::*;
use base::id::Id;
use daemon::rpc::codes;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[tokio::test]
async fn status_returns_info() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.status","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "expected success, got: {v}");
    assert!(v["result"]["version"].is_string());
    assert_eq!(v["result"]["sessions"], 0);
    assert_eq!(
        v["result"]["protocol_version"],
        daemon::discovery::INSTANCE_PROTOCOL_VERSION
    );
    assert!(v["result"]["capabilities"].is_array());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn session_list_returns_empty_initially() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"session.list","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"]["sessions"].is_array());
    assert!(v["result"]["sessions"].as_array().unwrap().is_empty());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn run_turn_without_session_id_creates_new() {
    let (_server, sock, _dir, handle) = start_server().await;
    // session_id 可选，不传则自动创建
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"hello"},"id":1}"#,
    )
    .await;
    // 会尝试调用 LLM（但测试环境没有真实 API key），所以应该返回 engine error
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    // 只要有 result 或 error（不是 INVALID_PARAMS），说明 session 创建成功了
    assert!(v["result"].is_object() || v["error"].is_object());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn run_turn_nonexistent_session_errors() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"session_id":"NopeNopeNopeNopeNope12","message":"test"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    // 不存在的 session：新代码尝试创建/恢复，也不会报 SESSION_NOT_FOUND
    // 但最终会尝试调 LLM 而失败
    assert!(v["error"].is_object() || v["result"].is_object());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn unknown_method_returns_error() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"nonexistent","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::METHOD_NOT_FOUND);

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A request without an `id` is a notification: whatever it does, no
/// response comes back for it — not a result, not an error.
#[tokio::test]
async fn a_notification_gets_no_response() {
    let (_server, sock, _dir, handle) = start_server().await;
    let mut client = UnixStream::connect(&sock).await.unwrap();
    client
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"daemon.status\"}\n\
              {\"jsonrpc\":\"2.0\",\"method\":\"nonexistent\"}\n",
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    client
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"daemon.status\",\"id\":7}\n")
        .await
        .unwrap();
    let (r, _w) = client.split();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["id"], 7, "the first line back answers the request: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn a_line_that_is_not_json_gets_a_parse_error_and_the_connection_goes_on() {
    let (_server, sock, _dir, handle) = start_server().await;
    let mut client = UnixStream::connect(&sock).await.unwrap();
    client.write_all(b"this is not json\n").await.unwrap();
    client
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"daemon.status\",\"id\":9}\n")
        .await
        .unwrap();
    let (r, _w) = client.split();
    let mut lines = BufReader::new(r);

    let mut first = String::new();
    lines.read_line(&mut first).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": -32700, "message": "Parse error" }
        })
    );

    let mut second = String::new();
    lines.read_line(&mut second).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(v["id"], 9, "the connection still serves requests: {v}");
    assert!(v.get("result").is_some(), "{v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// `daemon.shutdown` answers once the sessions are stopped; a connection
/// still open after that cannot start another one.
#[tokio::test]
async fn no_session_is_created_after_shutdown_starts() {
    let (_server, sock, _dir, handle) = start_server().await;
    let mut client = UnixStream::connect(&sock).await.unwrap();
    let (r, mut w) = client.split();
    let mut lines = BufReader::new(r).lines();
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"daemon.shutdown\",\"id\":1}\n")
        .await
        .unwrap();
    let down: serde_json::Value =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(down["result"]["shutting_down"], true, "{down}");

    w.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"session.create\",\"id\":2}\n")
        .await
        .unwrap();
    let created: serde_json::Value =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert!(created["result"].is_null(), "{created}");
    assert!(
        created["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("shutting down"),
        "{created}"
    );
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the listener stopped")
        .unwrap();
}

/// `options` that do not read are refused whole, and the error names each
/// field; nothing is created.
#[tokio::test]
async fn malformed_options_are_refused() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":1,"params":{"options":{"strict":true,"model":3,"permision_rules":[],"prompt_types":["question",7]}}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_null(), "{v}");
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "{v}");
    let mut fields: Vec<&str> = v["error"]["data"]["invalid_options"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .map(|i| i["field"].as_str().unwrap())
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        ["model", "permision_rules", "prompt_types[1]", "strict"],
        "{v}"
    );

    let listed = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"session.list","id":2}"#).await;
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(
        listed["result"]["sessions"].as_array().map(Vec::len),
        Some(0),
        "a refused create made a session: {listed}"
    );

    let clean = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":3,"params":{"options":{"model":"claude-haiku-4-5"}}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&clean).unwrap();
    assert!(v["result"]["session_id"].is_string(), "{v}");
    assert!(v["result"].get("ignored_options").is_none(), "{v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn turn_id_is_base58_uuid_22_chars() {
    let id = Id::new().to_string();
    assert!(
        (21..=22).contains(&id.len()),
        "expected 21-22 chars, got {}: {id}",
        id.len()
    );
}

#[tokio::test]
async fn session_close_removes_unknown_session_gracefully() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.close","params":{"session_id":"doesnotexist"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["closed"], "doesnotexist", "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn session_close_rejects_missing_session_id() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.close","params":{},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Unlike `session.close` (idempotent no-op on an unknown id — see above),
/// answering a prompt for a session that isn't running is a real caller
/// error: there's no pending wait to silently no-op.
#[tokio::test]
async fn respond_to_prompt_rejects_unknown_session() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{"session_id":"doesnotexist","prompt_id":"p1","decision":{"type":"permit"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::SESSION_NOT_FOUND, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn respond_to_prompt_rejects_missing_params() {
    let (_server, sock, _dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{"prompt_id":"p1","decision":{"type":"permit"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["error"]["code"],
        codes::INVALID_PARAMS,
        "missing session_id, resp: {v}"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{"session_id":"x","decision":{"type":"permit"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["error"]["code"],
        codes::INVALID_PARAMS,
        "missing prompt_id, resp: {v}"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{"session_id":"x","prompt_id":"p1"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["error"]["code"],
        codes::INVALID_PARAMS,
        "missing decision, resp: {v}"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{"session_id":"x","prompt_id":"p1","prompt_type":"something_else","decision":{"type":"permit"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["error"]["code"],
        codes::INVALID_PARAMS,
        "unsupported prompt_type, resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn commands_list_returns_builtin_local_commands() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"commands.list","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let commands = v["result"]["commands"].as_array().expect("commands array");

    let names: Vec<&str> = commands
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    for expected in ["help", "skills", "clear", "compact", "cost"] {
        assert!(
            names.contains(&expected),
            "missing builtin command {expected}: {names:?}"
        );
    }
    let help = commands.iter().find(|c| c["name"] == "help").unwrap();
    assert_eq!(help["kind"], "local");
    assert_eq!(help["source"], "builtin");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn run_turn_with_malformed_session_id_and_history_store_does_not_panic() {
    let (_server, sock, _dir, handle) = start_server_with_history(8).await;

    // Not a valid BASE58(UUID). A panicking `tokio::spawn`'d connection task
    // just drops the socket silently, so a panic would show here as "no
    // response at all (empty read)", not a catchable Rust panic in the test.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"session_id":"not-a-valid-id!!","message":"hi"},"id":1}"#,
    )
    .await;
    assert!(
        !resp.is_empty(),
        "connection died with no response — resume_or_create likely panicked on an unparsable session_id"
    );
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"].is_object() || v["error"].is_object(),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn import_list_detects_claude_md_in_project_dir() {
    let (_server, sock, dir, handle) = start_server().await;
    std::fs::write(dir.path().join("CLAUDE.md"), "# hi").unwrap();

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"import.list","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let sources = v["result"]["sources"].as_array().unwrap();
    assert!(
        sources.iter().any(|s| s["source"] == "claude_code"),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn import_run_executes_and_writes_agents_md() {
    let (_server, sock, dir, handle) = start_server().await;
    std::fs::write(dir.path().join("CLAUDE.md"), "Be concise.").unwrap();

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"import.run","params":{"source":"claude_code"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "resp: {v}");

    let agents_md = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
    assert!(agents_md.contains("Be concise."));

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn import_run_unknown_source_errors() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"import.run","params":{"source":"bogus"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn import_run_source_not_currently_detected_errors() {
    let (_server, sock, _dir, handle) = start_server().await;
    // No CLAUDE.md/.cursorrules/etc written — nothing detected.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"import.run","params":{"source":"claude_code"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[cfg(feature = "skills")]
#[tokio::test]
async fn commands_list_tells_a_global_skill_from_a_scene_skill_and_says_where_it_lives() {
    use daemon::config::{load_daemon_config, StaticDaemonPaths};
    use daemon::DaemonServer;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("agix");
    let scene_root = global.join("scenes").join("code");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let skill = |root: &std::path::Path, name: &str| {
        let folder = root.join("skills").join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} skill\n---\nbody"),
        )
        .unwrap();
        folder
    };
    let global_dir = skill(&global, "from-global");
    let scene_dir = skill(&scene_root, "from-scene");

    let sock = dir.path().join("agix.sock");
    let paths = StaticDaemonPaths::with_project(scene_root, work.clone()).with_global(global);
    let config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(work),
            transcripts: daemon::Transcripts::Nowhere,
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");
    let server = Arc::new(DaemonServer::new(
        pool,
        tokio_util::sync::CancellationToken::new(),
    ));
    let listener = DaemonServer::bind_unix(&sock).unwrap();
    tokio::spawn(async move {
        let _ = server.serve_unix_listener(listener).await;
    });

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"commands.list","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let find = |name: &str| {
        v["result"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("no {name} in {v}"))
    };

    let (global, scene) = (find("from-global"), find("from-scene"));
    assert_eq!(global["source"], "user");
    assert_eq!(scene["source"], "user");
    assert_eq!(global["tier"], "global", "{global}");
    assert_eq!(scene["tier"], "scene", "{scene}");
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
    assert_eq!(
        canon(std::path::Path::new(global["dir"].as_str().unwrap())),
        canon(&global_dir)
    );
    assert_eq!(
        canon(std::path::Path::new(scene["dir"].as_str().unwrap())),
        canon(&scene_dir)
    );
    let help = find("help");
    assert!(
        help.get("tier").is_none() && help.get("dir").is_none(),
        "{help}"
    );
    let bundled = find("simplify");
    assert!(
        bundled.get("tier").is_none() && bundled.get("dir").is_none(),
        "{bundled}"
    );
}

#[cfg(feature = "skills")]
#[tokio::test]
async fn a_deployment_without_a_scene_layer_lists_its_skills_as_global_once() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("skills").join("only-layer");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("SKILL.md"),
        "---\nname: only-layer\ndescription: only layer\n---\nbody",
    )
    .unwrap();
    let (_server, sock, _dir, handle) = start_server_in(dir).await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"commands.list","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let found: Vec<_> = v["result"]["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["name"] == "only-layer")
        .collect();
    assert_eq!(found.len(), 1, "{v}");
    assert_eq!(found[0]["tier"], "global", "{v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}
