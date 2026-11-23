//! Sessions with no project, or the daemon default: their scratch directory, transcripts and permanent allows.

use std::time::Duration;

use crate::support::*;
use daemon::rpc::codes;

#[tokio::test]
async fn a_no_project_sessions_transcript_lands_in_the_no_project_bucket() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;

    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"harnix"},"id":1}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"harnix","project_root":null},"id":2}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&sid), "first").await;

    assert!(
        transcript_dir(srv._dir.path(), None)
            .join(format!("{sid}.jsonl"))
            .exists(),
        "a session bound to no project must not be filed under one"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_create_rejects_a_nonexistent_project_root() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let missing = srv._dir.path().join("does-not-exist");
    let resp = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            missing.display()
        ),
    )
    .await;
    assert_eq!(resp["error"]["code"], codes::PROJECT_NOT_FOUND, "{resp}");

    srv.stop().await;
}

#[tokio::test]
async fn session_create_with_null_project_root_is_a_no_project_session() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    // `harnix`, not the daemon's `code`: a no-project session is only
    // meaningful in a scene that doesn't require one, and `code`
    // refuses (see `SceneSpec::requires_project`).
    let _ = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"harnix"},"id":0}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"harnix","project_root":null},"id":1}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(created["result"]["project_root"].is_null(), "{created}");

    let entries = srv
        .store
        .load(base::session::SessionId::parse(&sid).unwrap())
        .await
        .unwrap();
    match &entries[0].entry {
        history::entry::LogEntry::Meta {
            project_root,
            session_kind,
            ..
        } => {
            assert_eq!(*project_root, None);
            assert_eq!(*session_kind, history::entry::SessionKind::Primary);
        }
        other => panic!("expected Meta as the first entry, got {other:?}"),
    }

    srv.stop().await;
}

/// A session without a project still gets its work done, unattended and
/// unasked: it writes, reads and runs commands in a directory of its own in
/// the state root's scratchpad, and writes the shared scratchpad too. What
/// is around that directory is no project — not a git repository the state
/// root sits in, nor an `AGENTS.md` above it.
#[tokio::test]
async fn a_no_project_session_works_in_its_own_scratch_directory() {
    let rounds = vec![
        tool_round(
            "Write",
            "w1",
            serde_json::json!({"file_path": "notes.md", "content": "NOTE-ONE\n"}),
        ),
        tool_round("Read", "r1", serde_json::json!({"file_path": "notes.md"})),
        tool_round(
            "Bash",
            "b1",
            serde_json::json!({"command": "cat notes.md > copy.md && pwd"}),
        ),
        tool_round(
            "Write",
            "w2",
            serde_json::json!({"file_path": "../../shared.md", "content": "SHARED\n"}),
        ),
        text_round("done"),
    ];
    let mut settings = ask_settings();
    settings.sandbox.network.mode = base::interface::exec::NetworkMode::DenyAll;
    let (srv, seen) = start_scripted_server(rounds, settings, Duration::ZERO).await;
    let state_root = srv._dir.path().to_path_buf();
    std::fs::write(state_root.join("AGENTS.md"), "STATE-ROOT-AGENTS-MARKER").unwrap();
    std::fs::write(state_root.join("repo-marker.txt"), "x").unwrap();
    let git = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&state_root)
        .status()
        .unwrap();
    assert!(git.success());

    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"scene.activate","params":{"scene":"harnix"},"id":0}"#,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"harnix","project_root":null},"id":1}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();
    let (frames, response) = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"go"}},"id":2}}"#
        ),
    )
    .await;
    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(events_of_kind(&frames, "prompt").is_empty(), "{frames:?}");

    let work = state_root.join("scratchpad").join("sessions").join(&sid);
    let results = events_of_kind(&frames, "tool_result");
    let result = |id: &str| {
        results
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("no tool_result for {id}: {frames:?}"))
    };
    // Without an OS sandbox an unattended session's commands are refused,
    // not run unconfined, so `Bash` only goes through in a sandboxed build.
    let sandboxed = cfg!(feature = "sandbox");
    for id in ["w1", "r1", "w2"] {
        assert_eq!(result(id)["is_error"], false, "{id}: {}", result(id));
    }
    assert_eq!(result("b1")["is_error"], !sandboxed, "b1: {}", result("b1"));
    assert_eq!(
        std::fs::read_to_string(work.join("notes.md")).unwrap(),
        "NOTE-ONE\n"
    );
    assert!(result("r1")["content"].to_string().contains("NOTE-ONE"));
    if sandboxed {
        assert_eq!(
            std::fs::read_to_string(work.join("copy.md")).unwrap(),
            "NOTE-ONE\n"
        );
        assert!(result("b1")["content"]
            .to_string()
            .contains(&format!("sessions/{sid}")));
    }
    assert_eq!(
        std::fs::read_to_string(state_root.join("scratchpad").join("shared.md")).unwrap(),
        "SHARED\n"
    );

    let shown = serde_json::to_string(&*seen.lock().unwrap()).unwrap();
    assert!(
        !shown.contains("STATE-ROOT-AGENTS-MARKER"),
        "an AGENTS.md above the scratch directory was loaded"
    );
    assert!(
        !shown.contains("repo-marker.txt"),
        "the repository around the state root was reported as the session's"
    );

    let deleted = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.delete","params":{{"session_id":"{sid}"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(deleted["result"]["deleted"], true, "{deleted}");
    assert!(
        !work.exists(),
        "a deleted session left its directory behind"
    );

    srv.stop().await;
}

