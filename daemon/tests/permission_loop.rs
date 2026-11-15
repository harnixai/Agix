//! The `ask`-by-default permission loop: prompts, answers, denials, rules from settings and per-session options.

use std::time::Duration;

use crate::support::*;

#[tokio::test]
async fn ask_default_prompts_then_proceeds_once_permitted() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("permitted.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = run_turn_answering_prompts(&srv.sock, "make a file", "", |_| {
        Some(r#"{"type":"permit"}"#.to_string())
    })
    .await;

    let prompts = events_of_kind(&frames, "prompt");
    assert!(
        !prompts.is_empty(),
        "the new default must emit a kind:\"prompt\" frame for an unlisted Bash call: {frames:?}"
    );
    assert_eq!(prompts[0]["prompt_type"], "permission");
    assert_eq!(prompts[0]["tool_name"], "Bash");
    assert!(prompts[0]["prompt_id"].is_string());
    assert_eq!(prompts[0]["tool_use_id"], "tool-1");
    assert!(response["result"].is_object(), "turn failed: {response}");
    let done = events_of_kind(&frames, "turn_complete");
    assert_eq!(done[0]["tool_calls"], 1, "{done:?}");

    let results = events_of_kind(&frames, "tool_result");
    assert!(
        !results.is_empty(),
        "no tool_result after permitting: {frames:?}"
    );
    let rendered = serde_json::to_string(&results).unwrap();
    assert!(
        !rendered.contains("Denied by permission"),
        "a permitted call was denied anyway: {rendered}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn ask_default_blocks_the_tool_when_denied() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("denied.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = run_turn_answering_prompts(&srv.sock, "make a file", "", |_| {
        Some(r#"{"type":"deny","reason":"not on my machine"}"#.to_string())
    })
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    let rendered = serde_json::to_string(&events_of_kind(&frames, "tool_result")).unwrap();
    assert!(
        rendered.contains("Denied by permission") && rendered.contains("not on my machine"),
        "a denied call must surface the denial to the model: {rendered}"
    );

    srv.stop().await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_read_that_climbs_out_through_a_symlink_asks_first() {
    let (srv, _seen) = start_scripted_server(
        vec![
            tool_round(
                "Read",
                "tool-1",
                serde_json::json!({"file_path": "link/../secret"}),
            ),
            text_round("done"),
        ],
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;
    let project = srv._dir.path().join("work");
    let project = project.as_path();
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = elsewhere.path();
    std::fs::create_dir_all(outside.join("sub")).unwrap();
    std::fs::write(outside.join("secret"), "outside-secret").unwrap();
    std::fs::write(project.join("secret"), "inside-file").unwrap();
    std::os::unix::fs::symlink(outside.join("sub"), project.join("link")).unwrap();

    let (frames, response) = run_turn_answering_prompts(&srv.sock, "read it", "", |_| {
        Some(r#"{"type":"deny","reason":"stay in the project"}"#.to_string())
    })
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    let prompts = events_of_kind(&frames, "prompt");
    assert!(
        prompts.iter().any(|p| p["tool_name"] == "Read"),
        "a Read landing outside the project must ask: {frames:?}"
    );
    let rendered = serde_json::to_string(&events_of_kind(&frames, "tool_result")).unwrap();
    assert!(!rendered.contains("outside-secret"), "{rendered}");

    srv.stop().await;
}

#[tokio::test]
async fn per_session_bypass_option_is_clamped_to_the_configured_mode() {
    // `settings.permission_mode` is a policy, not a default. A client
    // asking for `bypassPermissions` against an `ask`-configured daemon is
    // clamped and still prompted — otherwise anyone who can reach the socket
    // opts themselves out, and `crates/assembly/src/server/mod.rs` is explicit that there
    // is no per-method authorization on it.
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("session-bypassed.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file","options":{"permission_mode":"bypassPermissions"}},"id":1}"#,
    )
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        !events_of_kind(&frames, "prompt").is_empty(),
        "a client-requested bypass must be clamped and still prompt: {frames:?}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn allow_client_permission_override_lets_a_client_opt_out() {
    // ...and the operator can still hand that authority over explicitly.
    let mut settings = ask_settings();
    settings.allow_client_permission_override = true;
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("session-bypassed.txt"),
        settings,
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file","options":{"permission_mode":"bypassPermissions"}},"id":1}"#,
    )
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        events_of_kind(&frames, "prompt").is_empty(),
        "with the override enabled, bypassPermissions must not prompt: {frames:?}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn auto_and_yolo_open_a_session_that_reports_bypass_permissions() {
    for spelling in ["auto", "yolo"] {
        let mut settings = ask_settings();
        settings.allow_client_permission_override = true;
        let (srv, _seen) = start_scripted_server(
            asking_bash_rounds("aliased.txt"),
            settings,
            Duration::from_secs(30),
        )
        .await;

        let (frames, response) = rpc_streaming(
            &srv.sock,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session.run_turn",
                "params": {"message": "make a file", "options": {"permission_mode": spelling}},
                "id": 1,
            })
            .to_string(),
        )
        .await;
        assert!(response["result"].is_object(), "turn failed: {response}");
        assert!(
            events_of_kind(&frames, "prompt").is_empty(),
            "{spelling} must not prompt: {frames:?}"
        );
        let sid = response["result"]["session_id"].as_str().unwrap();

        let entries = srv
            .store
            .load(base::session::SessionId::parse(sid).unwrap())
            .await
            .unwrap();
        let recorded = entries.iter().find_map(|env| match &env.entry {
            history::entry::LogEntry::Extension { ns, payload, .. } if ns == "agix.session" => {
                Some(payload["options"]["permission_mode"].clone())
            }
            _ => None,
        });
        assert_eq!(recorded, Some(serde_json::json!("bypassPermissions")));

        let resumed = rpc(
            &srv.sock,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session.resume",
                "params": {"session_id": sid, "options": {"permission_mode": spelling}},
                "id": 2,
            })
            .to_string(),
        )
        .await;
        assert_eq!(
            resumed["result"]["permission"]["mode"], "bypassPermissions",
            "{resumed}"
        );

        srv.stop().await;
    }
}

