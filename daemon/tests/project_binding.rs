//! A session bound to a project (or to none): which settings, memory, skills, agents and scripts it sees and where its transcript lands.

use std::time::Duration;

use crate::support::*;

/// A session bound to a project other than the pool's default must use
/// *that* project's merged settings — proven functionally (the model name
/// actually sent to the API), not just via the Meta stamp.
#[tokio::test]
async fn session_create_binds_to_a_different_project_with_its_own_settings() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("hi")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(other_project.join(".agix")).unwrap();
    std::fs::write(
        other_project.join(".agix").join("settings.json"),
        r#"{"model": {"effort": "low"}, "memory": {"enabled": false}}"#,
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = match created["result"]["session_id"].as_str() {
        Some(s) => s.to_string(),
        None => panic!("session.create should succeed: {created}"),
    };
    assert_eq!(
        created["result"]["project_root"],
        other_project.display().to_string()
    );
    assert_eq!(created["result"]["scene"], "code");
    assert_eq!(created["result"]["session_kind"], "primary");

    let entries = srv
        .store
        .load(base::session::SessionId::parse(&sid).unwrap())
        .await
        .unwrap();
    match &entries[0].entry {
        history::entry::LogEntry::Meta { project_root, .. } => {
            assert_eq!(
                project_root.as_deref(),
                Some(other_project.display().to_string().as_str())
            );
        }
        other => panic!("expected Meta as the first entry, got {other:?}"),
    }

    run_turn(&srv.sock, Some(&sid), "go").await;
    let sent_effort = seen.lock().unwrap().last().unwrap().effort;
    assert_eq!(
        sent_effort,
        Some(base::interface::model::Effort::Low),
        "the turn must have used the project's own settings, not the pool default"
    );

    srv.stop().await;
}

#[tokio::test]
async fn a_project_bound_sessions_memories_land_in_its_own_project() {
    // The scripted client repeats its last round forever, so the background
    // memory extraction that runs after the turn gets this same array back and
    // has something to persist. `type: "project"` is what routes a memory to
    // the project-local directory rather than the global one.
    let extracted = r#"[{"name":"merge-freeze","description":"freeze window","content":"Thursday.","type":"project","confidence":0.9}]"#;
    let (srv, _seen) =
        start_scripted_server(vec![text_round(extracted)], ask_settings(), Duration::ZERO).await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(&other_project).unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&sid), "remember the freeze window").await;

    let own = other_project
        .join(".agix")
        .join("state")
        .join("memory")
        .join("merge-freeze.md");
    let pools = srv
        ._dir
        .path()
        .join("work")
        .join(".agix")
        .join("state")
        .join("memory")
        .join("merge-freeze.md");
    for _ in 0..60 {
        if own.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        own.exists(),
        "a project memory belongs to the session's own project, not the daemon's"
    );
    assert!(
        !pools.exists(),
        "the daemon's own project must not collect other projects' memories"
    );

    srv.stop().await;
}

#[tokio::test]
async fn a_project_bound_sessions_transcript_lands_in_that_projects_directory() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(&other_project).unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&sid), "first").await;

    let own = transcript_dir(srv._dir.path(), Some(&other_project)).join(format!("{sid}.jsonl"));
    let pools = transcript_dir(srv._dir.path(), Some(&srv._dir.path().join("work")))
        .join(format!("{sid}.jsonl"));
    assert!(
        own.exists(),
        "a session's transcript belongs to its own project, not the daemon's"
    );
    assert!(!pools.exists(), "and not to both");

    // Still listed: `session.list` enumerates every project this daemon has
    // served, which is what keeps a cold session findable after the move.
    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    let listed = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.list","id":3}"#,
    )
    .await;
    let entry = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == sid.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("cold session missing from the list: {listed}"));
    assert_eq!(entry["status"], "inactive", "{entry}");

    srv.stop().await;
}

#[tokio::test]
async fn closing_a_project_bound_parent_still_cascades_to_its_sidechains() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(&other_project).unwrap();
    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let parent = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&parent), "root").await;
    let child = inject_sidechain(&srv, &parent).await;

    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{parent}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(
        closed["result"]["sidechains_deleted"], 1,
        "a sidechain is found in its parent's project, not the daemon's: {closed}"
    );
    assert!(srv.store.load(child).await.is_err());

    srv.stop().await;
}

