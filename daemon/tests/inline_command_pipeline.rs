//! A skill's `` !`cmd` `` is one `Bash` call of the session, dispatched the way
//! the model's own calls are: whatever sits on that dispatch — a `tool.result`
//! script, telemetry, the nested-repository baseline — sees it too, whether
//! the model invokes the skill or the user types `/name`.

#![cfg(feature = "skills")]

use crate::support::call;
use crate::support::{text_round, tool_round};
use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// A daemon serving sessions in `work` on the global `settings`, with one
/// user skill, `probe`, whose body is `body`.
async fn serve_with_probe_skill(
    dir: &std::path::Path,
    work: &std::path::Path,
    settings: Value,
    body: &str,
) -> (std::path::PathBuf, Arc<MockAnthropicClient>) {
    let global = dir.join("agix");
    let skill = global.join("skills").join("probe");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(global.join("settings.json"), settings.to_string()).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: probe\ndescription: probes\n---\n{body}\n"),
    )
    .unwrap();
    let sock = dir.join("agix.sock");
    let paths =
        StaticDaemonPaths::with_project(global.clone(), work.to_path_buf()).with_global(global);
    let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
    config.settings.values.memory_enabled = false;
    let model = Arc::new(MockAnthropicClient::new());
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(work.to_path_buf()),
            model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
            transcripts: daemon::Transcripts::Nowhere,
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let listener = DaemonServer::bind_unix(&sock).unwrap();
    tokio::spawn(async move {
        let _ = server.serve_unix_listener(listener).await;
    });
    (sock, model)
}

fn request_text(model: &MockAnthropicClient, n: usize) -> String {
    serde_json::to_string(&model.nth_request(n).expect("the model was asked")).unwrap()
}

/// The lines of a telemetry file once one of them satisfies `found`.
async fn telemetry_line(path: &std::path::Path, found: impl Fn(&str) -> bool) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if let Some(line) = text.lines().find(|l| found(l)) {
            return line.to_string();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no such telemetry line: {text}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A `tool.result` script that redacts a marker sees an inline command's
/// output before it is spliced into the skill — both when the user types
/// `/probe` and when the model invokes the skill — and the command is
/// recorded in telemetry as a `Bash` call under an id derived from the
/// expansion's.
#[cfg(feature = "scripts")]
#[tokio::test]
async fn an_inline_commands_output_goes_through_the_result_transformers() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let script = dir.path().join("redact.js");
    std::fs::write(
        &script,
        r#"function onResult(r) { return r.text.split("INLINE-SECRET").join("[redacted]"); }"#,
    )
    .unwrap();
    let (sock, model) = serve_with_probe_skill(
        dir.path(),
        &work,
        json!({
            "sandbox": {"dangerously_disable_sandbox": true},
            "permission_rules": [{"tool": "Bash", "action": "allow"}],
            "scripts": [{"path": script, "point": "tool.result", "entry": "onResult",
                         "timeout_ms": 5000}],
        }),
        "Marker: !`echo INLINE\"\"-SECRET-u11`",
    )
    .await;
    let telemetry = dir.path().join("telemetry.jsonl");
    let session = call(
        &sock,
        "session.create",
        json!({"options": {"telemetry": {"output": telemetry}}}),
    )
    .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    model.push_turn(text_round("done"));
    call(
        &sock,
        "session.run_turn",
        json!({"session_id": session, "message": "/probe"}),
    )
    .await;
    let sent = request_text(&model, 0);
    assert!(sent.contains("Marker: [redacted]-u11"), "{sent}");
    assert!(!sent.contains("INLINE-SECRET"), "{sent}");

    let execution = telemetry_line(&telemetry, |l| {
        l.contains("tool_execution") && l.contains(r#""tool_use_id":"slash-"#)
    })
    .await;
    assert!(execution.contains(r#""tool_name":"Bash""#), "{execution}");
    telemetry_line(&telemetry, |l| {
        l.contains("tool_start") && l.contains(r#""tool_use_id":"slash-"#)
    })
    .await;

    let before = model.calls();
    model.push_turn(tool_round("Skill", "call-skill", json!({"skill": "probe"})));
    model.push_turn(text_round("done"));
    call(
        &sock,
        "session.run_turn",
        json!({"session_id": session, "message": "use the probe skill"}),
    )
    .await;
    let later = request_text(&model, before + 1);
    assert!(later.contains("Marker: [redacted]-u11"), "{later}");
    assert!(!later.contains("INLINE-SECRET"), "{later}");
    telemetry_line(&telemetry, |l| {
        l.contains("tool_execution") && l.contains(r#""tool_use_id":"call-skill."#)
    })
    .await;
}

fn host_git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(["-c", "user.email=t@example.com", "-c", "user.name=T"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

/// The user's project already holds a gitlinked repository whose
/// configuration names a program. A session's very first input is `/probe`,
/// whose inline command a rule lets through: the repository was there before
/// the session's first call, so it is the user's and stays where it is.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_slash_commands_inline_command_leaves_the_users_nested_repository_alone() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let work = dir.join("work");
    let sub = work.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    for (at, args) in [
        (&work, &["init", "-q"][..]),
        (&sub, &["init", "-q"][..]),
        (&sub, &["commit", "-q", "--allow-empty", "-m", "s"][..]),
        (&sub, &["config", "core.fsmonitor", "./x.sh"][..]),
        (&work, &["add", "sub"][..]),
        (&work, &["commit", "-qm", "with sub"][..]),
    ] {
        let out = host_git(at, args);
        assert!(out.status.success(), "{args:?}: {out:?}");
    }
    let (sock, model) = serve_with_probe_skill(
        &dir,
        &work,
        json!({ "permission_rules": [{"tool": "Bash", "action": "allow"}] }),
        "Ran: !`touch inline-ran`",
    )
    .await;

    model.push_turn(text_round("done"));
    call(&sock, "session.run_turn", json!({"message": "/probe"})).await;

    let sent = request_text(&model, 0);
    assert!(work.join("inline-ran").exists(), "the command did not run");
    assert!(!sent.contains("Moved aside"), "{sent}");
    assert!(
        sub.join(".git").is_dir(),
        "the user's own nested repository was moved aside: {sent}"
    );
}
