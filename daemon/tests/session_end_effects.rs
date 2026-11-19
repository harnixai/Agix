//! Session start and end hooks, and processes left in the background: what close, eviction and shutdown do to them.

use std::time::Duration;

use crate::support::*;

#[tokio::test]
async fn a_stop_hook_that_discontinues_still_ends_the_turn_and_frees_the_session() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("answer")],
        hooked_settings(serde_json::json!({
            "Stop": [
                { "type": "command", "command": "echo '{\"continue\":false,\"message\":\"enough\"}'" }
            ]
        })),
        Duration::ZERO,
    )
    .await;

    let (sid, done) = turn_complete_of(&srv.sock, None, "hello").await;
    assert_eq!(done["stop_reason"], "stopped_by_hook");
    assert_eq!(done["message"], "enough");

    let (_, again) = turn_complete_of(&srv.sock, Some(&sid), "hello again").await;
    assert_eq!(again["stop_reason"], "stopped_by_hook");

    srv.stop().await;
}

/// A slow `SessionEnd` hook still runs when the session is rebuilt for a new
/// prompt, but the rebuild does not wait for it.
#[tokio::test]
async fn a_slow_session_end_hook_does_not_hold_up_a_rebuild() {
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_dir.path().join("hooks.log");
    let (srv, _seen) = start_scripted_server(
        vec![text_round("first"), text_round("second")],
        hooked_settings(serde_json::json!({
            "SessionEnd": [{
                "type": "command",
                "command": format!("sleep 8; echo end >> '{}'", log.display()),
                "timeout": 30000
            }]
        })),
        Duration::ZERO,
    )
    .await;
    let sid = run_turn(&srv.sock, None, "hello").await;

    let set = tokio::time::timeout(
        Duration::from_secs(4),
        rpc(
            &srv.sock,
            &format!(
                r#"{{"jsonrpc":"2.0","method":"session.setPrompt","params":{{"session_id":"{sid}","append":"Answer briefly."}},"id":3}}"#
            ),
        ),
    )
    .await
    .expect("the rebuild waited for the SessionEnd hook");
    assert!(set["result"].is_object(), "{set}");
    let again = tokio::time::timeout(
        Duration::from_secs(4),
        run_turn(&srv.sock, Some(&sid), "again"),
    )
    .await
    .expect("the next turn waited for the SessionEnd hook");
    assert_eq!(again, sid, "the session was not kept");

    tokio::time::timeout(Duration::from_secs(15), async {
        while !std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("end")
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the SessionEnd hook never finished");

    srv.stop().await;
}

/// The `SessionEnd` input a hook wrote to `path`, once the whole of it is there.
async fn session_end_input(path: &std::path::Path) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            if let Ok(input) = serde_json::from_str::<serde_json::Value>(&text) {
                return input;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the SessionEnd hook never wrote its input")
}

/// `session.close` tells the `SessionEnd` hook why the session ended.
#[tokio::test]
async fn a_closed_session_ends_with_reason_close() {
    let log_dir = tempfile::tempdir().unwrap();
    let input = log_dir.path().join("input.json");
    let (srv, _seen) = start_scripted_server(
        vec![text_round("hi")],
        hooked_settings(serde_json::json!({
            "SessionEnd": [
                { "type": "command", "command": format!("cat > '{}'", input.display()) }
            ]
        })),
        Duration::ZERO,
    )
    .await;
    let sid = run_turn(&srv.sock, None, "hello").await;
    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","id":5,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;

    let ended = session_end_input(&input).await;
    assert_eq!(ended["hook_event_name"], "SessionEnd", "{ended}");
    assert_eq!(ended["reason"], "close", "{ended}");

    srv.stop().await;
}

/// Each engine's `SessionStart` hook says why it started: a new session, a
/// new engine for a new prompt, a session brought back from its log.
#[tokio::test]
async fn session_start_says_why_the_session_started() {
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_dir.path().join("starts.log");
    let (srv, _seen) = start_scripted_server(
        vec![text_round("hi")],
        hooked_settings(serde_json::json!({
            "SessionStart": [
                { "type": "command", "command": format!("cat >> '{0}'; echo >> '{0}'", log.display()) }
            ]
        })),
        Duration::ZERO,
    )
    .await;
    let sources = |n: usize| {
        let log = log.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let text = std::fs::read_to_string(&log).unwrap_or_default();
                    let inputs: Vec<serde_json::Value> = text
                        .lines()
                        .filter_map(|line| serde_json::from_str(line).ok())
                        .collect();
                    if inputs.len() >= n {
                        return inputs
                            .iter()
                            .map(|i| i["source"].as_str().unwrap_or("-").to_string())
                            .collect::<Vec<_>>();
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("a SessionStart hook never wrote its input")
        }
    };

    let sid = run_turn(&srv.sock, None, "hello").await;
    assert_eq!(sources(1).await, ["new"]);
    let set = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.setPrompt","params":{{"session_id":"{sid}","append":"Answer briefly."}},"id":3}}"#
        ),
    )
    .await;
    assert!(set["result"].is_object(), "{set}");
    assert_eq!(sources(2).await, ["new", "rebuild"]);
    close_and_resume(&srv.sock, &sid).await;
    assert_eq!(sources(3).await, ["new", "rebuild", "resume"]);

    srv.stop().await;
}