#[tokio::test]
async fn a_project_bound_session_sees_that_projects_own_skills() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("a"), text_round("b")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let other_project = srv._dir.path().join("other-project");
    let skill_dir = other_project
        .join(".agents")
        .join("skills")
        .join("freeze-check");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: freeze-check\ndescription: Check the merge freeze window\n---\nbody",
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&sid), "anything").await;

    // The last request that carried a system prompt: the background memory
    // extraction fires a request of its own after every turn, and it sends
    // none, so `last()` alone would be reading the wrong one. The skills
    // inventory opens that request's conversation.
    let opening_of_last = |seen: &SeenRequests| {
        let seen = seen.lock().unwrap();
        let req = seen
            .iter()
            .rev()
            .find(|req| !req.system.is_empty())
            .expect("some request carried a system prompt");
        serde_json::to_string(&req.messages[0]).unwrap()
    };
    assert!(
        opening_of_last(&seen).contains("freeze-check"),
        "a session must see the skills of the project it is bound to"
    );

    // And the daemon's own project does not acquire them.
    run_turn(&srv.sock, None, "anything").await;
    assert!(
        !opening_of_last(&seen).contains("freeze-check"),
        "one project's skills must not leak into another's sessions"
    );

    srv.stop().await;
}

#[tokio::test]
async fn another_projects_settings_change_reaches_the_daemon_without_a_reload() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("a")], ask_settings(), Duration::ZERO).await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(other_project.join(".agix")).unwrap();
    let settings_file = other_project.join(".agix").join("settings.json");
    std::fs::write(&settings_file, r#"{"model": {"effort": "low"}}"#).unwrap();

    // Reading the effective settings is what builds (and caches) this
    // project's binding — the state the file change has to invalidate.
    let effective = |root: &std::path::Path| {
        format!(
            r#"{{"jsonrpc":"2.0","method":"config.get","params":{{"scene":"code","project_root":"{}","tier":"effective"}},"id":1}}"#,
            root.display()
        )
    };
    let first = rpc(&srv.sock, &effective(&other_project)).await;
    assert_eq!(
        first["result"]["settings"]["model"]["effort"], "low",
        "{first}"
    );

    std::fs::write(&settings_file, r#"{"model": {"effort": "max"}}"#).unwrap();

    let mut seen_second = false;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = rpc(&srv.sock, &effective(&other_project)).await;
        if now["result"]["settings"]["model"]["effort"] == "max" {
            seen_second = true;
            break;
        }
    }
    assert!(
        seen_second,
        "a project this daemon serves must not need a restart to pick up its own settings"
    );

    srv.stop().await;
}

#[tokio::test]
async fn a_project_bound_session_sees_that_projects_own_agent_types() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("a"), text_round("b")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let other_project = srv._dir.path().join("other-project");
    let agents_dir = other_project.join(".agix").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("freeze-bot.md"),
        "---\ndescription: Watches the merge freeze\n---\nbody",
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&sid), "anything").await;

    // The catalog is rendered into the `Agent` tool's own description.
    let agent_tool_description = |seen: &SeenRequests| {
        seen.lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|req| {
                req.tools
                    .iter()
                    .find(|t| t.name == "Agent")
                    .map(|t| t.description.clone())
            })
            .expect("some request offered the Agent tool")
    };
    assert!(
        agent_tool_description(&seen).contains("freeze-bot"),
        "a session must see the agent types of the project it is bound to"
    );

    run_turn(&srv.sock, None, "anything").await;
    assert!(
        !agent_tool_description(&seen).contains("freeze-bot"),
        "one project's agent types must not leak into another's sessions"
    );

    srv.stop().await;
}

