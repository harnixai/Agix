//! Git worktree 管理器 —— 给 sub-agent 提供独立 checkout 的隔离运行环境。
//!
//! 本模块不引入 git 库依赖，全靠 `git` 子命令；每条都经 [`EngineExec`] 起——
//! 会话的命令在沙箱里时，这里的 git 也在同一个沙箱里，只能写 git 目录与
//! 仓库根下的 worktree 目录（[`Layout::worktrees_dir`]，缺省 `<仓库>/.wkts/`）。
//! 第一次建 worktree 时把这个目录（不是项目层目录）加进仓库根的 `.gitignore`。
//! 删除 worktree 时，它的目录和 `.git/worktrees/` 下的那一项由引擎经 `FileSystem`
//! 删，不交给 git：沙箱不许写已有 worktree 的 `.git` 链接与 `commondir`。
//!
//! ## 用法
//!
//! ```ignore
//! let handle = create_worktree(&exec, &git, repo_root, "probe", &Default::default()).await?;
//! // sub-agent 在 handle.path() 跑
//! if let Some(kept) = handle.remove_if_untouched().await {
//!     // 有未提交的改动或新提交：worktree 与分支都留着，kept 说在哪
//! }
//! ```
//!
//! ## 边界
//!
//! - 非 git 仓库 → 立即 `NotAGitRepo` 错
//! - slug 必须通过 [`validate_slug`]（防 path traversal）
//! - 同一 slug 路径已存在 → `AlreadyExists`（首层不做 resume；交给 phase 1c
//!   决定要不要做）
//! - git 必须进沙箱却进不去 → `GitRefused`，不在沙箱外跑
//! - cleanup 失败仅 warn，不传播 —— sub-agent 主要工作已完成
//! - `remove_if_untouched` 只删干净的：没有未提交 / 未跟踪的改动、分支与 HEAD
//!   都没有建它以来的新提交；git 读不出状态时也留着

use base::engine_exec::{EngineExec, EngineExecError, GitAccess};
use base::interface::exec::{ExecError, ExecProviders, FileSystem};
use base::paths::Layout;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, warn};

const SLUG_MAX_LEN: usize = 64;
const GIT_TIMEOUT: Duration = Duration::from_secs(15);
const UNREADABLE: &str = "a state git could not read";

/// `WorktreeHandle` —— 创建后持有；drop 时不自动清（避免 sync drop 起 tokio
/// runtime）。caller 必须显式 await `cleanup()`。
#[derive(Debug)]
pub struct WorktreeHandle {
    path: PathBuf,
    branch: String,
    /// The commit the worktree was made at.
    base: String,
    repo: Repo,
    linked_git: Option<base::paths::WorktreeGit>,
    exec: ExecProviders,
    git: EngineExec,
    /// 若已经被 cleanup 过则 false，二次调用变 no-op
    pending: bool,
}

/// A repository's work tree and the git directory its worktree bookkeeping
/// is written to.
#[derive(Debug, Clone)]
struct Repo {
    root: PathBuf,
    git_dir: PathBuf,
    /// Where the engine's worktrees of it live ([`Layout::worktrees_dir`]).
    worktrees: PathBuf,
}

impl Repo {
    /// What `git worktree` and `git branch` may write: the bookkeeping in the
    /// git directory, and the directory the worktrees live in.
    fn bookkeeping(&self) -> Vec<PathBuf> {
        vec![self.git_dir.clone(), self.worktrees.clone()]
    }
}

impl WorktreeHandle {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn branch(&self) -> &str {
        &self.branch
    }
    pub fn repo_root(&self) -> &Path {
        &self.repo.root
    }

    /// The worktree with what a commit in it writes of the repository's git
    /// directory — `None` there when the worktree's links, read right after
    /// it was made, did not lead from its entry under `worktrees/` back.
    pub fn linked(&self) -> base::paths::LinkedWorktree {
        base::paths::LinkedWorktree {
            dir: self.path.clone(),
            git: self.linked_git.clone(),
        }
    }

    /// Removes the worktree and its branch if nothing was left in them;
    /// otherwise leaves both on disk and says where they are and why.
    pub async fn remove_if_untouched(mut self) -> Option<KeptWorktree> {
        if !self.pending {
            return None;
        }
        let holds = self.work_left().await;
        if holds.is_empty() {
            self.cleanup().await;
            return None;
        }
        self.pending = false;
        Some(KeptWorktree {
            path: self.path.clone(),
            branch: self.branch.clone(),
            holds,
        })
    }

    async fn work_left(&self) -> Vec<String> {
        let branch = format!("refs/heads/{}", self.branch);
        let status = [
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=all",
        ];
        let new_commits = ["rev-list", "--count", "HEAD", &branch, "--not", &self.base];
        let (Ok(status), Ok(commits)) = (
            self.git_in_worktree(&status).await,
            self.git_in_worktree(&new_commits).await,
        ) else {
            return vec![UNREADABLE.to_string()];
        };
        let Ok(commits) = commits.trim().parse::<u64>() else {
            return vec![UNREADABLE.to_string()];
        };
        let mut holds = Vec::new();
        if !status.trim().is_empty() {
            holds.push("uncommitted changes".to_string());
        }
        if commits > 0 {
            holds.push(format!("{commits} commit(s) since it was made"));
        }
        holds
    }

    /// A read-only git in the worktree, started from the repository's root:
    /// under bwrap a sandboxed git working in `/tmp` sees nothing there but
    /// its working directory, and from the worktree that would leave out the
    /// git directory its `.git` link leads to.
    async fn git_in_worktree(&self, args: &[&str]) -> Result<String, WorktreeError> {
        let path = self.path.to_string_lossy();
        let args: Vec<&str> = ["-C", path.as_ref()]
            .into_iter()
            .chain(args.iter().copied())
            .collect();
        run_git_capture(&self.git, &self.repo.root, &args, &[]).await
    }

