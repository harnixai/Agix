//! What the file tools and `Bash` decide about paths outside the project:
//! read freely only under the readable roots, and in an unattended session run
//! a shell command only when the OS sandbox holds it.

use base::context::SessionEnv;
use base::interface::permission::Reach;
use base::interface::tool::{Tool, ToolContext};
use session_state::SessionState;
use std::path::Path;
use std::sync::Arc;

fn ctx_in(cwd: &Path, state: impl FnOnce(&mut SessionEnv)) -> ToolContext {
    let mut ctx = session_state::testing::tool_context_for_test(
        platform::local_providers(),
        cwd.to_path_buf(),
    );
    let mut env = SessionEnv::new(cwd.to_path_buf());
    state(&mut env);
    ctx.session = Arc::new(SessionState::for_tests_in(env));
    ctx.session
        .set_permission_mode(base::permission::PermissionMode::Default);
    ctx
}

/// The policy's word on a call made in `ctx`, in the mode `ctx` says.
async fn judged(
    tool: Arc<dyn Tool>,
    input: &serde_json::Value,
    ctx: &ToolContext,
) -> base::interface::permission::PermissionOutcome {
    use base::interface::permission::{PermissionPolicy, PermissionRequest, ToolFacts};
    let facts = ToolFacts::gather(tool.as_ref(), input, ctx).await;
    permissions::policy::Policy::empty()
        .decide(&PermissionRequest::new(
            tool.name(),
            Some(&tool),
            input,
            &facts,
            ctx,
            &ctx.session,
        ))
        .await
}

async fn inside(tool: &dyn Tool, input: serde_json::Value, ctx: &ToolContext) -> bool {
    tool.permission_facts(&input, ctx).await.reach == Reach::Inside
}

async fn reads(ctx: &ToolContext, dir: &Path) -> [bool; 3] {
    let file = dir.join("notes.txt").display().to_string();
    let dir = dir.display().to_string();
    use serde_json::json;
    [
        inside(
            &tools::file_read::FileReadTool,
            json!({ "file_path": file }),
            ctx,
        )
        .await,
        inside(
            &tools::glob::GlobTool,
            json!({ "pattern": "*", "path": dir }),
            ctx,
        )
        .await,
        inside(
            &tools::grep::GrepTool,
            json!({ "pattern": "x", "path": dir }),
            ctx,
        )
        .await,
    ]
}

#[tokio::test]
async fn reading_outside_the_project_needs_a_readable_root() {
    let project = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();

    let plain = ctx_in(project.path(), |_| {});
    assert_eq!(reads(&plain, project.path()).await, [true; 3]);
    assert_eq!(reads(&plain, elsewhere.path()).await, [false; 3]);

    let listed = ctx_in(project.path(), |s| {
        s.read_directories = vec![elsewhere.path().to_path_buf()]
    });
    assert_eq!(reads(&listed, elsewhere.path()).await, [true; 3]);

    let writable = ctx_in(project.path(), |s| {
        s.additional_writable_dirs = vec![elsewhere.path().to_path_buf()]
    });
    assert_eq!(reads(&writable, elsewhere.path()).await, [true; 3]);

    let anywhere = ctx_in(project.path(), |s| s.read_anywhere = true);
    assert_eq!(reads(&anywhere, elsewhere.path()).await, [true; 3]);
}

#[tokio::test]
async fn glob_and_grep_without_a_path_search_the_project() {
    let project = tempfile::tempdir().unwrap();
    let ctx = ctx_in(project.path(), |_| {});
    assert!(
        inside(
            &tools::glob::GlobTool,
            serde_json::json!({ "pattern": "*" }),
            &ctx
        )
        .await
    );
}

/// Whether the policy lets `python3 build.py` run with nobody attending.
async fn runs_unattended(ctx: &ToolContext) -> bool {
    use base::interface::permission::{Attendance, PermissionPolicy, PermissionRequest, ToolFacts};
    let mut ctx = ctx.clone();
    ctx.attendance = Attendance::Unattended;
    ctx.confinement = ctx
        .config
        .sandbox
        .confinement(&ctx.exec, &ctx.cwd, &ctx.session_id);
    let tool: Arc<dyn Tool> = Arc::new(tools::bash::BashTool);
    let input = serde_json::json!({ "command": "python3 build.py" });
    let facts = ToolFacts::gather(tool.as_ref(), &input, &ctx).await;
    permissions::policy::Policy::empty()
        .decide(
            &PermissionRequest::new("Bash", Some(&tool), &input, &facts, &ctx, &ctx.session)
                .with_attendance(Attendance::Unattended),
        )
        .await
        .is_permit()
}