/// A session created without `project_root` belongs to the project the
/// daemon was assembled on, and works there: a relative path is read from
/// that directory, not from the daemon's state root.
#[tokio::test]
async fn a_default_project_session_works_in_the_assembled_directory() {
    let rounds = vec![
        tool_round("Read", "r1", serde_json::json!({"file_path": "probe.txt"})),
        text_round("done"),
    ];
    let (srv, _seen) = start_scripted_server(rounds, ask_settings(), Duration::ZERO).await;
    let state_root = srv._dir.path().to_path_buf();
    let work = state_root.join("work");
    std::fs::write(work.join("probe.txt"), "IN-THE-PROJECT\n").unwrap();
    std::fs::write(state_root.join("probe.txt"), "IN-THE-STATE-ROOT\n").unwrap();

    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{},"id":1}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();
    assert_eq!(
        created["result"]["project_root"],
        work.display().to_string(),
        "{created}"
    );
    let (frames, response) = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"go"}},"id":2}}"#
        ),
    )
    .await;
    assert!(response["result"].is_object(), "turn failed: {response}");
    let results = events_of_kind(&frames, "tool_result");
    let read = results
        .iter()
        .find(|r| r["id"] == "r1")
        .unwrap_or_else(|| panic!("no tool_result for r1: {frames:?}"));
    assert_eq!(read["is_error"], false, "{read}");
    let content = read["content"].to_string();
    assert!(content.contains("IN-THE-PROJECT"), "{content}");

    srv.stop().await;
}

/// "Always allow, this project" has no project to mean without one: the
/// answer holds for the rest of the session and is written nowhere — not
/// into the state root's `settings.local.json`, which is the global tier.
#[tokio::test]
async fn a_no_project_sessions_permanent_allow_stays_in_the_session() {
    let beside = serde_json::json!("../../../beside.md");
    let rounds = vec![
        tool_round(
            "Write",
            "tool-1",
            serde_json::json!({"file_path": beside, "content": "one\n"}),
        ),
        tool_round("Read", "tool-2", serde_json::json!({"file_path": beside})),
        tool_round(
            "Write",
            "tool-3",
            serde_json::json!({"file_path": beside, "content": "two\n"}),
        ),
        text_round("done"),
    ];
    let (srv, _seen) = start_scripted_server(rounds, ask_settings(), Duration::from_secs(30)).await;
    let state_root = srv._dir.path().to_path_buf();
    let scene_dir = state_root.join("scenes").join("desk");
    std::fs::create_dir_all(&scene_dir).unwrap();
    std::fs::write(
        scene_dir.join("scene.json"),
        r#"{"id": "desk", "name": "Desk", "interactive": true, "tools": ["Read", "Write"]}"#,
    )
    .unwrap();
    std::fs::write(scene_dir.join("prompt.md"), "Help at the desk.").unwrap();
    for (i, method) in ["scene.reload", "scene.activate"].iter().enumerate() {
        let reply = rpc(
            &srv.sock,
            &format!(
                r#"{{"jsonrpc":"2.0","method":"{method}","params":{{"scene":"desk"}},"id":{i}}}"#
            ),
        )
        .await;
        assert!(reply["result"].is_object(), "{method}: {reply}");
    }
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{"scene":"desk","project_root":null},"id":2}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();

    let (frames, response) = run_turn_answering_prompts(
        &srv.sock,
        "go",
        &format!(r#","session_id":"{sid}""#),
        |_| Some(r#"{"type":"permit_always","scope":"local"}"#.to_string()),
    )
    .await;
    assert!(response["result"].is_object(), "turn failed: {response}");
    assert_eq!(events_of_kind(&frames, "prompt").len(), 1, "{frames:?}");
    assert_eq!(
        std::fs::read_to_string(state_root.join("beside.md")).unwrap(),
        "two\n"
    );
    let global_local =
        std::fs::read_to_string(state_root.join("settings.local.json")).unwrap_or_default();
    assert!(
        !global_local.contains("beside.md"),
        "a no-project session's permanent allow reached the global tier: {global_local}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_create_without_project_root_uses_the_pool_default() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","params":{},"id":1}"#,
    )
    .await;
    let default_project = srv._dir.path().join("work");
    assert_eq!(
        created["result"]["project_root"],
        default_project.display().to_string(),
        "{created}"
    );

    srv.stop().await;
}
