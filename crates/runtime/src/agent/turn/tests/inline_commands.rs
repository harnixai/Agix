//! 技能内联命令：经会话的分发，按一次 `Bash` 调用判与跑。

use crate::turn::tool_env::tool_context_for;
use crate::turn::tool_exec::{Caller, ToolExecCtx};
use base::interface::tool_dispatch::SubCallAnswer;
use base::permission::{PermissionRule, RuleBehavior, RuleSource};
use std::sync::Arc;

fn bash_rule(behavior: RuleBehavior, content: Option<&str>) -> PermissionRule {
    PermissionRule {
        source: RuleSource::ProjectSettings,
        behavior,
        tool_name: "Bash".into(),
        rule_content: content.map(Into::into),
    }
}

/// A turn's context in `cwd` whose tools are the real `Bash` (or none), judged
/// by the session's real policy under `rules`.
fn turn_in(cwd: &std::path::Path, rules: Vec<PermissionRule>, with_bash: bool) -> ToolExecCtx {
    let tools = base::tool::InMemoryToolRegistry::new();
    if with_bash {
        tools.register(Arc::new(tools::bash::BashTool));
    }
    let mut config = base::context::EngineConfig::defaults_for("test");
    std::sync::Arc::make_mut(&mut config.sandbox).disabled = true;
    let policy = permissions::policy::Policy::new(permissions::ruleset::RuleSet::new(rules));
    let exec = crate::exec::providers_for(&config, None);
    ToolExecCtx {
        tool_use_id: "toolu_skill".into(),
        tools: Arc::new(tools),
        cwd: cwd.to_path_buf(),
        project_root: Some(cwd.to_path_buf()),
        session_id: "test-session".into(),
        turn_no: 1,
        agent_depth: 0,
        telemetry_handle: telemetry::TelemetryHandle::noop(),
        turn_id: "test-turn".into(),
        cancel: tokio_util::sync::CancellationToken::new(),
        hooks: Arc::new(hooks::HookRunner::new(
            Default::default(),
            hooks::HookExec {
                process: exec.process.clone(),
                network: exec.network.clone(),
            },
        )),
        discontinued: Arc::default(),
        engine_git: Arc::new(config.sandbox.engine_git(&exec, config.project_trust, None)),
        exec,
        config: Arc::new(config),
        permission: base::interface::permission::PermissionSet::without_grants(Arc::new(policy)),
        session_state: Arc::new(session_state::SessionState::for_tests(cwd.to_path_buf())),
        elicitation: Arc::new(base::interface::elicitation::DeclineAll),
        host_calls: None,
        host_tools: Arc::default(),
        tool_middleware: Arc::default(),
        result_transformers: Arc::default(),
        images: Arc::default(),
        caller: Caller::Model,
        events: None,
        turn: Default::default(),
        checkpoints: None,
    }
}

async fn run(ctx: &ToolExecCtx, command: &str) -> Result<SubCallAnswer, String> {
    let call = tool_context_for(ctx, None);
    call.inline_commands
        .expect("every call's context runs inline commands")
        .run_bash(command, call.cancel.clone())
        .await
}

#[tokio::test]
async fn an_inline_command_is_held_to_the_rules_a_bash_call_would_be() {
    let dir = tempfile::tempdir().unwrap();
    let command = "echo stamped > stamp.txt";

    let denied = run(
        &turn_in(dir.path(), vec![bash_rule(RuleBehavior::Deny, None)], true),
        command,
    )
    .await
    .unwrap_err();
    assert!(denied.contains("denied by rule"), "{denied}");
    assert!(!dir.path().join("stamp.txt").exists());

    let unasked = run(&turn_in(dir.path(), vec![], true), command)
        .await
        .unwrap_err();
    assert!(unasked.contains("needs the user's approval"), "{unasked}");
    assert!(!dir.path().join("stamp.txt").exists());

    let allowed = vec![bash_rule(RuleBehavior::Allow, Some("echo:*"))];
    run(&turn_in(dir.path(), allowed, true), command)
        .await
        .unwrap();
    assert!(dir.path().join("stamp.txt").exists());
}

#[tokio::test]
async fn an_inline_command_is_not_run_in_a_session_without_bash() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = turn_in(
        dir.path(),
        vec![bash_rule(RuleBehavior::Allow, None)],
        false,
    );
    let refused = run(&ctx, "echo ran > ran.txt").await.unwrap_err();
    assert_eq!(refused, "Tool not found: Bash");
    assert!(!dir.path().join("ran.txt").exists());
}

#[tokio::test]
async fn a_read_only_inline_command_runs_as_a_bash_call_would() {
    let dir = tempfile::tempdir().unwrap();
    let answer = run(&turn_in(dir.path(), vec![], true), "echo hello")
        .await
        .unwrap();
    assert_eq!(answer.text.trim_end(), "hello");
}

/// A repository an inline command nests in the project with configuration
/// that names a program is moved aside before the user's own `git` can enter
/// it, and the answer says so.
#[tokio::test]
async fn a_repository_an_inline_command_nests_in_the_project_is_moved_aside() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-c", "user.email=t@example.com", "-c", "user.name=T"])
            .args(args)
            .current_dir(&project)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    };
    git(&["init", "-q"]);
    std::fs::write(project.join("a"), "a").unwrap();
    git(&["add", "a"]);
    git(&["commit", "-qm", "first"]);
    let command = "mkdir sub && cd sub && git init -q && touch f && git add f && \
                   git -c user.email=t@example.com -c user.name=T commit -q -m s && \
                   git config core.fsmonitor ./x.sh && cd .. && git add sub && echo planted";

    let ctx = turn_in(&project, vec![bash_rule(RuleBehavior::Allow, None)], true);
    let out = run(&ctx, command).await.unwrap().text;

    assert!(out.starts_with("planted"), "{out}");
    assert!(
        out.contains("core.fsmonitor") && out.contains("Moved aside"),
        "{out}"
    );
    assert!(!project.join("sub/.git").exists(), "{out}");
}

/// Each command is a call of its own, under an id derived from the call that
/// expanded it, and is recorded as one; the host hears no progress for an id
/// it never saw.
#[tokio::test]
async fn each_inline_command_is_a_bash_call_under_a_derived_id() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = turn_in(dir.path(), vec![], true);
    ctx.telemetry_handle = telemetry::TelemetryHandle::new(tx);
    ctx.events = Some(crate::event_bus::EventBus::new(events_tx));
    let call = tool_context_for(&ctx, None);
    let commands = call.inline_commands.clone().unwrap();
    commands
        .run_bash("echo one", call.cancel.clone())
        .await
        .unwrap();
    commands
        .run_bash("echo two", call.cancel.clone())
        .await
        .unwrap();
    drop((commands, call, ctx));

    let mut executed = Vec::new();
    while let Some(event) = rx.recv().await {
        let event = serde_json::to_value(&event).unwrap();
        if event.to_string().contains("tool_execution") {
            executed.push(event.to_string());
        }
    }
    assert_eq!(executed.len(), 2, "{executed:?}");
    assert!(
        executed[0].contains(r#""tool_use_id":"toolu_skill.1""#),
        "{executed:?}"
    );
    assert!(
        executed[1].contains(r#""tool_use_id":"toolu_skill.2""#),
        "{executed:?}"
    );
    assert!(executed.iter().all(|e| e.contains(r#""tool_name":"Bash""#)));
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, base::event::AgentEvent::ToolProgress { .. }),
            "{event:?}"
        );
    }
}