/// A project that binds a script gets it, through the daemon, on the prompt
/// the daemon sends.
///
/// The harness that covers the nine extension points drives `Builder`
/// directly, which is the right place to ask what an adapter does and the
/// wrong place to ask whether a *session* ever gets one: everything between a
/// `scripts` section on disk and an adapter installed on a session belongs to
/// the daemon — parsing it, resolving it against a root, binding it once per
/// session. None of that had a test.
///
/// Two claims in one case, because they fail together. The mark proves the
/// script ran at all. The rewrite proves the daemon judged it the operator's
/// own code: authority follows the file's location, so a script bound against
/// the wrong root is a script demoted to add-only — which looks exactly like a
/// script that chose not to rewrite anything.
#[cfg(feature = "scripts")]
#[tokio::test]
async fn a_projects_own_script_reaches_the_prompt_the_daemon_sends() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("hi")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let project = srv._dir.path().join("scripted-project");
    crate::support::trust_projects_under(srv._dir.path(), &project);
    std::fs::create_dir_all(project.join(".agix").join("scripts")).unwrap();
    std::fs::write(
        project.join(".agix").join("scripts").join("house.js"),
        // Adds one block and rewrites every block already there. The rewrite
        // is the half only the operator's own code may do.
        r#"function onAssemble(blocks) {
             var out = blocks.map(function (b) {
               return { name: b.name, content: b.content + "\nDAEMON-SCRIPT-EDIT" };
             });
             out.push({ name: "daemon.house", content: "DAEMON-SCRIPT-ADD" });
             return out;
           }"#,
    )
    .unwrap();
    // `timeout_ms` well above the carrier's 100ms default, because the
    // subject here is whether a project's script reaches the prompt at all.
    // The default budget is wall clock, and on a machine running the rest of
    // this suite beside it even this script can miss it — the engine then
    // does exactly what it promises, drops the contribution and leaves the
    // prompt alone, and the case fails for a reason it is not about. The
    // deadline itself is covered where it belongs, by a script written to
    // exceed it (`tests/runner/tests/script_boundaries.rs`).
    std::fs::write(
        project.join(".agix").join("settings.json"),
        r#"{
             "memory": {"enabled": false},
             "scripts": [
               {"path": ".agix/scripts/house.js", "point": "prompt.assemble",
                "entry": "onAssemble", "timeout_ms": 5000}
             ]
           }"#,
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session.create failed: {created}"))
        .to_string();

    run_turn(&srv.sock, Some(&sid), "hello").await;

    // Marks only: a failure here otherwise prints the whole system prompt,
    // which is a screenful of noise around one missing word.
    let sent = {
        let requests = seen.lock().unwrap();
        let last = requests
            .last()
            .expect("the turn must have called the model");
        format!("{:?}", last.system)
    };
    assert!(
        sent.contains("DAEMON-SCRIPT-ADD"),
        "the project's script never reached the prompt the daemon sent — \
         nothing between the settings file and the session installed it"
    );
    assert!(
        sent.contains("DAEMON-SCRIPT-EDIT"),
        "the script was demoted to add-only, so the daemon judged the \
         project's own file to be from outside it"
    );

    srv.stop().await;
}

/// `session.get` says what this session's scripts have been doing.
///
/// The failure mode of the whole script tier is a script that quietly does
/// nothing, and until this the only account of it was a line on the daemon's
/// stderr — which the person asking "why is my script not working" does not
/// have. A throwing script is the case worth reporting: nothing about the
/// session looks different, and the answer has to come from somewhere.
#[cfg(feature = "scripts")]
#[tokio::test]
async fn session_get_reports_what_the_scripts_did() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("hi")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let project = srv._dir.path().join("reported-project");
    crate::support::trust_projects_under(srv._dir.path(), &project);
    std::fs::create_dir_all(project.join(".agix").join("scripts")).unwrap();
    std::fs::write(
        project.join(".agix").join("scripts").join("broken.js"),
        "function onAssemble() { throw new Error('nope'); }",
    )
    .unwrap();
    // Generous on purpose: this case is about a script that *throws* being
    // reported, and a script that timed out would be reported as failed too
    // — passing for the wrong reason.
    std::fs::write(
        project.join(".agix").join("settings.json"),
        r#"{
             "memory": {"enabled": false},
             "scripts": [
               {"path": ".agix/scripts/broken.js", "point": "prompt.assemble",
                "entry": "onAssemble", "timeout_ms": 5000}
             ]
           }"#,
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session.create failed: {created}"))
        .to_string();
    run_turn(&srv.sock, Some(&sid), "hello").await;

    let detail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    let scripts = &detail["result"]["scripts"];
    assert!(
        scripts.is_object(),
        "a session that bound a script must report on it: {detail}"
    );
    assert!(
        scripts["failed"].as_u64().unwrap_or(0) >= 1,
        "the throwing script is not reported as having failed: {scripts}"
    );
    let recent = scripts["recent"].as_array().expect("recent is an array");
    let first = &recent[0];
    assert_eq!(first["point"], "prompt.assemble");
    assert_eq!(first["outcome"], "failed");
    assert!(
        first["detail"].as_str().is_some_and(|d| !d.is_empty()),
        "a failure without a reason sends the reader nowhere: {first}"
    );

    srv.stop().await;
}