#[tokio::test]
async fn an_unattended_command_runs_only_inside_the_sandbox() {
    let project = tempfile::tempdir().unwrap();

    let attended = ctx_in(project.path(), |_| {});
    let asked = judged(
        Arc::new(tools::bash::BashTool),
        &serde_json::json!({ "command": "python3 build.py" }),
        &attended,
    )
    .await;
    assert!(
        matches!(
            asked,
            base::interface::permission::PermissionOutcome::Prompt { .. }
        ),
        "{asked:?}"
    );

    let mut unconfined = ctx_in(project.path(), |s| s.unattended = true);
    unconfined.sandbox_mut().disabled = true;
    assert!(!runs_unattended(&unconfined).await);

    let mut unattended = ctx_in(project.path(), |s| s.unattended = true);
    unattended.sandbox_mut().disabled = false;
    let holds = tools::bash::sandbox_status().starts_with("available");
    assert_eq!(runs_unattended(&unattended).await, holds);

    // Asked to, the network has to be closed as well: the sandbox holds
    // writes, not the network.
    unattended.sandbox_mut().unattended_requires_network_policy = true;
    assert!(!runs_unattended(&unattended).await);
    unattended.sandbox_mut().network_mode = base::interface::exec::NetworkMode::DenyAll;
    assert_eq!(runs_unattended(&unattended).await, holds);
}

#[tokio::test]
async fn agent_configuration_is_confirmed_before_a_write_not_a_read() {
    for name in [".agix", ".codix"] {
        configuration_is_confirmed_in(&base::paths::Layout::new(name).unwrap(), name).await;
    }
}

/// Under `layout`, whose project directory `own` is.
#[allow(clippy::unwrap_used)]
async fn configuration_is_confirmed_in(layout: &base::paths::Layout, own: &str) {
    use base::interface::permission::{
        PermissionOutcome, PermissionPolicy, PermissionRequest, ToolFacts,
    };
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(own).join("agents")).unwrap();
    std::fs::write(project.path().join(own).join("agents/a.md"), "x").unwrap();
    let registry = Arc::new(base::interface::tool::InMemoryToolRegistry::new());
    registry.register(Arc::new(tools::file_read::FileReadTool));
    registry.register(Arc::new(tools::file_write::FileWriteTool));
    let gate = permissions::policy::Policy::from_settings(
        &settings::LoadedSettings::new(
            settings::Settings::defaults_for("m"),
            base::paths::PathSettings::default(),
        )
        .permission_settings(),
        [],
    );
    let session: Arc<dyn base::context::Session> = Arc::new(
        session_state::SessionState::for_tests(project.path().to_path_buf()),
    );
    let config = project
        .path()
        .join(own)
        .join("agents/a.md")
        .display()
        .to_string();
    let scratch = project
        .path()
        .join(own)
        .join("state/scratchpad/x.txt")
        .display()
        .to_string();
    let check = |tool: &'static str, input: serde_json::Value| {
        let (gate, session, found) = (&gate, &session, registry.get(tool));
        let cwd = project.path().to_path_buf();
        async move {
            let mut ctx =
                session_state::testing::tool_context_for_test(platform::local_providers(), cwd);
            ctx.session = session.clone();
            Arc::make_mut(&mut ctx.config).layout = layout.clone();
            let paths = base::paths::SensitivePaths::new(None, Some(&ctx.cwd), layout.clone());
            ctx.sandbox_mut().paths = Arc::new(paths);
            let facts = ToolFacts::gather(found.as_deref().unwrap(), &input, &ctx).await;
            gate.decide(&PermissionRequest::new(
                tool,
                found.as_ref(),
                &input,
                &facts,
                &ctx,
                session,
            ))
            .await
        }
    };

    let read = check("Read", serde_json::json!({ "file_path": config })).await;
    assert!(matches!(read, PermissionOutcome::Permit { .. }), "{read:?}");
    let new_config = project
        .path()
        .join(own)
        .join("agents/b.md")
        .display()
        .to_string();
    let write = check(
        "Write",
        serde_json::json!({ "file_path": new_config, "content": "y" }),
    )
    .await;
    assert!(
        matches!(write, PermissionOutcome::Prompt { .. }),
        "{write:?}"
    );
    let scratch = check(
        "Write",
        serde_json::json!({ "file_path": scratch, "content": "y" }),
    )
    .await;
    assert!(
        matches!(scratch, PermissionOutcome::Permit { .. }),
        "{scratch:?}"
    );
    let foreign = project
        .path()
        .join(".agix/hooks/x.sh")
        .display()
        .to_string();
    let foreign = check(
        "Write",
        serde_json::json!({ "file_path": foreign, "content": "y" }),
    )
    .await;
    assert!(
        matches!(foreign, PermissionOutcome::Prompt { .. }),
        "{own}: {foreign:?}"
    );
}