#[tokio::test]
async fn settings_permission_rules_finally_reach_the_daemon() {
    // `settings.permission_rules` has been parsed by the settings loader for a
    // long time with no production consumer at all. Under the ask default it
    // finally has one: an Allow rule pre-empts the prompt.
    let settings = Seed(
        ask_settings(),
        vec![base::permission::PermissionRuleEntry {
            tool: "Bash(touch ruled.txt)".into(),
            action: base::permission::RuleBehavior::Allow,
            matching: Default::default(),
        }],
    );

    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("ruled.txt"),
        settings,
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file"},"id":1}"#,
    )
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        events_of_kind(&frames, "prompt").is_empty(),
        "an explicit allow rule must pre-empt the prompt: {frames:?}"
    );

    srv.stop().await;
}

/// The same for a settings file: a rule in `permission_rules` that does not
/// read is left out by itself, and the deny beside it still refuses the call.
#[tokio::test]
async fn a_bad_settings_rule_leaves_the_deny_rules_beside_it_in_force() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("denied.txt"),
        ask_settings(),
        Duration::from_secs(1),
    )
    .await;
    let project = srv._dir.path().join("ruled-project");
    std::fs::create_dir_all(project.join(".agix")).unwrap();
    std::fs::write(
        project.join(".agix").join("settings.json"),
        r#"{"memory_enabled": false, "permission_rules": [
             {"tool": "Read", "action": "allwo"},
             {"tool": "Bash", "action": "deny"}
           ]}"#,
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &serde_json::json!({"jsonrpc": "2.0", "method": "session.create", "id": 1,
            "params": {"project_root": project}})
        .to_string(),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session.create failed: {created}"))
        .to_string();
    let (frames, response) = rpc_streaming(
        &srv.sock,
        &serde_json::json!({"jsonrpc": "2.0", "method": "session.run_turn", "id": 2,
            "params": {"session_id": sid, "message": "make a file"}})
        .to_string(),
    )
    .await;

    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(
        events_of_kind(&frames, "prompt").is_empty(),
        "the deny rule was dropped with the bad one, so Bash was asked about: {frames:?}"
    );
    let rendered = serde_json::to_string(&events_of_kind(&frames, "tool_result")).unwrap();
    assert!(
        rendered.contains("Denied by permission") && !rendered.contains("no answer"),
        "the deny rule must refuse the call itself: {rendered}"
    );

    srv.stop().await;
}