    /// 删除 worktree 目录 + 关联分支。失败 warn 不报错。
    /// Takes `&mut self` so callers can store the handle in an `Option` and
    /// clean up without consuming ownership.
    pub async fn cleanup(&mut self) {
        if !self.pending {
            return;
        }
        self.pending = false;

        let admin = admin_dir(
            self.exec.filesystem.as_ref(),
            &self.path,
            &self.repo.git_dir,
        )
        .await;
        for dir in std::iter::once(&self.path).chain(&admin) {
            if let Err(e) = self.exec.filesystem.remove_dir_all(dir).await {
                warn!(
                    path = %dir.display(),
                    error = %e,
                    "could not remove the worktree; remove it by hand, then run \
                     `git worktree prune`"
                );
            }
        }
        if admin.is_none() {
            warn!(
                path = %self.path.display(),
                "the worktree's entry under .git/worktrees/ was not found; run `git worktree prune`"
            );
        }

        if let Err(e) = run_git(
            &self.git,
            &self.repo.root,
            &["branch", "-D", &self.branch],
            &self.repo.bookkeeping(),
        )
        .await
        {
            warn!(
                branch = %self.branch,
                error = %e,
                "git branch -D failed; remove manually if not wanted"
            );
        }
    }
}

/// A worktree [`WorktreeHandle::remove_if_untouched`] left on disk, with its
/// branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptWorktree {
    pub path: PathBuf,
    pub branch: String,
    /// What it holds that removing it would lose: uncommitted changes, new
    /// commits.
    pub holds: Vec<String>,
}

/// The entry of the worktree at `path` under `git_dir/worktrees/`, as its
/// `.git` file names it — and only if it is there.
async fn admin_dir(fs: &dyn FileSystem, path: &Path, git_dir: &Path) -> Option<PathBuf> {
    let link = fs.read_to_string(&path.join(".git")).await.ok()?;
    let named = link.trim().strip_prefix("gitdir:")?.trim();
    let admin = fs.canonicalize(&path.join(named)).await.ok()?;
    let worktrees = fs
        .canonicalize(&git_dir.join(base::paths::GIT_WORKTREES_DIR))
        .await
        .ok()?;
    (admin.parent() == Some(worktrees.as_path())).then_some(admin)
}

/// `repo`'s git directory, resolved, with the entry of the worktree just
/// made at `path` and its `branch`, when the worktree is linked to it: its
/// `.git` file names an entry directly under `worktrees/` there, that
/// entry's `commondir` leads back, and the branch is a loose ref. Otherwise
/// something rewrote the links, or the repository keeps its refs some other
/// way, and the worktree gets nothing of the git directory.
async fn linked_worktree_git(
    exec: &ExecProviders,
    path: &Path,
    repo: &Repo,
    branch: &str,
) -> Option<base::paths::WorktreeGit> {
    let fs = exec.filesystem.as_ref();
    let git = match links_lead_back(fs, path, &repo.git_dir).await {
        Some((common, admin)) => loose_branch(fs, common, admin, branch).await,
        None => None,
    };
    if git.is_none() {
        warn!(
            worktree = %path.display(),
            git_dir = %repo.git_dir.display(),
            "the worktree's links do not lead back to the repository's git directory, or \
             its branch is not a loose ref; a sandboxed sub-agent in it cannot commit"
        );
    }
    git
}

/// `git_dir`, resolved, and the worktree's entry in it, when the worktree at
/// `path` is linked to it.
async fn links_lead_back(
    fs: &dyn FileSystem,
    path: &Path,
    git_dir: &Path,
) -> Option<(PathBuf, PathBuf)> {
    let common = fs.canonicalize(git_dir).await.ok()?;
    let admin = admin_dir(fs, path, &common).await?;
    let named = fs.read_to_string(&admin.join("commondir")).await.ok()?;
    let named = fs.canonicalize(&admin.join(named.trim())).await.ok()?;
    (named == common).then_some((common, admin))
}

/// What a commit on `branch` writes in `dir`, when `branch` is a loose ref.
async fn loose_branch(
    fs: &dyn FileSystem,
    dir: PathBuf,
    admin: PathBuf,
    branch: &str,
) -> Option<base::paths::WorktreeGit> {
    let git = base::paths::WorktreeGit {
        dir,
        admin,
        branch: branch.to_string(),
    };
    let loose = fs
        .metadata(&git.branch_ref())
        .await
        .is_ok_and(|m| !m.is_dir);
    loose.then_some(git)
}

/// The worktree at `dir` again, for a session brought back from its log
/// that worked there: still a directory directly in one of the
/// repository's worktrees directories ([`Layout::worktrees_dir_names`]), of a
/// repository `project` is in, and
/// still linked to `<repository>/.git`. That git directory is the one it
/// carries, never one the links name, and the branch is the one the engine
/// names after the directory, never the one the worktree's `HEAD` names.
pub async fn reopen_worktree(
    fs: &dyn FileSystem,
    project: &Path,
    dir: &Path,
    layout: &Layout,
) -> Result<base::paths::LinkedWorktree, String> {
    let not_one = || {
        format!(
            "{} is not a worktree of the project {}",
            dir.display(),
            project.display()
        )
    };
    let root = dir
        .parent()
        .filter(|parent| {
            parent
                .file_name()
                .is_some_and(|name| layout.worktrees_dir_names().any(|own| name == own))
        })
        .and_then(Path::parent)
        .ok_or_else(not_one)?;
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(not_one)?;
    let (project_real, root_real) = (
        fs.canonicalize_best_effort(project).await,
        fs.canonicalize_best_effort(root).await,
    );
    if !project_real.starts_with(&root_real) {
        return Err(not_one());
    }
    if !fs.metadata(dir).await.is_ok_and(|m| m.is_dir) {
        return Err(format!("its worktree {} is gone", dir.display()));
    }
    let git_dir = root.join(".git");
    let (common, admin) = links_lead_back(fs, dir, &git_dir).await.ok_or_else(|| {
        format!(
            "its worktree {} is no longer linked to {}",
            dir.display(),
            git_dir.display()
        )
    })?;
    let branch = format!("{}{name}", layout.worktree_branch_prefix());
    Ok(base::paths::LinkedWorktree {
        dir: dir.to_path_buf(),
        git: loose_branch(fs, common, admin, &branch).await,
    })
}

#[derive(thiserror::Error, Debug)]
pub enum WorktreeError {
    #[error("not a git repository: {0}")]
    NotAGitRepo(PathBuf),

    #[error("invalid worktree slug: {0}")]
    InvalidSlug(String),

    #[error("worktree path already exists: {0}")]
    AlreadyExists(PathBuf),

