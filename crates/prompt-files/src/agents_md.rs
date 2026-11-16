//! `AGENTS.md` collection: the instruction files from the working directory up.

use base::frozen::MemoryFileEntry;
use base::paths::{ConfigPaths, PromptFiles};
use std::path::{Path, PathBuf};

/// AGENTS.md total length cap.
const MAX_AGENTS_MD_CHARS: usize = 20_000;

/// `AGENTS.md` 收集：全局层（`<global_data_dir>/AGENTS.md`）最先，其后是
/// git 根到 cwd 每一层的 `AGENTS.md`，远到近排列，近的在后、优先。去重 by
/// canonical path。
///
/// 向上只走到 git 根；找不到 git 根就只看 cwd 这一层，不会走出项目。
/// 总长上限 20 KB：从最远的一份开始丢，放不下的那份截断而不是整份丢掉。
///
/// `walk_up=false` 时只读 cwd 级和全局层。
///
/// `AGENTS.md` 是唯一权威指令文件，**不读** `.agix/AGIX.md`。
pub async fn collect_agents_md(
    cwd: &Path,
    do_walk_up: bool,
    paths: &ConfigPaths,
    files: &PromptFiles,
) -> Vec<MemoryFileEntry> {
    let mut candidates = vec![paths.global_data_dir.join("AGENTS.md")];
    let mut levels = vec![cwd.to_path_buf()];
    if do_walk_up {
        levels = levels_up_to_git_root(cwd).unwrap_or(levels);
    }
    candidates.extend(levels.iter().rev().map(|dir| dir.join("AGENTS.md")));

    let mut visited = std::collections::HashSet::new();
    let mut loaded: Vec<MemoryFileEntry> = Vec::new();
    for path in candidates {
        let Ok(canonical) = files.resolve(&path) else {
            continue;
        };
        if !visited.insert(canonical.clone()) {
            continue;
        }
        let Ok(content) = tokio::fs::read_to_string(&canonical).await else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        loaded.push(MemoryFileEntry {
            path: canonical,
            content,
        });
    }

    let mut remaining = MAX_AGENTS_MD_CHARS;
    let mut kept: Vec<MemoryFileEntry> = Vec::new();
    for mut entry in loaded.into_iter().rev() {
        if remaining == 0 {
            break;
        }
        if entry.content.len() > remaining {
            let mut end = remaining;
            while !entry.content.is_char_boundary(end) {
                end -= 1;
            }
            entry.content.truncate(end);
        }
        remaining -= entry.content.len();
        kept.push(entry);
    }
    kept.reverse();
    kept
}

fn levels_up_to_git_root(cwd: &Path) -> Option<Vec<PathBuf>> {
    let mut levels = Vec::new();
    for dir in cwd.ancestors() {
        levels.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            return Some(levels);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn paths(root: &Path) -> ConfigPaths {
        ConfigPaths::new(
            root.join("global"),
            root.join(".agix"),
            "code",
            Default::default(),
        )
    }

    #[tokio::test]
    async fn walk_up_false_skips_parent_claude_md() {
        // parent 有 AGENTS.md / cwd 有 AGENTS.md；walk_up=false 应只读 cwd
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("parent");
        let child = parent.join("child");
        tokio::fs::create_dir_all(&child).await.unwrap();
        tokio::fs::create_dir_all(parent.join(".git"))
            .await
            .unwrap();
        tokio::fs::write(parent.join("AGENTS.md"), "PARENT-MONOREPO")
            .await
            .unwrap();
        tokio::fs::write(child.join("AGENTS.md"), "CHILD-LOCAL")
            .await
            .unwrap();

        // walk_up = true（默认）-- parent 应该出现
        let with_walk =
            collect_agents_md(&child, true, &paths(dir.path()), &PromptFiles::unscoped()).await;
        assert!(with_walk
            .iter()
            .any(|e| e.content.contains("PARENT-MONOREPO")));
        assert!(with_walk.iter().any(|e| e.content.contains("CHILD-LOCAL")));

        // walk_up = false -- 只 child
        let no_walk =
            collect_agents_md(&child, false, &paths(dir.path()), &PromptFiles::unscoped()).await;
        assert!(!no_walk
            .iter()
            .any(|e| e.content.contains("PARENT-MONOREPO")));
        assert!(no_walk.iter().any(|e| e.content.contains("CHILD-LOCAL")));
    }

    #[tokio::test]
    async fn the_walk_stops_at_the_git_root() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        let cwd = repo.join("pkg");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(repo.join(".git")).await.unwrap();
        tokio::fs::write(dir.path().join("AGENTS.md"), "ABOVE")
            .await
            .unwrap();
        tokio::fs::write(repo.join("AGENTS.md"), "REPO")
            .await
            .unwrap();

        let got = collect_agents_md(&cwd, true, &paths(dir.path()), &PromptFiles::unscoped()).await;
        let texts: Vec<_> = got.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(texts, ["REPO"]);

        tokio::fs::remove_dir_all(repo.join(".git")).await.unwrap();
        tokio::fs::write(cwd.join("AGENTS.md"), "CWD")
            .await
            .unwrap();
        let got = collect_agents_md(&cwd, true, &paths(dir.path()), &PromptFiles::unscoped()).await;
        let texts: Vec<_> = got.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(
            texts,
            ["CWD"],
            "without a git root only the cwd level counts"
        );
    }

    #[tokio::test]
    async fn over_the_cap_the_farthest_is_cut_and_the_nearest_stays_whole() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        let cwd = repo.join("pkg");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(repo.join(".git")).await.unwrap();
        tokio::fs::create_dir_all(dir.path().join("global"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("global/AGENTS.md"), "G".repeat(9_000))
            .await
            .unwrap();
        tokio::fs::write(repo.join("AGENTS.md"), "R".repeat(30_000))
            .await
            .unwrap();
        tokio::fs::write(cwd.join("AGENTS.md"), "C".repeat(5_000))
            .await
            .unwrap();

        let got = collect_agents_md(&cwd, true, &paths(dir.path()), &PromptFiles::unscoped()).await;
        let lens: Vec<usize> = got.iter().map(|e| e.content.len()).collect();
        assert_eq!(lens, [15_000, 5_000], "global dropped, repo cut, cwd whole");
        assert!(got[0].content.starts_with('R') && got[1].content.starts_with('C'));
    }
}