/// A `SessionEnd` hook still running when the daemon shuts down is waited
/// for within the shutdown grace, then sent `SIGTERM`; it learns the session
/// ended for a shutdown.
#[cfg(unix)]
#[tokio::test]
async fn a_session_end_hook_running_at_shutdown_gets_sigterm() {
    let log_dir = tempfile::tempdir().unwrap();
    let input = log_dir.path().join("input.json");
    let term = log_dir.path().join("term");
    let (srv, _seen) = start_scripted_server(
        vec![text_round("hi")],
        hooked_settings(serde_json::json!({
            "SessionEnd": [{
                "type": "command",
                "command": format!(
                    "trap 'echo term > \"{}\"; exit 0' TERM; cat > '{}'; sleep 30 & wait",
                    term.display(),
                    input.display()
                ),
                "timeout": 60000
            }]
        })),
        Duration::ZERO,
    )
    .await;
    run_turn(&srv.sock, None, "hello").await;

    tokio::time::timeout(Duration::from_secs(10), srv.server.shutdown())
        .await
        .expect("the shutdown waited for the SessionEnd hook past its grace");

    assert_eq!(session_end_input(&input).await["reason"], "shutdown");
    assert!(term.exists(), "the SessionEnd hook was not sent SIGTERM");

    srv.stop().await;
}

/// A turn stuck in a slow notify-only hook does not keep its session from
/// closing: closed, it can be resumed at once.
#[tokio::test]
async fn a_turn_in_a_slow_turn_start_hook_does_not_hold_up_close_and_resume() {
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_dir.path().join("hooks.log");
    let (srv, _seen) = start_scripted_server(
        vec![text_round("answer")],
        hooked_settings(serde_json::json!({
            "TurnStart": [{
                "type": "command",
                "command": format!("echo started >> '{}'; sleep 8", log.display()),
                "timeout": 30000
            }]
        })),
        Duration::ZERO,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":1,"params":{}}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no session: {created}"))
        .to_string();

    let _turn = crate::support::begin(
        &srv.sock,
        "session.run_turn",
        serde_json::json!({"session_id": sid, "message": "hello"}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("started")
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the TurnStart hook never ran");

    tokio::time::timeout(Duration::from_secs(2), close_and_resume(&srv.sock, &sid))
        .await
        .expect("close and resume waited for the TurnStart hook");

    srv.stop().await;
}

/// What a test's background process is called, so it can be found among
/// everyone else's. `$!` would not do: on Linux the command runs in a PID
/// namespace of its own, and the pid it reports means nothing out here.
fn background_name(test: &str) -> String {
    format!("agix-left-running-{}-{test}", std::process::id())
}

async fn start_server_running_a_background_sleep(reap: bool, name: &str) -> ScriptedServer {
    let mut settings = ask_settings_no_memory();
    settings.permissions.mode = base::permission::PermissionMode::BypassPermissions;
    settings.sandbox.reap_on_close = reap;
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round(
                "Bash",
                "tool-1",
                serde_json::json!({"command": format!("exec -a {name} tail -f /dev/null >/dev/null 2>&1 &")}),
            ),
            text_round("started"),
        ],
        settings,
        Duration::from_secs(30),
    )
    .await;
    srv
}

