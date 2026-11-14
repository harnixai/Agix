//! Resuming and listing sessions: reattaching, refusals, sidechains, and one writer per session log.

use std::time::Duration;

use crate::support::*;
use daemon::rpc::codes;
use model::types::MessagesRequest;

/// `flock` the file from an open file of the test's own — as another process
/// would — or `None` when somebody already holds it.
#[cfg(unix)]
fn lock_elsewhere(path: &std::path::Path) -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new().write(true).open(path).ok()?;
    // SAFETY: `flock` only reads the descriptor, which `file` keeps open.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (rc == 0).then_some(file)
}

/// [`lock_elsewhere`], allowing a moment for a child process another test
/// forked to exec and let go of its inherited copy of the descriptor.
#[cfg(unix)]
async fn lock_elsewhere_soon(path: &std::path::Path) -> Option<std::fs::File> {
    for _ in 0..100 {
        if let Some(file) = lock_elsewhere(path) {
            return Some(file);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

/// A session open in this daemon cannot be written by another one, a session
/// another one holds is refused rather than handed back under a fresh id, and
/// a log whose last line a crash cut short still resumes with its history.
#[cfg(unix)]
#[tokio::test]
async fn a_session_log_has_one_writer_and_survives_a_cut_last_line() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("noted"), text_round("ostrich")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let sid = run_turn(&srv.sock, None, "the word is ostrich").await;
    let lock = srv
        ._dir
        .path()
        .join("sessions")
        .join(&sid)
        .join("writer.lock");
    assert!(
        lock_elsewhere(&lock).is_none(),
        "a live session's log is held by its daemon"
    );

    rpc(
        &srv.sock,
        &format!(r#"{{"jsonrpc":"2.0","method":"session.close","id":2,"params":{{"session_id":"{sid}"}}}}"#),
    )
    .await;
    let other = lock_elsewhere_soon(&lock)
        .await
        .expect("closing the session let go of its log");
    for method in ["session.resume", "session.run_turn"] {
        let resp = rpc(
            &srv.sock,
            &format!(
                r#"{{"jsonrpc":"2.0","method":"{method}","id":3,"params":{{"session_id":"{sid}","message":"x"}}}}"#
            ),
        )
        .await;
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("another process"),
            "{method}: {resp}"
        );
    }
    drop(other);

    let projects = srv._dir.path().join("projects");
    let log = std::fs::read_dir(&projects)
        .unwrap()
        .flatten()
        .map(|dir| dir.path().join(format!("{sid}.jsonl")))
        .find(|path| path.is_file())
        .expect("the session's log");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(br#"{"uuid":"cut","session_id":"#).unwrap();
    }

    seen.lock().unwrap().clear();
    let resumed = rpc(
        &srv.sock,
        &format!(r#"{{"jsonrpc":"2.0","method":"session.resume","id":4,"params":{{"session_id":"{sid}"}}}}"#),
    )
    .await;
    assert_eq!(resumed["result"]["status"], "resumed", "{resumed}");
    assert_eq!(
        run_turn(&srv.sock, Some(&sid), "what was the word").await,
        sid
    );
    let sent = {
        let requests = seen.lock().unwrap();
        format!("{:?}", requests.last().expect("a model call").messages)
    };
    assert!(
        sent.contains("the word is ostrich"),
        "the history was lost: {sent}"
    );
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        !text.contains(r#""uuid":"cut""#),
        "the fragment is still in the log"
    );
    assert!(text
        .lines()
        .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok()));

    srv.stop().await;
}

/// Opening a session that is only on disk is one call, not subscribe,
/// recognize `SESSION_NOT_FOUND`, resume, subscribe again: the details that
/// sequence has to get right (which scene, which project) are the ones a
/// client is worst placed to know.
#[tokio::test]
async fn subscribing_with_resume_opens_a_session_that_left_memory() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("answer")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "hello").await;
    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","id":1,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    assert_eq!(closed["result"]["closed"], sid.as_str());

    // The default is unchanged: subscribing does not resume, and says so
    // rather than handing back a subscription that will never see a frame.
    let refused = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.subscribe","id":2,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    assert_eq!(
        refused["error"]["code"],
        codes::SESSION_NOT_FOUND as i64,
        "subscribe still refuses a cold session by default: {refused}"
    );

    let opened = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.subscribe","id":3,"params":{{"session_id":"{sid}","resume":true}}}}"#
        ),
    )
    .await;
    assert!(
        opened.get("result").is_some(),
        "subscribe with resume opens it: {opened}"
    );
    assert_eq!(opened["result"]["session_id"], sid.as_str());
    assert_eq!(
        opened["result"]["resumed"], true,
        "the caller is told a resume happened on its behalf: {opened}"
    );

    // Already live now: the same call subscribes without resuming again.
    let again = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.subscribe","id":4,"params":{{"session_id":"{sid}","resume":true}}}}"#
        ),
    )
    .await;
    assert_eq!(
        again["result"]["resumed"], false,
        "a live session is subscribed to, not resumed: {again}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn session_resume_reattaches_and_reports_state() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("stored answer")],
        ask_settings(),
        Duration::from_secs(42),
    )
    .await;

    let sid = run_turn(&srv.sock, None, "remembered question").await;

    // An already-live session is reported as such, not recreated.
    let active = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(active["result"]["status"], "already_active", "{active}");
    assert!(active["result"]["message_count"].as_u64().unwrap() >= 2);
    assert_eq!(active["result"]["scene"], "code");
    assert_eq!(active["result"]["permission"]["mode"], "default");
    assert_eq!(active["result"]["permission"]["prompts"], true);
    assert_eq!(active["result"]["permission"]["prompt_timeout_secs"], 42);

    // Drop it from memory the way a daemon restart / idle eviction would,
    // then reattach: the transcript must come back with it.
    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(closed["result"]["closed"], sid);

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{sid}"}},"id":4}}"#
        ),
    )
    .await;
    assert_eq!(resumed["result"]["status"], "resumed", "{resumed}");
    assert_eq!(resumed["result"]["session_id"], sid);
    assert_eq!(resumed["result"]["active"], true);
    assert!(
        resumed["result"]["message_count"].as_u64().unwrap() >= 2,
        "resume must report what it reattached to: {resumed}"
    );
    assert!(resumed["result"]["entry_count"].as_u64().unwrap() >= 2);

    // And the reattached engine really does hold the old context.
    seen.lock().unwrap().clear();
    run_turn(&srv.sock, Some(&sid), "and now").await;
    let sent = {
        let requests = seen.lock().unwrap();
        format!("{:?}", requests.last().unwrap().messages)
    };
    assert!(
        sent.contains("remembered question"),
        "resumed session lost its transcript: {sent}"
    );

    srv.stop().await;
}

