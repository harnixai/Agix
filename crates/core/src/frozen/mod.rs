//! Session-start frozen environment snapshot — the data sources for system prompt
//! segments [4] and [5].
//!
//! All git subcommands have a 1.5 s timeout; failures silently skip the
//! related field. **Not refreshed during a session** —
//! mid-session git commits are not updated.

use crate::engine_exec::{read_head, EngineExec, EngineExecError, GitAccess};
use crate::paths::ConfigPaths;
use crate::text::truncate_chars;
use std::path::{Path, PathBuf};
use std::time::Duration;
use time::format_description::well_known::Iso8601;

/// 一段 AGENTS.md 文件（路径 + 内容）。多条按"远到近"顺序排列。
#[derive(Debug, Clone)]
pub struct MemoryFileEntry {
    pub path: PathBuf,
    pub content: String,
}

/// Git status output character limit.
const MAX_GIT_STATUS_CHARS: usize = 2000;

/// How long each git subcommand collecting the context may take.
const GIT_TIMEOUT: Duration = Duration::from_millis(1500);

/// 会话级冻结的环境信息。一次会话只算一次；turn 之间不变。
#[derive(Debug, Clone, Default)]
pub struct FrozenContext {
    pub cwd: PathBuf,
    pub is_git: bool,
    pub is_worktree: bool,
    pub git_branch: Option<String>,
    pub git_main_branch: Option<String>,
    pub git_user_name: Option<String>,
    pub git_status: Option<String>,
    pub git_log: Option<String>,
    pub platform: String,
    pub shell: Option<String>,
    /// 这台机器上用户的 home 目录——和 `platform`/`shell` 一样是**环境事实**，
    /// 进 system prompt 给模型交代环境，不是 Agix 存东西的地方（那些根一律
    /// 由调用方注入，见 [`crate::paths::ConfigPaths`]）。收进快照而不是在拼
    /// prompt 时现读，是因为快照可以被测试替换，而 env 不能。
    pub home_dir: Option<String>,
    pub today: String,
    pub memory_blocks: Vec<MemoryFileEntry>,
    pub user_email: Option<String>,
    /// 模型写记忆的目录，即 [`crate::memory::MemoryStore::for_paths`] 的全局一层。
    pub memory_dir: PathBuf,
    /// 全局与项目两层 `MEMORY.md` 合起来的索引（截到 8 KB），场景的 `memory`
    /// 提醒带的就是它。
    pub memory_index: Option<String>,
    /// Memory file paths already surfaced in prior turns. Used to deduplicate
    /// and avoid re-injecting the same memories every turn.
    pub already_surfaced: std::collections::HashSet<String>,
    /// output style 已加载的内容（`name` 来自 EngineConfig，
    /// 由 `collect_output_style` 从 user/project 级别 `output-styles/<name>.md`
    /// 读取）。None = 没指定或文件不存在。
    pub output_style: Option<OutputStyle>,
}

/// 一个 output-style 文件的加载结果。注入 system prompt 时用。
#[derive(Debug, Clone)]
pub struct OutputStyle {
    pub name: String,
    pub source: OutputStyleSource,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutputStyleSource {
    /// `~/.agix/scenes/<scope>/output-styles/<name>.md`
    User,
    /// `<cwd>/.agix/output-styles/<name>.md`
    Project,
}

/// FrozenContext 收集选项。
#[derive(Debug, Clone, Default)]
pub struct CollectOptions {
    /// 是否在 cwd 之上向上爬找 `AGENTS.md`（含 git root）。默认 true。
    /// monorepo 子目录不想吃父级 monorepo 上下文时设为 false。
    pub walk_up_claude_md: bool,

    /// 启动时按名称加载 output style（先项目级再用户级）。
    /// None = 不加载。
    pub output_style: Option<String>,