/// The session the turn ran in, and the pid of the process it left behind.
async fn turn_leaving_a_sleep(sock: &std::path::Path, name: &str) -> (String, libc::pid_t) {
    let (frames, response) = rpc_streaming(
        sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"start it"},"id":1}"#,
    )
    .await;
    assert!(response["result"].is_object(), "turn failed: {response}");
    assert_eq!(
        events_of_kind(&frames, "tool_result")[0]["is_error"],
        false,
        "{frames:?}"
    );
    let pid = pid_named(name, Duration::from_secs(10))
        .await
        .unwrap_or_else(|| panic!("the command left nothing named {name} running"));
    (
        response["result"]["session_id"]
            .as_str()
            .unwrap()
            .to_string(),
        pid,
    )
}

/// The process whose command line starts with `name`, once there is one.
async fn pid_named(name: &str, limit: Duration) -> Option<libc::pid_t> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        let listing = std::process::Command::new("ps")
            .args(["-A", "-o", "pid=", "-o", "args="])
            .output()
            .unwrap();
        let found = String::from_utf8_lossy(&listing.stdout)
            .lines()
            .find_map(|line| {
                let (pid, args) = line.trim_start().split_once(' ')?;
                let first = args.trim_start().split(' ').next()?;
                (first == name).then(|| pid.parse().ok()).flatten()
            });
        if found.is_some() || std::time::Instant::now() >= deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only asks whether the pid exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn gone_within(pid: libc::pid_t, limit: Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn kill_leftover(pid: libc::pid_t) {
    if alive(pid) {
        // SAFETY: plain integers; the pid is the test's own sleep.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

#[tokio::test]
async fn closing_a_session_stops_what_its_commands_left_running() {
    let name = background_name("close");
    let srv = start_server_running_a_background_sleep(true, &name).await;
    let (sid, pid) = turn_leaving_a_sleep(&srv.sock, &name).await;
    assert!(alive(pid), "the command left its sleep running");

    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","id":2,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    let gone = gone_within(pid, Duration::from_secs(3)).await;
    kill_leftover(pid);
    assert!(gone, "session.close reaps the background sleep");

    srv.stop().await;
}

#[tokio::test]
async fn without_the_setting_a_background_process_outlives_its_session() {
    let name = background_name("default");
    let srv = start_server_running_a_background_sleep(false, &name).await;
    let (sid, pid) = turn_leaving_a_sleep(&srv.sock, &name).await;

    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","id":2,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    let gone = gone_within(pid, Duration::from_millis(1500)).await;
    kill_leftover(pid);
    assert!(!gone, "the default leaves a background service alone");

    srv.stop().await;
}

#[tokio::test]
async fn a_daemon_shutdown_stops_what_its_sessions_left_running() {
    let name = background_name("shutdown");
    let srv = start_server_running_a_background_sleep(true, &name).await;
    let (_sid, pid) = turn_leaving_a_sleep(&srv.sock, &name).await;

    rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"daemon.shutdown","id":2}"#,
    )
    .await;
    let gone = gone_within(pid, Duration::from_secs(3)).await;
    kill_leftover(pid);
    assert!(gone, "shutdown_all reaps the background sleep");

    srv.stop().await;
}

#[tokio::test]
async fn evicting_a_session_stops_what_its_commands_left_running() {
    let name = background_name("evict");
    let srv = start_server_running_a_background_sleep(true, &name).await;
    let (_sid, pid) = turn_leaving_a_sleep(&srv.sock, &name).await;

    // The pool holds 8; the eighth new one pushes the oldest out.
    for i in 0..8 {
        let created = rpc(
            &srv.sock,
            &format!(r#"{{"jsonrpc":"2.0","method":"session.create","id":{i},"params":{{}}}}"#),
        )
        .await;
        assert!(created["result"]["session_id"].is_string(), "{created}");
    }
    let gone = gone_within(pid, Duration::from_secs(3)).await;
    kill_leftover(pid);
    assert!(gone, "LRU eviction reaps the background sleep");

    srv.stop().await;
}