/// A session that cannot be brought back is an error under its own id, not a
/// fresh session the turn quietly runs in instead.
#[tokio::test]
async fn resume_failure_does_not_switch_session_id() {
    let (srv, seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;
    let sid = inject_session_in_scene(&srv, "code").await;
    srv.store
        .append(
            sid,
            history::entry::LogEntry::Extension {
                ns: "agix.session".into(),
                event: "spec".into(),
                payload: serde_json::json!({"provider": "no-longer-configured"}),
            },
        )
        .await
        .unwrap();

    let turn = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"hi"}},"id":1}}"#
        ),
    )
    .await;
    assert!(turn["result"].is_null(), "{turn}");
    assert!(
        turn["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no-longer-configured"),
        "{turn}"
    );
    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert!(resumed["result"].is_null(), "{resumed}");
    assert!(seen.lock().unwrap().is_empty(), "a turn ran somewhere");

    let listed = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","params":{},"id":3}"#,
    )
    .await;
    let ids: Vec<&str> = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["session_id"].as_str())
        .collect();
    assert_eq!(ids, [sid.to_string().as_str()], "{listed}");

    srv.stop().await;
}

#[tokio::test]
async fn session_resume_refuses_an_unknown_id_unless_asked_to_create() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let missing = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.resume","params":{"session_id":"NopeNopeNopeNopeNope12"},"id":1}"#,
    )
    .await;
    assert_eq!(
        missing["error"]["code"],
        codes::SESSION_NOT_FOUND,
        "{missing}"
    );

    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.resume","params":{"session_id":"NopeNopeNopeNopeNope12","create_if_missing":true},"id":2}"#,
    )
    .await;
    assert_eq!(created["result"]["status"], "created", "{created}");
    assert_eq!(created["result"]["message_count"], 0);

    srv.stop().await;
}