    /// cwd 不是项目（无项目会话的专属工作目录）：不跑 git、不找 `AGENTS.md`，
    /// 项目级的 skills 与 output style 也没有。
    pub outside_project: bool,
}

impl CollectOptions {
    pub fn defaults() -> Self {
        Self {
            walk_up_claude_md: true,
            output_style: None,
            outside_project: false,
        }
    }
}

/// The git-derived subset of `FrozenContext` — factored out so
/// `FrozenContext::refresh_git` can re-run just this part instead of the
/// whole `collect_environment`, which also reads the platform and the date.
struct GitFields {
    is_git: bool,
    is_worktree: bool,
    git_branch: Option<String>,
    git_main_branch: Option<String>,
    git_user_name: Option<String>,
    git_status: Option<String>,
    git_log: Option<String>,
}

impl GitFields {
    fn none() -> Self {
        Self {
            is_git: false,
            is_worktree: false,
            git_branch: None,
            git_main_branch: None,
            git_user_name: None,
            git_status: None,
            git_log: None,
        }
    }
}

/// `git <args>` in `cwd`: its trimmed stdout, or `None` on any failure,
/// timeout, non-zero exit or empty output.
async fn run_git_text(engine: &EngineExec, cwd: &Path, args: &[&str]) -> Option<String> {
    let out = engine
        .git(cwd, GitAccess::ReadOnly, args, GIT_TIMEOUT)
        .await
        .ok()?;
    if !out.status.success {
        return None;
    }
    let s = out.stdout_lossy().trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Whether `cwd` is in a work tree, as git says; `Err` when the engine may
/// not run git there at all.
async fn run_git_check(engine: &EngineExec, cwd: &Path) -> Result<bool, EngineExecError> {
    match engine
        .git(
            cwd,
            GitAccess::ReadOnly,
            &["rev-parse", "--is-inside-work-tree"],
            GIT_TIMEOUT,
        )
        .await
    {
        Err(refused @ EngineExecError::Refused(_)) => Err(refused),
        Err(_) => Ok(false),
        Ok(out) => Ok(out.status.success && out.stdout_lossy().trim() == "true"),
    }
}

/// The branch checked out in `cwd`: from git, or where the engine may not run
/// git, from the repository's `HEAD` file.
async fn current_branch(engine: &EngineExec, cwd: &Path) -> Option<String> {
    match engine
        .git(
            cwd,
            GitAccess::ReadOnly,
            &["symbolic-ref", "--short", "HEAD"],
            GIT_TIMEOUT,
        )
        .await
    {
        Err(EngineExecError::Refused(_)) => {
            read_head(&*engine.exec().filesystem, cwd).await?.branch
        }
        Err(_) => None,
        Ok(out) => {
            let branch = out.stdout_lossy().trim().to_string();
            (out.status.success && !branch.is_empty()).then_some(branch)
        }
    }
}

/// A worktree has `.git` as a file holding a `gitdir:` line, not a directory.
async fn is_worktree(engine: &EngineExec, cwd: &Path) -> bool {
    engine
        .exec()
        .filesystem
        .read_to_string(&cwd.join(".git"))
        .await
        .is_ok_and(|content| content.trim().starts_with("gitdir:"))
}

/// `origin/HEAD` if there is one, else whichever of `main` / `master` exists.
async fn detect_main_branch(engine: &EngineExec, cwd: &Path) -> Option<String> {
    if let Some(out) = run_git_text(
        engine,
        cwd,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .await
    {
        if let Some(name) = out.strip_prefix("origin/") {
            return Some(name.to_string());
        }
        return Some(out);
    }
    for &cand in &["main", "master"] {
        let exists = engine
            .git(
                cwd,
                GitAccess::ReadOnly,
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{cand}"),
                ],
                GIT_TIMEOUT,
            )
            .await
            .is_ok_and(|out| out.status.success);
        if exists {
            return Some(cand.to_string());
        }
    }
    None
}

/// The user's home directory and shell name on this machine.
#[expect(
    clippy::disallowed_methods,
    reason = "the home directory and the shell are facts about the machine the prompt describes"
)]
fn home_and_shell() -> (Option<String>, Option<String>) {
    let home_dir = std::env::var("HOME").ok();
    let shell = std::env::var("SHELL").ok().map(|p| {
        Path::new(&p)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(p)
    });
    (home_dir, shell)
}