    #[error(
        "{0} is a symbolic link; worktrees are made only in a directory of the repository's own"
    )]
    WorktreesDirIsALink(PathBuf),

    #[error("git command failed: {0}")]
    GitFailed(String),

    #[error("git was not run: {0}")]
    GitRefused(String),

    #[error("{0}")]
    Exec(#[from] ExecError),
}

/// 创建 worktree。会:
/// 1. 验 slug
/// 2. 找 repo root 与 git 目录（slug 错或非 git 都 fail-fast）
/// 3. 写仓库根的 `.gitignore` 加 worktree 目录（若未存在）
/// 4. `git worktree add -b <branch> <path> <HEAD 的提交>`
pub async fn create_worktree(
    exec: &ExecProviders,
    git: &EngineExec,
    cwd: &Path,
    slug: &str,
    layout: &Layout,
) -> Result<WorktreeHandle, WorktreeError> {
    validate_slug(slug)?;
    let repo = find_repo(git, cwd, layout).await?;
    let flat = flatten_slug(slug);
    let path = repo.worktrees.join(&flat);
    if exec.filesystem.metadata(&path).await.is_ok() {
        return Err(WorktreeError::AlreadyExists(path));
    }
    let branch = format!("{}{flat}", layout.worktree_branch_prefix());

    // A sandboxed command may write the project, so may have left a link
    // here: followed, the checkout would land where the link points.
    if exec
        .filesystem
        .metadata(&repo.worktrees)
        .await
        .is_ok_and(|m| m.is_symlink)
    {
        return Err(WorktreeError::WorktreesDirIsALink(repo.worktrees));
    }
    // 父目录得先存在 git worktree add 才不报错
    if let Some(parent) = path.parent() {
        exec.filesystem.create_dir_all(parent).await?;
    }

    ensure_gitignore_entry(exec, &repo.root, layout).await;

    let base = run_git_capture(git, &repo.root, &["rev-parse", "HEAD"], &[])
        .await?
        .trim()
        .to_string();
    run_git(
        git,
        &repo.root,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &path.to_string_lossy(),
            &base,
        ],
        &repo.bookkeeping(),
    )
    .await?;

    debug!(
        path = %path.display(),
        branch = %branch,
        "worktree created"
    );

    let linked_git = linked_worktree_git(exec, &path, &repo, &branch).await;
    Ok(WorktreeHandle {
        path,
        branch,
        base,
        repo,
        linked_git,
        exec: exec.clone(),
        git: git.clone(),
        pending: true,
    })
}

/// `template` with `{worktrees_dir}` and `{branch_prefix}` filled in: where
/// this session keeps worktrees and what its branches are called.
pub fn name_the_directories(template: &str, layout: &Layout) -> String {
    template
        .replace("{worktrees_dir}", layout.worktrees_dir_name())
        .replace("{branch_prefix}", &layout.worktree_branch_prefix())
}

/// Slug 校验：长度上限 + segment 正则 + 拒 `.` `..`
///
/// 安全性：path traversal 通过 `Path::join` 时，`..` 段会逃出根 —— 这里硬拒。
pub fn validate_slug(s: &str) -> Result<(), WorktreeError> {
    if s.is_empty() {
        return Err(WorktreeError::InvalidSlug("empty".into()));
    }
    if s.len() > SLUG_MAX_LEN {
        return Err(WorktreeError::InvalidSlug(format!(
            "must be {SLUG_MAX_LEN} chars or fewer (got {})",
            s.len()
        )));
    }
    for seg in s.split('/') {
        if seg.is_empty() {
            return Err(WorktreeError::InvalidSlug(
                "empty segment (leading / trailing / consecutive `/`)".into(),
            ));
        }
        if seg == "." || seg == ".." {
            return Err(WorktreeError::InvalidSlug(format!(
                "segment cannot be `.` or `..`: {s}"
            )));
        }
        if !seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(WorktreeError::InvalidSlug(format!(
                "segment must match [a-zA-Z0-9._-]+: {seg}"
            )));
        }
    }
    Ok(())
}

/// `/` → `+` —— 让嵌套 slug 在文件系统 + git 分支名里都安全。
/// `+` 不在 SLUG_VALID 字符集里，所以映射可逆（虽然我们不需要逆向）。
pub(crate) fn flatten_slug(s: &str) -> String {
    s.replace('/', "+")
}

/// The repository `cwd` is in.
///
/// A sandboxed git may write the git directory only where the sandbox guards
/// its configuration and hooks, which it does for `<root>/.git`; a repository
/// whose git directory lives anywhere else is refused there.
async fn find_repo(git: &EngineExec, cwd: &Path, layout: &Layout) -> Result<Repo, WorktreeError> {
    let out = run_git_capture(git, cwd, &["rev-parse", "--show-toplevel"], &[]).await?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Err(WorktreeError::NotAGitRepo(cwd.to_path_buf()));
    }
    let root = PathBuf::from(trimmed);
    let common = run_git_capture(git, &root, &["rev-parse", "--git-common-dir"], &[]).await?;
    let git_dir = root.join(common.trim());
    let guarded = root.join(".git");
    if git.is_sandboxed() && !same_dir(&git_dir, &guarded) {
        return Err(WorktreeError::GitRefused(format!(
            "the session is sandboxed, and {}'s git directory is {}, not {}; the \
             sandbox can only keep a worktree from rewriting the repository's \
             configuration in the latter",
            root.display(),
            git_dir.display(),
            guarded.display()
        )));
    }
    Ok(Repo {
        worktrees: layout.worktrees_dir(&root),
        root,
        git_dir,
    })
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b && b.is_dir(),
        _ => false,
    }
}