/// A sidechain that already ran its one-shot task to
/// conclusion (has a `SessionEnd` marker) must not be resumable — a stale
/// `session.resume` against it is rejected with `SIDECHAIN_TERMINAL`
/// instead of silently reattaching to a "finished" transcript.
#[tokio::test]
async fn session_resume_rejects_a_terminal_sidechain() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;
    srv.store
        .append(
            child,
            history::entry::LogEntry::SessionEnd {
                state: history::entry::SessionEndState::Completed,
            },
        )
        .await
        .unwrap();

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{child}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(
        resumed["error"]["code"],
        codes::SIDECHAIN_TERMINAL,
        "{resumed}"
    );
    assert_eq!(resumed["error"]["data"]["session_id"], child.to_string());
    assert_eq!(resumed["error"]["data"]["parent_session_id"], parent);
    assert_eq!(resumed["error"]["data"]["final_state"], "completed");

    srv.stop().await;
}

/// `session.subscribe {resume: true}` opens a cold session, and "cold" must
/// not become a way around the rule that a finished sidechain has no
/// continuation. It refuses through the same mapping `session.resume` uses,
/// which is why the structured `data` is asserted here too: a second copy of
/// that mapping is exactly what would drift.
#[tokio::test]
async fn opening_a_terminal_sidechain_is_refused_the_same_way() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;
    srv.store
        .append(
            child,
            history::entry::LogEntry::SessionEnd {
                state: history::entry::SessionEndState::Failed,
            },
        )
        .await
        .unwrap();

    let opened = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.subscribe","params":{{"session_id":"{child}","resume":true}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(
        opened["error"]["code"],
        codes::SIDECHAIN_TERMINAL,
        "{opened}"
    );
    assert_eq!(opened["error"]["data"]["parent_session_id"], parent);
    assert_eq!(opened["error"]["data"]["final_state"], "failed");

    // Without `resume` it is the plain refusal, not the sidechain one: the
    // session is simply not in memory, and nothing was asked to open it.
    let plain = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.subscribe","params":{{"session_id":"{child}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(plain["error"]["code"], codes::SESSION_NOT_FOUND, "{plain}");

    srv.stop().await;
}

/// Sanity counterpart: a sidechain with no `SessionEnd` marker (still
/// running, or cut off externally) must resume normally — the terminal
/// check must not fire on absence.
#[tokio::test]
async fn session_resume_allows_a_non_terminal_sidechain() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let resumed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","params":{{"session_id":"{child}"}},"id":1}}"#
        ),
    )
    .await;
    assert!(resumed.get("error").is_none(), "{resumed}");

    srv.stop().await;
}

/// Sidechains are hidden from the default list, and returned
/// exactly via `parent_session_id`.
#[tokio::test]
async fn session_list_excludes_sidechains_by_default_and_returns_them_via_parent_filter() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;

    let child_sid = base::session::SessionId::new();
    srv.store
        .append(
            child_sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                agix_version: "0.0.1".into(),
                parent_session_id: Some(parent.clone()),
                scene: Some("code".into()),
                project_root: None,
                session_kind: history::entry::SessionKind::Sidechain,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();

    let default_list = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","params":{},"id":1}"#,
    )
    .await;
    let ids: Vec<String> = default_list["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        ids.contains(&parent),
        "default list must include the primary session: {ids:?}"
    );
    assert!(
        !ids.contains(&child_sid.to_string()),
        "default list must not include the sidechain: {ids:?}"
    );

    let children = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.list","params":{{"parent_session_id":"{parent}"}},"id":2}}"#
        ),
    )
    .await;
    let child_ids: Vec<String> = children["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(child_ids, vec![child_sid.to_string()], "{children}");

    let with_children = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","params":{"include_children":true},"id":3}"#,
    )
    .await;
    let all_ids: Vec<String> = with_children["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap().to_string())
        .collect();
    assert!(all_ids.contains(&parent));
    assert!(all_ids.contains(&child_sid.to_string()));

    srv.stop().await;
}