async fn collect_git_fields(engine_git: &EngineExec, cwd: &Path) -> GitFields {
    match run_git_check(engine_git, cwd).await {
        Ok(true) => {}
        Ok(false) => return GitFields::none(),
        // The engine may not run this repository's git: its branch is all
        // the snapshot takes, read from `HEAD`.
        Err(_) => {
            let Some(head) = read_head(&*engine_git.exec().filesystem, cwd).await else {
                return GitFields::none();
            };
            return GitFields {
                is_git: true,
                is_worktree: head.linked,
                git_branch: head.branch,
                ..GitFields::none()
            };
        }
    }
    let is_git = true;
    let is_worktree = is_worktree(engine_git, cwd).await;
    let git_branch = current_branch(engine_git, cwd).await;
    let git_main_branch = detect_main_branch(engine_git, cwd).await;
    let git_user_name = run_git_text(engine_git, cwd, &["config", "user.name"]).await;
    let git_status = run_git_text(
        engine_git,
        cwd,
        &["--no-optional-locks", "status", "--short"],
    )
    .await
    .map(|s| truncate_chars(&s, MAX_GIT_STATUS_CHARS, "\n... (truncated)"));
    let git_log = run_git_text(
        engine_git,
        cwd,
        &["--no-optional-locks", "log", "--oneline", "-n", "5"],
    )
    .await;
    GitFields {
        is_git,
        is_worktree,
        git_branch,
        git_main_branch,
        git_user_name,
        git_status,
        git_log,
    }
}

impl FrozenContext {
    /// 收集环境快照里不依赖指令文件、记忆库的那部分：git、平台、home、日期、
    /// 邮箱、记忆目录。AGENTS.md、记忆索引、output style 由 runtime 填。
    /// 所有 IO 错误都吞掉转 None / 默认值；这个函数不该 panic、不该 fail。
    pub async fn collect_environment(
        cwd: PathBuf,
        opts: &CollectOptions,
        paths: &ConfigPaths,
        environment: &dyn crate::interface::environment::Environment,
        engine_git: &EngineExec,
    ) -> Self {
        let project = (!opts.outside_project).then_some(cwd.as_path());
        let git = match project {
            Some(project) => collect_git_fields(engine_git, project).await,
            None => GitFields::none(),
        };

        let (home_dir, shell) = home_and_shell();

        let today = environment
            .now()
            .format(&Iso8601::DATE)
            .unwrap_or_else(|_| "unknown".to_string());

        let user_email = if git.is_git {
            run_git_text(engine_git, &cwd, &["config", "user.email"]).await
        } else {
            None
        };

        Self {
            is_git: git.is_git,
            is_worktree: git.is_worktree,
            git_branch: git.git_branch,
            git_main_branch: git.git_main_branch,
            git_user_name: git.git_user_name,
            git_status: git.git_status,
            git_log: git.git_log,
            platform: std::env::consts::OS.to_string(),
            shell,
            home_dir,
            today,
            user_email,
            memory_dir: paths.global_memory_dir(),
            cwd,
            ..Self::default()
        }
    }
}