/// 在 repo root 的 `.gitignore` 末尾追加 worktree 目录那一行（带换行）；若文件不存在创建；
/// 若这一行已经在文件里出现一次，no-op。`.gitignore` 是符号链接时不动它，也不动它指向的文件。
/// 失败仅 warn。
async fn ensure_gitignore_entry(exec: &ExecProviders, repo_root: &Path, layout: &Layout) {
    let entry = &layout.gitignore_entry();
    let path = repo_root.join(".gitignore");
    if exec
        .filesystem
        .metadata(&path)
        .await
        .is_ok_and(|m| m.is_symlink)
    {
        warn!(
            path = %path.display(),
            "the repository's .gitignore is a symlink; not adding the worktree directory to it"
        );
        return;
    }
    let existing = exec
        .filesystem
        .read_to_string(&path)
        .await
        .unwrap_or_default();
    if existing
        .lines()
        .any(|line| line.trim() == entry.trim_end_matches('/') || line.trim() == entry)
    {
        return;
    }
    let mut new_content = existing;
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(&format!(
        "# {} worktrees (auto-added)\n",
        layout.product_id()
    ));
    new_content.push_str(entry);
    if !entry.ends_with('\n') {
        new_content.push('\n');
    }
    if let Err(e) = exec
        .filesystem
        .write_not_following(&path, new_content.as_bytes())
        .await
    {
        warn!(
            path = %path.display(),
            error = %e,
            "failed to update .gitignore for worktree directory; user may want to add it manually"
        );
    }
}

/// 跑一个 git 命令；非零退出转 GitFailed。stdout/stderr 一并附进 error。
async fn run_git(
    git: &EngineExec,
    cwd: &Path,
    args: &[&str],
    writable: &[PathBuf],
) -> Result<(), WorktreeError> {
    let _ = run_git_capture(git, cwd, args, writable).await?;
    Ok(())
}