/// `SessionInfo.resumable` (`docs/daemon_rpc_protocol.md` §3.3): a primary session is always resumable; a
/// sidechain is resumable until it gets a `SessionEnd` marker, after which
/// `session.list` must report it as `false` too — not just `session.resume`.
#[tokio::test]
async fn session_list_reports_resumable_and_flips_it_off_after_a_terminal_marker() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;
    let parent = run_turn(&srv.sock, None, "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let before = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.list","params":{{"parent_session_id":"{parent}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(
        before["result"]["sessions"][0]["resumable"], true,
        "{before}"
    );

    srv.store
        .append(
            child,
            history::entry::LogEntry::SessionEnd {
                state: history::entry::SessionEndState::Failed,
            },
        )
        .await
        .unwrap();

    let after = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.list","params":{{"parent_session_id":"{parent}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(
        after["result"]["sessions"][0]["resumable"], false,
        "{after}"
    );

    let primary_list = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","params":{},"id":3}"#,
    )
    .await;
    let primary = primary_list["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == parent)
        .expect("primary session should be listed");
    assert_eq!(primary["resumable"], true, "{primary}");

    srv.stop().await;
}

/// How many opening context messages `request` shows the model.
fn opening_reminders(request: &MessagesRequest) -> usize {
    format!("{:?}", request.messages)
        .matches("As you answer the user's questions")
        .count()
}

#[tokio::test]
async fn a_resumed_session_is_shown_its_opening_context_once() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("first"), text_round("second")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let sid = run_turn(&srv.sock, None, "hello").await;
    assert_eq!(opening_reminders(seen.lock().unwrap().last().unwrap()), 1);

    rpc(
        &srv.sock,
        &format!(r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":2}}"#),
    )
    .await;
    run_turn(&srv.sock, Some(&sid), "again").await;
    assert_eq!(opening_reminders(seen.lock().unwrap().last().unwrap()), 1);

    srv.stop().await;
}

/// A session pushed out of a full pool and used again comes back with its
/// history, and each engine's `SessionStart` and `SessionEnd` hooks run — an
/// evicted engine's `SessionEnd` in the background, which the session coming
/// back does not wait for.
#[tokio::test]
async fn an_evicted_session_resumes_without_locked() {
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_dir.path().join("hooks.log");
    let (srv, seen) = start_scripted_server_holding(
        "code",
        vec![
            text_round("noted"),
            text_round("other"),
            text_round("ostrich"),
        ],
        hooked_settings(serde_json::json!({
            "SessionStart": [
                { "type": "command", "command": format!("echo start >> '{}'", log.display()) }
            ],
            "SessionEnd": [
                { "type": "command", "command": format!("sleep 1; echo end >> '{}'", log.display()) }
            ]
        })),
        Duration::ZERO,
        1,
    )
    .await;

    let first = run_turn(&srv.sock, None, "the word is ostrich").await;
    let second = run_turn(&srv.sock, None, "something else").await;
    assert_ne!(first, second);
    seen.lock().unwrap().clear();
    assert_eq!(
        run_turn(&srv.sock, Some(&first), "what was the word").await,
        first
    );
    let sent = format!("{:?}", seen.lock().unwrap().last().unwrap().messages);
    assert!(sent.contains("the word is ostrich"), "history lost: {sent}");

    let lines = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let lines: Vec<String> = text.lines().map(str::to_string).collect();
            if lines.len() >= 5 {
                return lines;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("every hook ran");
    let count = |what: &str| lines.iter().filter(|line| *line == what).count();
    assert_eq!((count("start"), count("end")), (3, 2), "{lines:?}");

    srv.stop().await;
}
