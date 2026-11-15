//! The session's frozen environment: base's git and platform facts, plus what
//! needs the memory and prompt-files crates (AGENTS.md, memory index, output
//! style).

use base::engine_exec::EngineExec;
use base::frozen::{CollectOptions, FrozenContext};
use base::paths::{ConfigPaths, PromptFiles};
use std::path::PathBuf;

pub(crate) async fn collect(
    cwd: PathBuf,
    opts: CollectOptions,
    paths: &ConfigPaths,
    environment: &dyn base::interface::environment::Environment,
    engine_git: &EngineExec,
) -> FrozenContext {
    let mut ctx =
        FrozenContext::collect_environment(cwd, &opts, paths, environment, engine_git).await;
    let files = PromptFiles::for_project(paths);
    let project = (!opts.outside_project).then_some(ctx.cwd.clone());
    if let Some(project) = &project {
        ctx.memory_blocks =
            prompt_files::collect_agents_md(project, opts.walk_up_claude_md, paths, &files).await;
    }
    ctx.memory_index = memory::index_for_prompt(paths).await;
    ctx.output_style = match opts.output_style.as_deref() {
        Some(name) if !name.trim().is_empty() => {
            prompt_files::load_output_style(project.as_deref(), name, paths, &files)
        }
        _ => None,
    };
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    fn test_paths(root: &Path) -> ConfigPaths {
        ConfigPaths::new(root.join(".agix"), root.join(".agix"), "code")
    }

    fn no_git() -> EngineExec {
        EngineExec::new(
            base::interface::exec::ExecProviders::in_process(),
            Default::default(),
            Default::default(),
        )
    }

    #[tokio::test]
    async fn the_memory_index_comes_from_the_sessions_store() {
        let dir = TempDir::new().unwrap();
        let paths = test_paths(dir.path());
        memory::MemoryStore::for_paths(&paths)
            .persist_batch_scoped(vec![memory::ScopedMemory {
                memory: base::memory::DurableMemory {
                    name: "deploy-day".into(),
                    description: "deploys go out on Tuesdays".into(),
                    memory_type: base::memory::MemoryType::Project,
                    content: String::new(),
                    source_session_id: String::new(),
                    confidence: 0.9,
                    last_seen: String::new(),
                    recall_count: 0,
                },
                scope: memory::MemoryScope::Local,
            }])
            .unwrap();
        let ctx = collect(
            dir.path().to_path_buf(),
            CollectOptions::defaults(),
            &paths,
            &base::interface::environment::SystemEnvironment,
            &no_git(),
        )
        .await;
        assert_eq!(ctx.memory_dir, paths.global_memory_dir());
        assert!(ctx
            .memory_index
            .is_some_and(|index| index.contains("deploy-day")));
        assert!(
            ctx.memory_blocks.is_empty(),
            "memories are not instructions"
        );
    }

    #[tokio::test]
    async fn loads_claude_md_from_cwd() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("AGENTS.md");
        tokio::fs::write(&p, "# Test instructions\nbe concise.")
            .await
            .unwrap();
        let ctx = collect(
            dir.path().to_path_buf(),
            CollectOptions::defaults(),
            &test_paths(dir.path()),
            &base::interface::environment::SystemEnvironment,
            &no_git(),
        )
        .await;
        assert_eq!(ctx.memory_blocks.len(), 1, "expected one AGENTS.md");
        assert!(ctx.memory_blocks[0].content.contains("be concise"));
    }

    #[tokio::test]
    async fn loads_nested_claude_md_in_walk_up_order() {
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("p");
        let child = parent.join("c");
        tokio::fs::create_dir_all(&child).await.unwrap();
        tokio::fs::create_dir_all(parent.join(".git"))
            .await
            .unwrap();
        tokio::fs::write(parent.join("AGENTS.md"), "PARENT")
            .await
            .unwrap();
        tokio::fs::write(child.join("AGENTS.md"), "CHILD")
            .await
            .unwrap();

        let ctx = collect(
            child,
            CollectOptions::defaults(),
            &test_paths(dir.path()),
            &base::interface::environment::SystemEnvironment,
            &no_git(),
        )
        .await;
        // 应该顺序：parent 在前，child 在后（远到近）
        assert!(ctx.memory_blocks.len() >= 2);
        let parent_idx = ctx
            .memory_blocks
            .iter()
            .position(|e| e.content == "PARENT")
            .unwrap();
        let child_idx = ctx
            .memory_blocks
            .iter()
            .position(|e| e.content == "CHILD")
            .unwrap();
        assert!(parent_idx < child_idx, "parent must come before child");
    }
}