async fn accept_edits_allows_reading(ctx: &ToolContext, dir: &Path) -> [bool; 5] {
    let file = dir.join("notes.txt").display().to_string();
    let dir = dir.display().to_string();
    let calls: [(Arc<dyn Tool>, serde_json::Value); 5] = [
        (
            Arc::new(tools::file_read::FileReadTool),
            serde_json::json!({ "file_path": file }),
        ),
        (
            Arc::new(tools::glob::GlobTool),
            serde_json::json!({ "pattern": "*", "path": dir }),
        ),
        (
            Arc::new(tools::grep::GrepTool),
            serde_json::json!({ "pattern": "x", "path": dir }),
        ),
        (
            Arc::new(tools::bash::BashTool),
            serde_json::json!({ "command": format!("cat {file}") }),
        ),
        (
            Arc::new(tools::monitor::MonitorTool),
            serde_json::json!({ "command": format!("tail -f {file}"), "description": "watch" }),
        ),
    ];
    let mut out = [false; 5];
    for (slot, (tool, input)) in out.iter_mut().zip(calls) {
        let name = tool.name().to_string();
        let decision = judged(tool, &input, ctx).await;
        assert!(
            !matches!(
                decision,
                base::interface::permission::PermissionOutcome::Deny { .. }
            ),
            "{name}: {decision:?}"
        );
        *slot = decision.is_permit();
    }
    out
}

#[tokio::test]
async fn accept_edits_asks_before_reading_outside_the_readable_roots() {
    let project = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("notes.txt"), "x").unwrap();
    std::fs::write(elsewhere.path().join("notes.txt"), "x").unwrap();
    let accept_edits = |state: fn(&mut SessionEnv, &Path)| {
        let ctx = ctx_in(project.path(), |s| state(s, elsewhere.path()));
        ctx.session
            .set_permission_mode(base::permission::PermissionMode::AcceptEdits);
        ctx
    };

    let plain = accept_edits(|_, _| {});
    assert_eq!(
        accept_edits_allows_reading(&plain, project.path()).await,
        [true; 5]
    );
    assert_eq!(
        accept_edits_allows_reading(&plain, elsewhere.path()).await,
        [false; 5]
    );

    let listed = accept_edits(|s, dir| s.read_directories = vec![dir.to_path_buf()]);
    assert_eq!(
        accept_edits_allows_reading(&listed, elsewhere.path()).await,
        [true; 5]
    );

    let anywhere = accept_edits(|s, _| s.read_anywhere = true);
    assert_eq!(
        accept_edits_allows_reading(&anywhere, elsewhere.path()).await,
        [true; 5]
    );
}

#[tokio::test]
async fn accept_edits_asks_before_a_shell_read_of_the_home_directory() {
    let project = tempfile::tempdir().unwrap();
    let ctx = ctx_in(project.path(), |_| {});
    ctx.session
        .set_permission_mode(base::permission::PermissionMode::AcceptEdits);
    let decision = judged(
        Arc::new(tools::bash::BashTool),
        &serde_json::json!({ "command": "cat ~/.zshrc" }),
        &ctx,
    )
    .await;
    assert!(
        matches!(
            decision,
            base::interface::permission::PermissionOutcome::Prompt { .. }
        ),
        "{decision:?}"
    );
}