impl FrozenContext {
    /// Re-run just the git-derived fields (branch/status/log/worktree-ness),
    /// leaving memory/skills/output-style untouched. Exists to partially
    /// relax this module's "not refreshed during a session" characteristic
    /// for the one field that goes stale fastest in a long-running session —
    /// callers are expected to invoke this only after detecting a
    /// git-mutating command in the turn just completed (see
    /// `runtime::turn`'s caller), not on every turn: a git status/log/branch
    /// re-derivation is real subprocess I/O (bounded by the same 1.5s
    /// per-subcommand timeout `collect_environment` uses), not free.
    pub async fn refresh_git(&mut self, engine_git: &EngineExec) {
        let git = collect_git_fields(engine_git, &self.cwd).await;
        self.is_git = git.is_git;
        self.is_worktree = git.is_worktree;
        self.git_branch = git.git_branch;
        self.git_main_branch = git.git_main_branch;
        self.git_user_name = git.git_user_name;
        self.git_status = git.git_status;
        self.git_log = git.git_log;
    }

    /// Re-derive only `git_status` and `git_branch` — the two fields that get
    /// re-shown to the model on *every* turn (`git_status` through the
    /// per-turn `<system-reminder>`, `git_branch` through the scene's `<env>`
    /// block) and therefore the two that must not be a session-start
    /// snapshot.
    ///
    /// A stale git status is worse than an absent one: the model does not
    /// treat it as a possibly-outdated hint, it treats it as current fact, so
    /// after a commit it keeps reasoning about files that are no longer dirty
    /// and re-suggesting work that is already done. The wider
    /// [`FrozenContext::refresh_git`] fixes that too, but it is called only
    /// after the *model itself* runs a git-mutating `Bash` command — which
    /// misses every ordinary cause of drift: the user committing in another
    /// terminal, an editor writing a file, and the model's own `Write`/`Edit`
    /// calls, none of which go through `Bash`.
    ///
    /// Two subcommands rather than [`refresh_git`](Self::refresh_git)'s six:
    /// `git log`/`main-branch detection`/`user.name` do not appear in
    /// anything injected per turn and change far more slowly, so paying for
    /// them every turn buys nothing. Skips all I/O when the session is not in
    /// a git repository at all — the common non-repo case stays free.
    pub async fn refresh_git_status(&mut self, engine_git: &EngineExec) {
        if !self.is_git {
            return;
        }
        self.git_branch = current_branch(engine_git, &self.cwd).await;
        self.git_status = run_git_text(
            engine_git,
            &self.cwd,
            &["--no-optional-locks", "status", "--short"],
        )
        .await
        .map(|s| truncate_chars(&s, MAX_GIT_STATUS_CHARS, "\n... (truncated)"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roots under a tempdir. Tests never inherit the running user's home.
    fn test_paths(root: &Path) -> ConfigPaths {
        ConfigPaths::new(root.join(".agix"), root.join(".agix"), "code")
    }

    use tempfile::TempDir;

    /// Nothing scripted: every git the engine asks for fails, as it does
    /// outside a repository.
    fn no_git() -> EngineExec {
        EngineExec::new(
            crate::interface::exec::ExecProviders::in_process(),
            Default::default(),
            Default::default(),
        )
    }

    /// The git context is whatever the session's providers answer: its git
    /// through `exec.process`, its `.git` through `exec.filesystem`.
    #[tokio::test]
    async fn the_git_context_comes_from_the_sessions_providers() {
        use crate::engine_exec::ENGINE_GIT_OPTIONS;
        use crate::interface::exec::in_process::{
            InMemoryFileSystem, ScriptedProcess, ScriptedRun,
        };
        let dir = TempDir::new().unwrap();
        let git = |args: &str| format!("git {} {args}", ENGINE_GIT_OPTIONS.join(" "));
        let process = ScriptedProcess::new()
            .with(
                git("rev-parse --is-inside-work-tree"),
                ScriptedRun::ok("true\n"),
            )
            .with(git("symbolic-ref --short HEAD"), ScriptedRun::ok("topic\n"))
            .with(
                git("--no-optional-locks status --short"),
                ScriptedRun::ok(" M a.rs\n"),
            );
        let exec = crate::interface::exec::ExecProviders {
            process: std::sync::Arc::new(process.clone()),
            filesystem: std::sync::Arc::new(
                InMemoryFileSystem::new().with_file(dir.path().join(".git"), "gitdir: /x\n"),
            ),
            ..crate::interface::exec::ExecProviders::in_process()
        };
        let ctx = FrozenContext::collect_environment(
            dir.path().to_path_buf(),
            &CollectOptions::defaults(),
            &test_paths(dir.path()),
            &crate::interface::environment::SystemEnvironment,
            &EngineExec::new(exec, Default::default(), Default::default()),
        )
        .await;
        assert!(ctx.is_git);
        assert!(ctx.is_worktree);
        assert_eq!(ctx.git_branch.as_deref(), Some("topic"));
        assert_eq!(ctx.git_status.as_deref(), Some("M a.rs"));
        assert!(process.seen().iter().all(|argv| argv.starts_with(&git(""))));
    }

    /// An untrusted project's repository, no sandbox: no git runs, the branch
    /// comes from `HEAD`.
    #[tokio::test]
    async fn an_untrusted_repository_outside_a_sandbox_gives_only_its_branch() {
        use crate::interface::exec::in_process::{InMemoryFileSystem, ScriptedProcess};
        let dir = TempDir::new().unwrap();
        let process = ScriptedProcess::new();
        let exec = crate::interface::exec::ExecProviders {
            process: std::sync::Arc::new(process.clone()),
            filesystem: std::sync::Arc::new(
                InMemoryFileSystem::new()
                    .with_file(dir.path().join(".git/HEAD"), "ref: refs/heads/topic\n"),
            ),
            ..crate::interface::exec::ExecProviders::in_process()
        };
        let engine = EngineExec::new(
            exec,
            crate::engine_exec::Confinement::Unconfined,
            crate::permission::ProjectTrust::Untrusted,
        );
        let mut ctx = FrozenContext::collect_environment(
            dir.path().to_path_buf(),
            &CollectOptions::defaults(),
            &test_paths(dir.path()),
            &crate::interface::environment::SystemEnvironment,
            &engine,
        )
        .await;
        assert!(ctx.is_git);
        assert_eq!(ctx.git_branch.as_deref(), Some("topic"));
        assert!(ctx.git_status.is_none() && ctx.git_log.is_none() && ctx.user_email.is_none());
        ctx.refresh_git_status(&engine).await;
        assert_eq!(ctx.git_branch.as_deref(), Some("topic"));
        assert!(process.seen().is_empty(), "git ran: {:?}", process.seen());
    }

    #[tokio::test]
    async fn collects_basic_fields_for_arbitrary_dir() {
        let dir = TempDir::new().unwrap();
        let ctx = FrozenContext::collect_environment(
            dir.path().to_path_buf(),
            &CollectOptions::defaults(),
            &test_paths(dir.path()),
            &crate::interface::environment::SystemEnvironment,
            &no_git(),
        )
        .await;
        assert!(!ctx.platform.is_empty());
        assert!(ctx.today.len() == 10); // YYYY-MM-DD
                                        // 不在 git 仓库下，is_git=false 且 git_* 字段都 None
        assert!(!ctx.is_git);
        assert!(ctx.git_status.is_none());
    }

    /// Outside a git repository the refresh must cost nothing and invent
    /// nothing — the common non-repo session pays no subprocess per turn.
    #[tokio::test]
    async fn refresh_git_status_is_a_noop_outside_a_repo() {
        let dir = TempDir::new().unwrap();
        let mut ctx = FrozenContext::collect_environment(
            dir.path().to_path_buf(),
            &CollectOptions::defaults(),
            &test_paths(dir.path()),
            &crate::interface::environment::SystemEnvironment,
            &no_git(),
        )
        .await;
        assert!(!ctx.is_git);
        ctx.refresh_git_status(&no_git()).await;
        assert!(ctx.git_status.is_none());
        assert!(ctx.git_branch.is_none());
    }
}