/// 跑 git，返回 stdout（trimmed）。带 15s 超时。沙箱里它只能写 `writable`。
async fn run_git_capture(
    git: &EngineExec,
    cwd: &Path,
    args: &[&str],
    writable: &[PathBuf],
) -> Result<String, WorktreeError> {
    let out = git
        .git(cwd, GitAccess::Writes(writable), args, GIT_TIMEOUT)
        .await
        .map_err(|e| match e {
            EngineExecError::Refused(why) => WorktreeError::GitRefused(why),
            EngineExecError::TimedOut(_) => {
                WorktreeError::GitFailed(format!("git {} timed out after 15s", args.join(" ")))
            }
            EngineExecError::Failed(e) => WorktreeError::Exec(e),
            other => WorktreeError::GitFailed(other.to_string()),
        })?;

    if !out.status.success {
        return Err(WorktreeError::GitFailed(format!(
            "git {}: {}",
            args.join(" "),
            out.stderr_lossy().trim()
        )));
    }
    Ok(out.stdout_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::paths::Layout;

    fn layouts() -> [Layout; 2] {
        [
            Layout::default(),
            Layout::new(".codix")
                .unwrap()
                .with_worktrees_dir(".trees")
                .unwrap(),
        ]
    }

    #[test]
    fn the_guides_name_the_instances_directory_and_branches() {
        for layout in layouts() {
            let name = layout.worktrees_dir_name();
            let text = name_the_directories(
                include_str!("prompts/coding/worktree_enter.prompt.md"),
                &layout,
            );
            assert!(text.contains(&format!("`{name}/<slug>`")), "{text}");
            assert!(
                text.contains(&format!("`{}<slug>`", layout.worktree_branch_prefix())),
                "{text}"
            );
            assert!(!text.contains('{'), "{text}");
        }
    }

    use tempfile::TempDir;

    #[test]
    fn validate_slug_accepts_simple() {
        assert!(validate_slug("foo").is_ok());
        assert!(validate_slug("foo-bar_baz.42").is_ok());
    }

    #[test]
    fn validate_slug_accepts_nested() {
        assert!(validate_slug("user/feature").is_ok());
        assert!(validate_slug("a/b/c").is_ok());
    }

    #[test]
    fn validate_slug_rejects_empty() {
        assert!(matches!(
            validate_slug(""),
            Err(WorktreeError::InvalidSlug(_))
        ));
    }

    #[test]
    fn validate_slug_rejects_dotdot() {
        assert!(matches!(
            validate_slug(".."),
            Err(WorktreeError::InvalidSlug(_))
        ));
        assert!(matches!(
            validate_slug("foo/../bar"),
            Err(WorktreeError::InvalidSlug(_))
        ));
    }

    #[test]
    fn validate_slug_rejects_dot() {
        assert!(matches!(
            validate_slug("."),
            Err(WorktreeError::InvalidSlug(_))
        ));
        assert!(matches!(
            validate_slug("foo/./bar"),
            Err(WorktreeError::InvalidSlug(_))
        ));
    }

    #[test]
    fn validate_slug_rejects_too_long() {
        let s = "a".repeat(SLUG_MAX_LEN + 1);
        assert!(matches!(
            validate_slug(&s),
            Err(WorktreeError::InvalidSlug(_))
        ));
    }

    #[test]
    fn validate_slug_rejects_special_chars() {
        for bad in &["foo bar", "foo*", "foo$", "foo:bar", "foo\\bar", "foo;rm"] {
            assert!(
                matches!(validate_slug(bad), Err(WorktreeError::InvalidSlug(_))),
                "expected reject for {bad}"
            );
        }
    }

    #[test]
    fn validate_slug_rejects_leading_or_trailing_slash() {
        assert!(matches!(
            validate_slug("/foo"),
            Err(WorktreeError::InvalidSlug(_))
        ));
        assert!(matches!(
            validate_slug("foo/"),
            Err(WorktreeError::InvalidSlug(_))
        ));
        assert!(matches!(
            validate_slug("foo//bar"),
            Err(WorktreeError::InvalidSlug(_))
        ));
    }

    #[test]
    fn flatten_slug_replaces_slash() {
        assert_eq!(flatten_slug("foo"), "foo");
        assert_eq!(flatten_slug("user/feature"), "user+feature");
        assert_eq!(flatten_slug("a/b/c"), "a+b+c");
    }

    fn exec() -> ExecProviders {
        platform::local_providers()
    }

    fn engine(exec: &ExecProviders) -> EngineExec {
        EngineExec::new(exec.clone(), Confinement::Unconfined, ProjectTrust::Trusted)
    }

    fn direct() -> EngineExec {
        engine(&exec())
    }

    use base::engine_exec::Confinement;
    use base::permission::ProjectTrust;
    use tokio_util::sync::CancellationToken;

    use base::interface::exec::{
        DirEntry, FileSystem, Metadata, Process, ProcessHandle, ProcessSpec,
    };
    use platform::{LocalFileSystem, LocalProcess};

    /// The local providers, with a note of everything that went through them.
    ///
    /// This is what makes "the git subprocess goes through the contract" a
    /// testable claim rather than a code-reading exercise: a call that went
    /// straight to `tokio::process` would leave no entry here and every other
    /// assertion in these tests would still pass.
    #[derive(Default)]
    struct Log {
        argv: std::sync::Mutex<Vec<Vec<String>>>,
        written: std::sync::Mutex<Vec<PathBuf>>,
    }

    struct WatchedProcess(std::sync::Arc<Log>);

    #[async_trait::async_trait]
    impl Process for WatchedProcess {
        async fn spawn(
            &self,
            spec: ProcessSpec,
            cancel: CancellationToken,
        ) -> Result<Box<dyn ProcessHandle>, ExecError> {
            let mut argv = vec![spec.program.clone()];
            argv.extend(spec.args.iter().cloned());
            self.0.argv.lock().unwrap().push(argv);
            LocalProcess::default().spawn(spec, cancel).await
        }
    }

    struct WatchedFileSystem(std::sync::Arc<Log>);

    #[async_trait::async_trait]
    impl FileSystem for WatchedFileSystem {
        async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError> {
            LocalFileSystem.read(path).await
        }
        async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
            self.0.written.lock().unwrap().push(path.to_path_buf());
            LocalFileSystem.write(path, bytes).await
        }
        async fn create_dir_all(&self, path: &Path) -> Result<(), ExecError> {
            LocalFileSystem.create_dir_all(path).await
        }
        async fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>, ExecError> {
            LocalFileSystem.read_dir(path).await
        }
        async fn remove_dir_all(&self, path: &Path) -> Result<(), ExecError> {
            LocalFileSystem.remove_dir_all(path).await
        }
        async fn rename(&self, from: &Path, to: &Path) -> Result<(), ExecError> {
            LocalFileSystem.rename(from, to).await
        }
        async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError> {
            LocalFileSystem.metadata(path).await
        }
        async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError> {
            LocalFileSystem.canonicalize(path).await
        }
    }

    fn watched() -> (ExecProviders, std::sync::Arc<Log>) {
        let log = std::sync::Arc::new(Log::default());
        let mut providers = platform::local_providers();
        providers.process = std::sync::Arc::new(WatchedProcess(log.clone()));
        providers.filesystem = std::sync::Arc::new(WatchedFileSystem(log.clone()));
        (providers, log)
    }

    /// 在 TempDir 起一个最小 git repo，里面有一次提交（worktree add 需要 HEAD）
    async fn make_minimal_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        let init = run_git_capture(&direct(), p, &["init"], &[]).await;
        assert!(init.is_ok(), "git init: {init:?}");
        // 配置 user 让 commit 不报错（CI 可能没全局 config）
        run_git_capture(
            &direct(),
            p,
            &["config", "user.email", "test@example.com"],
            &[],
        )
        .await
        .unwrap();
        run_git_capture(&direct(), p, &["config", "user.name", "test"], &[])
            .await
            .unwrap();
        tokio::fs::write(p.join("README.md"), "# test")
            .await
            .unwrap();
        run_git_capture(&direct(), p, &["add", "."], &[])
            .await
            .unwrap();
        run_git_capture(&direct(), p, &["commit", "-m", "init"], &[])
            .await
            .unwrap();
        dir
    }

    #[tokio::test]
    async fn create_worktree_succeeds_in_real_repo() {
        for layout in layouts() {
            let repo = make_minimal_repo().await;
            let mut handle = create_worktree(&exec(), &direct(), repo.path(), "probe", &layout)
                .await
                .unwrap();

            // worktree 路径应当存在且包含一份 README
            assert!(handle.path().exists(), "worktree path missing");
            assert!(
                handle.path().join("README.md").exists(),
                "worktree should have README"
            );
            assert_eq!(handle.branch(), "agix/worktree-probe");

            let root = repo.path().canonicalize().unwrap();
            assert_eq!(handle.path(), layout.worktrees_dir(&root).join("probe"));
            let gi = tokio::fs::read_to_string(repo.path().join(".gitignore"))
                .await
                .unwrap_or_default();
            assert!(
                gi.lines().any(|l| l == layout.gitignore_entry()),
                "expected .gitignore entry; got {gi:?}"
            );
            assert!(
                !gi.contains(layout.project_dir_name()),
                "the project directory is the project's to commit: {gi:?}"
            );

            // cleanup 后路径 + 分支都应当消失
            handle.cleanup().await;
            assert!(!layout.worktrees_dir(repo.path()).join("probe").exists());
            let branches = run_git_capture(&direct(), repo.path(), &["branch", "--list"], &[])
                .await
                .unwrap();
            assert!(
                !branches.contains("agix/worktree-probe"),
                "branch should be deleted; got {branches:?}"
            );
        }
    }

    /// What the project directory holds stays the project's to commit;
    /// only the worktrees are kept out of the repository.
    #[tokio::test]
    async fn the_worktrees_are_ignored_and_the_project_directory_is_not() {
        for layout in layouts() {
            let repo = make_minimal_repo().await;
            let mut handle = create_worktree(&exec(), &direct(), repo.path(), "probe", &layout)
                .await
                .unwrap();
            let project_dir = layout.project_dir(repo.path());
            std::fs::create_dir_all(project_dir.join("agents")).unwrap();
            std::fs::write(project_dir.join("settings.json"), "{}").unwrap();
            std::fs::write(project_dir.join("agents/a.md"), "a").unwrap();
            let name = layout.project_dir_name();
            let ignored = |path: String| {
                let repo = repo.path().to_path_buf();
                async move {
                    git(&repo, &["check-ignore", "-q", "--no-index", &path])
                        .await
                        .status
                        .success
                }
            };
            assert!(!ignored(format!("{name}/settings.json")).await);
            assert!(!ignored(format!("{name}/agents/a.md")).await);
            assert!(ignored(format!("{}/probe/README.md", layout.worktrees_dir_name())).await);
            assert!(ignored(format!("{}/x", layout.worktrees_dir_name())).await);
            let added = git(
                repo.path(),
                &[
                    "add",
                    &format!("{name}/settings.json"),
                    &format!("{name}/agents/a.md"),
                ],
            )
            .await;
            assert!(added.status.success, "{}", added.stderr_lossy());
            let staged = run_git_capture(
                &direct(),
                repo.path(),
                &["diff", "--cached", "--name-only"],
                &[],
            )
            .await
            .unwrap();
            assert!(
                staged.contains(&format!("{name}/settings.json")),
                "{staged}"
            );
            let status = run_git_capture(&direct(), repo.path(), &["status", "--porcelain"], &[])
                .await
                .unwrap();
            assert!(!status.contains(layout.worktrees_dir_name()), "{status}");
            handle.cleanup().await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_worktrees_dir_that_is_a_link_is_refused() {
        for layout in layouts() {
            let repo = make_minimal_repo().await;
            let elsewhere = TempDir::new().unwrap();
            std::os::unix::fs::symlink(elsewhere.path(), layout.worktrees_dir(repo.path()))
                .unwrap();
            let r = create_worktree(&exec(), &direct(), repo.path(), "probe", &layout).await;
            assert!(
                matches!(&r, Err(WorktreeError::WorktreesDirIsALink(_))),
                "{r:?}"
            );
            assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
            assert!(
                !branch_exists(
                    repo.path(),
                    &format!("{}probe", layout.worktree_branch_prefix())
                )
                .await
            );
        }
    }

    async fn git(repo: &Path, args: &[&str]) -> base::interface::exec::Captured {
        direct()
            .git(repo, GitAccess::Writes(&[]), args, GIT_TIMEOUT)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_worktrees_branch_and_gitignore_note_carry_the_product_name() {
        let codix = Layout::new(".codix")
            .unwrap()
            .with_product_name("Codix")
            .unwrap();
        let repo = make_minimal_repo().await;
        let mut handle = create_worktree(&exec(), &direct(), repo.path(), "probe", &codix)
            .await
            .unwrap();
        assert_eq!(handle.branch(), "codix/worktree-probe");
        let gi = tokio::fs::read_to_string(repo.path().join(".gitignore"))
            .await
            .unwrap();
        assert!(gi.contains("# codix worktrees"), "{gi:?}");
        assert!(!gi.contains("agix"), "{gi:?}");
        handle.cleanup().await;
        assert!(!branch_exists(repo.path(), "codix/worktree-probe").await);
    }

    async fn branch_exists(repo: &Path, branch: &str) -> bool {
        run_git_capture(&direct(), repo, &["branch", "--list", branch], &[])
            .await
            .unwrap()
            .contains(branch)
    }

    #[tokio::test]
    async fn an_untouched_worktree_is_removed_with_its_branch() {
        for layout in layouts() {
            let repo = make_minimal_repo().await;
            let handle = create_worktree(&exec(), &direct(), repo.path(), "clean", &layout)
                .await
                .unwrap();
            let path = handle.path().to_path_buf();
            assert_eq!(handle.remove_if_untouched().await, None);
            assert!(!path.exists());
            assert!(!branch_exists(repo.path(), "agix/worktree-clean").await);
        }
    }

    #[tokio::test]
    async fn a_worktree_with_changes_or_commits_is_kept_with_its_branch() {
        for layout in layouts() {
            let repo = make_minimal_repo().await;
            let edited = create_worktree(&exec(), &direct(), repo.path(), "edited", &layout)
                .await
                .unwrap();
            std::fs::write(edited.path().join("new.txt"), "untracked").unwrap();
            let kept = edited.remove_if_untouched().await.expect("kept");
            assert_eq!(kept.holds, ["uncommitted changes"]);
            assert!(kept.path.join("new.txt").is_file());
            assert!(branch_exists(repo.path(), &kept.branch).await);

            let committed = create_worktree(&exec(), &direct(), repo.path(), "committed", &layout)
                .await
                .unwrap();
            std::fs::write(committed.path().join("c.txt"), "c").unwrap();
            for args in [&["add", "c.txt"][..], &["commit", "-qm", "c"]] {
                run_git_capture(&direct(), committed.path(), args, &[])
                    .await
                    .unwrap();
            }
            run_git_capture(
                &direct(),
                committed.path(),
                &["checkout", "-q", "--detach", "HEAD~1"],
                &[],
            )
            .await
            .unwrap();
            let kept = committed.remove_if_untouched().await.expect("kept");
            assert_eq!(kept.holds, ["1 commit(s) since it was made"]);
            assert_eq!(kept.branch, "agix/worktree-committed");
            assert!(kept.path.is_dir());
            assert!(branch_exists(repo.path(), &kept.branch).await);
        }
    }

    /// A worktree is reopened, with what a commit in it writes, while it
    /// is there and its links still lead back; a project in a subdirectory of
    /// the repository reopens it too, and anything else is refused.
    #[tokio::test]
    async fn a_worktree_is_reopened_only_while_it_is_still_linked_to_the_repository() {
        for layout in layouts() {
            let repo = make_minimal_repo().await;
            let root = repo.path().canonicalize().unwrap();
            let git_dir = root.join(".git");
            let handle = create_worktree(&exec(), &direct(), &root, "again", &layout)
                .await
                .unwrap();
            let dir = handle.path().to_path_buf();
            let reopened = reopen_worktree(&platform::LocalFileSystem, &root, &dir, &layout)
                .await
                .unwrap();
            assert_eq!(reopened.dir, dir);
            let git = reopened
                .git
                .expect("a fresh worktree's branch is a loose ref");
            assert_eq!(git.dir, git_dir);
            assert_eq!(git.branch, handle.branch());
            assert_eq!(git, handle.linked().git.unwrap());
            assert!(
                reopen_worktree(&platform::LocalFileSystem, &root.join("sub"), &dir, &layout)
                    .await
                    .is_ok()
            );

            let elsewhere = TempDir::new().unwrap();
            let refused = |project: PathBuf, dir: PathBuf| {
                let layout = layout.clone();
                async move {
                    reopen_worktree(&platform::LocalFileSystem, &project, &dir, &layout)
                        .await
                        .unwrap_err()
                }
            };
            assert!(refused(elsewhere.path().to_path_buf(), dir.clone())
                .await
                .contains("is not a worktree of"));
            assert!(refused(root.clone(), root.join("src"))
                .await
                .contains("is not a worktree of"));
            assert!(
                refused(root.clone(), layout.worktrees_dir(&root).join("never"))
                    .await
                    .contains("is gone")
            );
            assert!(refused(
                root.clone(),
                layout.project_dir(&root).join("worktrees").join("again")
            )
            .await
            .contains("is not a worktree of"));
            assert!(refused(root.clone(), root.join("wkts").join("again"))
                .await
                .contains("is not a worktree of"));

            let commondir = git_dir.join("worktrees/again/commondir");
            std::fs::write(&commondir, format!("{}\n", elsewhere.path().display())).unwrap();
            assert!(refused(root.clone(), dir.clone())
                .await
                .contains("no longer linked"));
            std::fs::write(&commondir, "../..\n").unwrap();
            handle.remove_if_untouched().await;
            assert!(refused(root.clone(), dir).await.contains("is gone"));
        }
    }

    /// The git directory a worktree commits to is the one its links led
    /// back to when it was made, with its entry there and its branch; a
    /// rewritten link leads nowhere accepted.
    #[tokio::test]
    async fn a_worktree_carries_the_git_directory_its_links_led_back_to() {
        for layout in layouts() {
            let repo = make_minimal_repo().await;
            let git_dir = repo.path().join(".git").canonicalize().unwrap();
            let mut handle = create_worktree(&exec(), &direct(), repo.path(), "linked", &layout)
                .await
                .unwrap();
            let linked = handle.linked();
            assert_eq!(linked.dir, handle.path());
            assert_eq!(
                linked.git,
                Some(base::paths::WorktreeGit {
                    dir: git_dir.clone(),
                    admin: git_dir.join("worktrees/linked"),
                    branch: "agix/worktree-linked".into(),
                })
            );
            let providers = exec();
            let linked_git = |handle: &WorktreeHandle| {
                let (path, repo, branch) = (
                    handle.path().to_path_buf(),
                    handle.repo.clone(),
                    handle.branch.clone(),
                );
                let providers = providers.clone();
                async move { linked_worktree_git(&providers, &path, &repo, &branch).await }
            };

            let elsewhere = TempDir::new().unwrap();
            let commondir = git_dir.join("worktrees/linked/commondir");
            std::fs::write(&commondir, format!("{}\n", elsewhere.path().display())).unwrap();
            assert_eq!(linked_git(&handle).await, None);
            std::fs::write(&commondir, "../..\n").unwrap();
            assert_eq!(linked_git(&handle).await, linked.git);
            run_git_capture(&direct(), repo.path(), &["pack-refs", "--all"], &[])
                .await
                .unwrap();
            assert_eq!(linked_git(&handle).await, None, "a packed branch");

            let fake = elsewhere.path().join("linked");
            std::fs::create_dir_all(&fake).unwrap();
            std::fs::write(fake.join("commondir"), format!("{}\n", git_dir.display())).unwrap();
            std::fs::write(
                handle.path().join(".git"),
                format!("gitdir: {}\n", fake.display()),
            )
            .unwrap();
            assert_eq!(linked_git(&handle).await, None);
            handle.cleanup().await;
        }
    }

    #[tokio::test]
    async fn create_worktree_flattens_nested_slug() {
        for layout in layouts() {
            let repo = make_minimal_repo().await;
            let mut handle =
                create_worktree(&exec(), &direct(), repo.path(), "team/feature-a", &layout)
                    .await
                    .unwrap();
            // 嵌套 slug 应当 flatten 成 +
            assert!(
                handle.path().to_string_lossy().contains("team+feature-a"),
                "expected flattened path; got {}",
                handle.path().display()
            );
            assert_eq!(handle.branch(), "agix/worktree-team+feature-a");
            handle.cleanup().await;
        }
    }

    #[tokio::test]
    async fn create_worktree_fails_on_existing_path() {
        for layout in layouts() {
            let repo = make_minimal_repo().await;
            let mut h1 = create_worktree(&exec(), &direct(), repo.path(), "dup", &layout)
                .await
                .unwrap();
            // 第二次同名 slug 应当 AlreadyExists
            let r2 = create_worktree(&exec(), &direct(), repo.path(), "dup", &layout).await;
            assert!(matches!(r2, Err(WorktreeError::AlreadyExists(_))));
            h1.cleanup().await;
        }
    }

    #[tokio::test]
    async fn create_worktree_fails_on_non_git_dir() {
        for layout in layouts() {
            let plain = TempDir::new().unwrap();
            let r = create_worktree(&exec(), &direct(), plain.path(), "probe", &layout).await;
            assert!(
                matches!(
                    r,
                    Err(WorktreeError::NotAGitRepo(_)) | Err(WorktreeError::GitFailed(_))
                ),
                "expected NotAGitRepo or GitFailed, got {r:?}"
            );
        }
    }

    #[tokio::test]
    async fn create_worktree_rejects_invalid_slug_before_touching_disk() {
        for layout in layouts() {
            let repo = make_minimal_repo().await;
            let r = create_worktree(&exec(), &direct(), repo.path(), "../escape", &layout).await;
            assert!(matches!(r, Err(WorktreeError::InvalidSlug(_))));
            // 路径不应当被创建
            assert!(!layout.worktrees_dir(repo.path()).exists());
        }
    }

    #[tokio::test]
    async fn ensure_gitignore_entry_is_idempotent() {
        for layout in layouts() {
            let dir = TempDir::new().unwrap();
            // 第一次：写入
            ensure_gitignore_entry(&exec(), dir.path(), &layout).await;
            let v1 = tokio::fs::read_to_string(dir.path().join(".gitignore"))
                .await
                .unwrap();
            assert!(v1.contains(&layout.gitignore_entry()));
            // 第二次：no-op，文件不变
            ensure_gitignore_entry(&exec(), dir.path(), &layout).await;
            let v2 = tokio::fs::read_to_string(dir.path().join(".gitignore"))
                .await
                .unwrap();
            assert_eq!(v1, v2, "second call should not change file");
        }
    }

    #[tokio::test]
    async fn ensure_gitignore_entry_appends_to_existing() {
        for layout in layouts() {
            let dir = TempDir::new().unwrap();
            tokio::fs::write(dir.path().join(".gitignore"), "node_modules/\n*.log\n")
                .await
                .unwrap();
            ensure_gitignore_entry(&exec(), dir.path(), &layout).await;
            let after = tokio::fs::read_to_string(dir.path().join(".gitignore"))
                .await
                .unwrap();
            assert!(after.contains("node_modules/"), "preserved existing");
            assert!(after.contains("*.log"), "preserved existing");
            assert!(after.contains(&layout.gitignore_entry()), "added new");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_gitignore_that_is_a_symlink_is_not_written_through() {
        let layout = Layout::default();
        let dir = TempDir::new().unwrap();
        let victim = dir.path().join("victim");
        tokio::fs::write(&victim, "keep\n").await.unwrap();
        let repo = dir.path().join("repo");
        tokio::fs::create_dir(&repo).await.unwrap();
        std::os::unix::fs::symlink(&victim, repo.join(".gitignore")).unwrap();
        ensure_gitignore_entry(&exec(), &repo, &layout).await;
        assert_eq!(tokio::fs::read_to_string(&victim).await.unwrap(), "keep\n");
        assert!(tokio::fs::symlink_metadata(repo.join(".gitignore"))
            .await
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[tokio::test]
    async fn every_git_call_and_every_write_reaches_the_providers() {
        for layout in layouts() {
            let repo = make_minimal_repo().await;
            let (providers, log) = watched();

            let mut handle = create_worktree(
                &providers,
                &engine(&providers),
                repo.path(),
                "watched",
                &layout,
            )
            .await
            .unwrap();
            handle.cleanup().await;

            let argv = log.argv.lock().unwrap().clone();
            assert!(
                argv.iter().all(|a| a[0] == "git"),
                "nothing but git should be spawned: {argv:?}"
            );
            let options = base::engine_exec::ENGINE_GIT_OPTIONS;
            assert!(
                argv.iter().all(|a| a[1..]
                    .starts_with(&options.iter().map(|o| o.to_string()).collect::<Vec<_>>())),
                "every git call carries the engine's options: {argv:?}"
            );
            let subcommands: Vec<&[String]> =
                argv.iter().map(|a| &a[1 + options.len()..]).collect();
            for expected in [
                vec!["rev-parse", "--show-toplevel"],
                vec!["worktree", "add"],
                vec!["branch", "-D"],
            ] {
                assert!(
                    subcommands.iter().any(|a| a
                        .iter()
                        .zip(&expected)
                        .all(|(got, want)| got == want)
                        && a.len() >= expected.len()),
                    "`git {}` never reached the Process contract; got {argv:?}",
                    expected.join(" ")
                );
            }

            let written = log.written.lock().unwrap().clone();
            assert_eq!(
                written
                    .iter()
                    .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
                vec![".gitignore".to_string()],
                "the .gitignore write is the one write this path makes, and it has \
                 to go through the FileSystem contract"
            );
        }
    }

    /// `git worktree add` runs the repository's post-checkout hook, and the
    /// engine's git runs outside the sandbox.
    #[cfg(unix)]
    #[tokio::test]
    async fn creating_a_worktree_does_not_run_the_repositorys_hooks() {
        for layout in layouts() {
            use std::os::unix::fs::PermissionsExt;
            let repo = make_minimal_repo().await;
            let marker = repo.path().join("hook-ran");
            let hook = repo.path().join(".git").join("hooks").join("post-checkout");
            std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
            std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

            let mut handle = create_worktree(&exec(), &direct(), repo.path(), "hooked", &layout)
                .await
                .unwrap();
            handle.cleanup().await;
            assert!(!marker.exists(), "post-checkout ran");
        }
    }

    /// In a sandboxed session the worktree's git runs in the sandbox, writing
    /// only the git directory and the worktrees directory, and the worktree it
    /// makes is one git can work in; untouched, it goes with its branch.
    #[tokio::test]
    async fn a_sandboxed_git_still_creates_and_removes_a_working_worktree() {
        for layout in layouts() {
            if !crate::bash::sandbox_status().starts_with("available") {
                return;
            }
            let repo = make_minimal_repo().await;
            let state = TempDir::new().unwrap();
            let git = EngineExec::new(
                exec(),
                Confinement::Sandboxed {
                    policy: base::interface::exec::SandboxPolicy {
                        state_root: Some(state.path().to_path_buf()),
                        ..Default::default()
                    },
                    require_enforcement: true,
                    tmpdir: None,
                },
                ProjectTrust::Trusted,
            );

            let handle = create_worktree(&exec(), &git, repo.path(), "boxed", &layout)
                .await
                .unwrap();
            assert!(handle.path().join("README.md").is_file());
            let head = handle
                .git_in_worktree(&["rev-parse", "--abbrev-ref", "HEAD"])
                .await
                .unwrap();
            assert_eq!(head.trim(), "agix/worktree-boxed");

            let path = handle.path().to_path_buf();
            assert_eq!(handle.remove_if_untouched().await, None);
            assert!(!path.exists());
            let direct = |args: &'static [&'static str]| {
                let root = repo.path().to_path_buf();
                async move { run_git_capture(&direct(), &root, args, &[]).await.unwrap() }
            };
            assert!(!direct(&["branch", "--list"]).await.contains("boxed"));
            assert!(!direct(&["worktree", "list", "--porcelain"])
                .await
                .contains("boxed"));
        }
    }

    /// A git that must run in the sandbox and cannot is not run at all, and
    /// the worktree is refused with the reason.
    #[tokio::test]
    async fn a_worktree_is_refused_when_its_git_must_be_sandboxed_and_cannot_be() {
        for layout in layouts() {
            let repo = make_minimal_repo().await;
            let (mut providers, log) = watched();
            providers.sandbox = std::sync::Arc::new(base::interface::exec::in_process::NoSandbox);
            let git = EngineExec::new(
                providers.clone(),
                Confinement::Sandboxed {
                    policy: Default::default(),
                    require_enforcement: true,
                    tmpdir: None,
                },
                ProjectTrust::Trusted,
            );

            let r = create_worktree(&providers, &git, repo.path(), "refused", &layout).await;
            assert!(
                matches!(&r, Err(WorktreeError::GitRefused(why)) if why.contains("sandbox.require")),
                "{r:?}"
            );
            assert!(log.argv.lock().unwrap().is_empty(), "git ran anyway");
            assert!(!layout.worktrees_dir(repo.path()).exists());
        }
    }
}
