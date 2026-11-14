//! `AGENTS.md` collection: the instruction files from the working directory up.

use base::frozen::MemoryFileEntry;
use base::paths::PromptFiles;
use std::path::{Path, PathBuf};

/// AGENTS.md total length cap.
const MAX_AGENTS_MD_CHARS: usize = 20_000;

/// 简版 AGENTS.md 加载：从 cwd 向上找到 git root（或 $HOME），每一层看
/// `AGENTS.md`。顺序：repo root -> 子目录 -> cwd（远到近）。去重 by canonical path。
/// **总长上限**：20 KB；超了从最远那段截。
///
/// `walk_up=false` 时只读 cwd 级 `AGENTS.md`，跳过中间所有父目录。给 monorepo
/// 子目录想隔离父级上下文用。
///
/// `AGENTS.md` 是唯一权威指令文件，**不读** `.agix/AGIX.md`。
pub async fn collect_agents_md(
    cwd: &Path,
    do_walk_up: bool,
    files: &PromptFiles,
) -> Vec<MemoryFileEntry> {
    let mut visited = std::collections::HashSet::new();
    let mut walk_up: Vec<PathBuf> = Vec::new();

    if do_walk_up {
        // 从 cwd 向上爬到根
        let mut p = cwd.to_path_buf();
        loop {
            let candidate = p.join("AGENTS.md");
            if candidate.exists() {
                walk_up.push(candidate);
            }
            match p.parent() {
                Some(parent) if parent != p => p = parent.to_path_buf(),
                _ => break,
            }
        }
    } else {
        // 只 cwd 级
        let candidate = cwd.join("AGENTS.md");
        if candidate.exists() {
            walk_up.push(candidate);
        }
    }

    // 远到近：顶层 repo → cwd → 子（这里 walk_up 是 cwd 向上，所以反转）
    walk_up.reverse();

    let mut entries: Vec<MemoryFileEntry> = Vec::new();
    let mut total_len = 0usize;

    for path in walk_up {
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
        let block_len = content.len();
        // 总长达上限 -> 不再 push 后续段；保最近的（向后截留更近的内容）
        if total_len + block_len > MAX_AGENTS_MD_CHARS {
            break;
        }
        total_len += block_len;
        entries.push(MemoryFileEntry {
            path: canonical,
            content,
        });
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn walk_up_false_skips_parent_claude_md() {
        // parent 有 AGENTS.md / cwd 有 AGENTS.md；walk_up=false 应只读 cwd
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("parent");
        let child = parent.join("child");
        tokio::fs::create_dir_all(&child).await.unwrap();
        tokio::fs::write(parent.join("AGENTS.md"), "PARENT-MONOREPO")
            .await
            .unwrap();
        tokio::fs::write(child.join("AGENTS.md"), "CHILD-LOCAL")
            .await
            .unwrap();

        // walk_up = true（默认）-- parent 应该出现
        let with_walk = collect_agents_md(&child, true, &PromptFiles::unscoped()).await;
        assert!(with_walk
            .iter()
            .any(|e| e.content.contains("PARENT-MONOREPO")));
        assert!(with_walk.iter().any(|e| e.content.contains("CHILD-LOCAL")));

        // walk_up = false -- 只 child
        let no_walk = collect_agents_md(&child, false, &PromptFiles::unscoped()).await;
        assert!(!no_walk
            .iter()
            .any(|e| e.content.contains("PARENT-MONOREPO")));
        assert!(no_walk.iter().any(|e| e.content.contains("CHILD-LOCAL")));
    }
}
