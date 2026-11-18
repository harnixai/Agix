//! 写文件的工具写到权限判过的那个位置：判定之后才换上的符号链接不把写入带走。

#![cfg(unix)]

use super::tool_exec::{local_hook_runner, test_exec_ctx_with_permission};
use crate::turn::tool_exec::{execute_tool_with_telemetry, ToolExecCtx};
use base::context::Session;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Lets every call through, and swaps a link in once it has decided.
struct PermitThenSwap(Box<dyn Fn() + Send + Sync>);
#[async_trait::async_trait]
impl base::interface::permission::PermissionPolicy for PermitThenSwap {
    async fn decide(
        &self,
        _req: &base::interface::permission::PermissionRequest<'_>,
    ) -> base::interface::permission::PermissionOutcome {
        (self.0)();
        base::interface::permission::PermissionOutcome::Permit {
            basis: base::interface::permission::Basis::Person,
            profile: Default::default(),
            obligations: Default::default(),
        }
    }
}

struct Scene {
    _dir: tempfile::TempDir,
    project: PathBuf,
    outside: PathBuf,
}

fn scene() -> Scene {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (project, outside) = (root.join("proj"), root.join("outside"));
    std::fs::create_dir_all(project.join("sub")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    Scene {
        _dir: dir,
        project,
        outside,
    }
}

fn ctx_in(project: &Path, swap: impl Fn() + Send + Sync + 'static) -> ToolExecCtx {
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(Arc::new(tools::file_write::FileWriteTool));
    tools.register(Arc::new(tools::file_edit::FileEditTool));
    tools.register(Arc::new(tools::notebook_edit::NotebookEditTool));
    let hooks = Arc::new(local_hook_runner(Default::default()));
    let mut ctx =
        test_exec_ctx_with_permission(tools, hooks, Arc::new(PermitThenSwap(Box::new(swap))));
    ctx.cwd = project.to_path_buf();
    ctx.session_state = Arc::new(session_state::SessionState::for_tests(
        project.to_path_buf(),
    ));
    ctx
}

/// Moves `path` aside and puts a symlink to `to` where it was.
fn swap_for_link(path: PathBuf, to: PathBuf) -> impl Fn() + Send + Sync {
    move || {
        let aside = path.with_extension("moved");
        std::fs::rename(&path, &aside).unwrap();
        std::os::unix::fs::symlink(&to, &path).unwrap();
    }
}

#[tokio::test]
async fn a_directory_swapped_for_a_link_after_the_decision_does_not_take_a_write_along() {
    let s = scene();
    let ctx = ctx_in(
        &s.project,
        swap_for_link(s.project.join("sub"), s.outside.clone()),
    );
    let result = execute_tool_with_telemetry(
        &ctx,
        "Write",
        serde_json::json!({"file_path": "sub/new.txt", "content": "planted"}),
    )
    .await;
    assert!(
        !s.outside.join("new.txt").exists(),
        "the write followed the link: {result:?}"
    );
    assert!(result.is_err(), "{result:?}");
}

#[tokio::test]
async fn a_file_swapped_for_a_link_after_the_decision_does_not_take_an_edit_along() {
    let s = scene();
    let target = s.project.join("notes.txt");
    std::fs::write(&target, "before").unwrap();
    std::fs::write(s.outside.join("victim.txt"), "before").unwrap();
    let ctx = ctx_in(
        &s.project,
        swap_for_link(target.clone(), s.outside.join("victim.txt")),
    );
    ctx.session_state
        .record_read_with_range(&target, None, None);
    let result = execute_tool_with_telemetry(
        &ctx,
        "Edit",
        serde_json::json!({"file_path": "notes.txt", "old_string": "before", "new_string": "after"}),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(s.outside.join("victim.txt")).unwrap(),
        "before",
        "the edit followed the link: {result:?}"
    );
    assert!(result.is_err(), "{result:?}");
}

#[tokio::test]
async fn a_directory_swapped_for_a_link_after_the_decision_does_not_take_a_notebook_edit_along() {
    let notebook = r#"{"cells": [{"cell_type": "code", "source": "x", "metadata": {}, "outputs": [], "execution_count": null}], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#;
    let s = scene();
    std::fs::write(s.project.join("sub/nb.ipynb"), notebook).unwrap();
    std::fs::write(s.outside.join("nb.ipynb"), notebook).unwrap();
    let ctx = ctx_in(
        &s.project,
        swap_for_link(s.project.join("sub"), s.outside.clone()),
    );
    let result = execute_tool_with_telemetry(
        &ctx,
        "NotebookEdit",
        serde_json::json!({"file_path": "sub/nb.ipynb", "mode": "edit", "cell_index": 0, "new_source": "planted"}),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(s.outside.join("nb.ipynb")).unwrap(),
        notebook,
        "the notebook edit followed the link: {result:?}"
    );
    assert!(result.is_err(), "{result:?}");
}

#[tokio::test]
async fn a_link_in_the_project_is_written_through_as_before() {
    let s = scene();
    std::fs::write(s.project.join("AGENTS.md"), "old").unwrap();
    std::os::unix::fs::symlink("AGENTS.md", s.project.join("CLAUDE.md")).unwrap();
    std::fs::create_dir_all(s.project.join("real")).unwrap();
    std::os::unix::fs::symlink("real", s.project.join("linked")).unwrap();
    let ctx = ctx_in(&s.project, || {});
    ctx.session_state
        .record_read_with_range(&s.project.join("CLAUDE.md"), None, None);

    execute_tool_with_telemetry(
        &ctx,
        "Edit",
        serde_json::json!({"file_path": "CLAUDE.md", "old_string": "old", "new_string": "new"}),
    )
    .await
    .unwrap();
    execute_tool_with_telemetry(
        &ctx,
        "Write",
        serde_json::json!({"file_path": "linked/deep/x.txt", "content": "x"}),
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(s.project.join("AGENTS.md")).unwrap(),
        "new"
    );
    assert!(s.project.join("CLAUDE.md").is_symlink());
    assert_eq!(
        std::fs::read_to_string(s.project.join("real/deep/x.txt")).unwrap(),
        "x"
    );
}