/// A rule in `options.permission_rules` that does not read refuses the turn
/// instead of running it without that rule.
#[tokio::test]
async fn a_bad_session_rule_refuses_the_turn() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("denied.txt"),
        ask_settings(),
        Duration::from_secs(1),
    )
    .await;

    let (frames, response) = rpc_streaming(
        &srv.sock,
        &serde_json::json!({"jsonrpc": "2.0", "method": "session.run_turn", "id": 1, "params": {
            "message": "make a file",
            "options": {
                "permission_mode": "default",
                "permission_rules": [
                    {"source": "session", "behavior": "allwo", "tool_name": "Read"},
                    {"source": "session", "behavior": "deny", "tool_name": "Bash"},
                ],
            },
        }})
        .to_string(),
    )
    .await;

    assert_eq!(response["error"]["code"], -32602, "{response}");
    let invalid = &response["error"]["data"]["invalid_options"];
    assert_eq!(invalid.as_array().map(Vec::len), Some(1), "{response}");
    assert_eq!(invalid[0]["field"], "permission_rules[0]", "{response}");
    assert!(
        invalid[0]["error"].as_str().unwrap().contains("allwo"),
        "{response}"
    );
    assert!(
        events_of_kind(&frames, "tool_result").is_empty(),
        "a refused turn ran: {frames:?}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn an_unanswered_prompt_is_denied_instead_of_hanging_forever() {
    // A one-second ceiling stands in for the production default (300s): the
    // property under test is "the turn ends without anyone answering", not
    // the particular duration.
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("never-answered.txt"),
        ask_settings(),
        Duration::from_secs(1),
    )
    .await;

    // Nobody ever calls `session.respondToPrompt`. Without the daemon-side
    // timeout this call never returns — the engine waits on its oneshot
    // forever — so the outer timeout is the regression guard.
    let (frames, response) = tokio::time::timeout(
        Duration::from_secs(30),
        rpc_streaming(
            &srv.sock,
            r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file"},"id":1}"#,
        ),
    )
    .await
    .expect("an unanswered permission prompt hung the turn forever");

    assert!(
        !events_of_kind(&frames, "prompt").is_empty(),
        "the prompt should still have been offered: {frames:?}"
    );
    assert!(response["result"].is_object(), "turn failed: {response}");
    let rendered = serde_json::to_string(&events_of_kind(&frames, "tool_result")).unwrap();
    assert!(
        rendered.contains("Denied by permission") && rendered.contains("no answer"),
        "the timeout must fail closed, with a reason that names itself: {rendered}"
    );

    srv.stop().await;
}

/// A prompt raised by a turn can be answered **on the connection running
/// that turn**.
///
/// This is the case a browser has: one tab, one socket, everything on it.
/// While requests were served inline the connection read nothing during a
/// turn, so the answer never arrived — the turn waited for an answer that
/// waited for the turn, until the prompt timeout denied it. Every other test
/// in this file answers from a second connection and so never saw it.
///
/// The whole test is under a deadline: a regression here does not fail, it
/// hangs until `permission_prompt_timeout`.
#[tokio::test]
async fn a_prompt_is_answerable_on_the_same_connection_that_started_the_turn() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("same-conn.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        let mut client = crate::support::begin(
            &srv.sock,
            "session.run_turn",
            serde_json::json!({"message": "make a file"}),
        )
        .await;
        let mut answered = false;
        loop {
            let v = client
                .next_frame()
                .await
                .unwrap()
                .expect("connection closed before the turn finished");

            if v["params"]["event"]["kind"] == "prompt" && !answered {
                let sid = v["params"]["session_id"].as_str().unwrap();
                let prompt_id = v["params"]["event"]["prompt_id"].as_str().unwrap();
                // Same socket the turn is streaming on.
                client
                    .send_request(
                        "session.respondToPrompt",
                        serde_json::json!({"session_id": sid, "prompt_id": prompt_id,
                            "decision": {"type": "deny", "reason": "same-connection test"}}),
                    )
                    .await
                    .unwrap();
                answered = true;
            }

            // The turn's own response, id 1, means the turn ran to an end
            // rather than stalling on an unanswerable question.
            if v["id"] == 1 {
                assert!(answered, "the turn ended without ever asking: {v}");
                return v;
            }
        }
    })
    .await;

    let response = outcome.expect(
        "the turn never finished — the connection is not reading while it runs, so its own \
         prompt cannot be answered",
    );
    assert!(response["result"].is_object(), "turn failed: {response}");

    srv.stop().await;
}