/// A session that bound none has no key at all — absent and empty are
/// different answers, and the difference is the first thing a reader needs.
#[cfg(feature = "scripts")]
#[tokio::test]
async fn session_get_says_nothing_about_scripts_when_there_are_none() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("hi")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let sid = run_turn(&srv.sock, None, "hello").await;
    let detail = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.get","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert!(
        detail["result"]["scripts"].is_null(),
        "a session with no scripts must not report an empty ledger: {detail}"
    );
    srv.stop().await;
}

/// A binding the daemon cannot honor costs the session every script, not some
/// of them.
///
/// `bind` refuses the set and the daemon logs it; what matters out here is
/// that the session still starts and runs, carrying none of them. A daemon
/// that failed the session instead would turn one typo in a settings file
/// into an agent that will not start.
#[cfg(feature = "scripts")]
#[tokio::test]
async fn one_bad_binding_leaves_the_session_running_with_no_scripts() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("hi")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let project = srv._dir.path().join("half-scripted-project");
    std::fs::create_dir_all(project.join(".agix").join("scripts")).unwrap();
    std::fs::write(
        project.join(".agix").join("scripts").join("house.js"),
        r#"function onAssemble(blocks) {
             blocks.push({ name: "daemon.house", content: "DAEMON-SCRIPT-ADD" });
             return blocks;
           }"#,
    )
    .unwrap();
    std::fs::write(
        project.join(".agix").join("settings.json"),
        r#"{
             "memory": {"enabled": false},
             "scripts": [
               {"path": ".agix/scripts/house.js", "point": "prompt.assemble",
                "entry": "onAssemble", "timeout_ms": 5000},
               {"path": ".agix/scripts/missing.js", "point": "tool.result",
                "entry": "onResult", "timeout_ms": 5000}
             ]
           }"#,
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("session.create failed: {created}"))
        .to_string();

    run_turn(&srv.sock, Some(&sid), "hello").await;

    let sent = {
        let requests = seen.lock().unwrap();
        format!("{:?}", requests.last().expect("the turn ran").system)
    };
    assert!(
        !sent.contains("DAEMON-SCRIPT-ADD"),
        "the good binding was applied although another one could not be \
         honored — half a configuration is one nobody wrote"
    );

    srv.stop().await;
}

/// A project-bound session must still use *its own*
/// project settings after being dropped from memory and reattached — not
/// silently fall back to the pool default the moment it needs rebuilding.
#[tokio::test]
async fn a_project_bound_session_keeps_its_own_settings_across_close_and_resume() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("a"), text_round("b")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let other_project = srv._dir.path().join("other-project");
    std::fs::create_dir_all(other_project.join(".agix")).unwrap();
    std::fs::write(
        other_project.join(".agix").join("settings.json"),
        r#"{"model": {"effort": "low"}, "memory": {"enabled": false}}"#,
    )
    .unwrap();

    let created = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.create","params":{{"project_root":"{}"}},"id":1}}"#,
            other_project.display()
        ),
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_turn(&srv.sock, Some(&sid), "first").await;

    let closed = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(closed["result"]["closed"], sid);

    run_turn(&srv.sock, Some(&sid), "second, after reattach").await;
    let sent_effort = seen.lock().unwrap().last().unwrap().effort;
    assert_eq!(
        sent_effort,
        Some(base::interface::model::Effort::Low),
        "resuming must rebuild against the session's own recorded project, not the pool default"
    );

    srv.stop().await;
}
