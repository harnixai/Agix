//! 会话状态的契约：引擎从设置推出来的 `SessionEnv`、写进会话日志的 `DurableState`、
//! 工具与策略经 `Arc<dyn Session>` 看到的 `Session` trait，以及绑定任务生命周期的
//! `TaskScope` / `TaskLease`。实现在 `session-state` crate，是这个 trait 唯一的实现。

use super::goal::{GoalAction, GoalError, GoalView, TurnOrigin};
use super::ledger::SessionLedger;
use super::task::{RunningStatus, RunningTask, TaskInfo, TaskType, TodoItem};
use crate::interface::exec::ExecProviders;
use crate::permission::PermissionMode;
use crate::tool::Tool;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// What the engine running a session derives from its settings and scene.
/// Each engine brings its own, and attaching an engine replaces the
/// session's whole.
#[derive(Debug, Clone, Default)]
pub struct SessionEnv {
    pub cwd: PathBuf,
    /// Directories a relative deny or ask path rule is also read against,
    /// besides `cwd`: the project root, for a session working in one of its
    /// worktrees. An allow is read against `cwd` alone.
    pub refusal_rule_roots: Vec<PathBuf>,
    /// 用户级附加可写目录（`files.extra_dirs`）—— Tool 在做路径校验时用
    pub additional_writable_dirs: Vec<PathBuf>,
    /// Directories outside the project the file tools may read without
    /// asking — `files.read_dirs`.
    pub read_directories: Vec<PathBuf>,
    /// `files.read_dirs` holding `/`.
    pub read_anywhere: bool,
    /// Package and build caches a sandboxed command may write —
    /// `sandbox.cache_dirs`; a `~/` path is expanded when the command
    /// runs.
    pub cache_directories: Vec<PathBuf>,
    /// Nobody answers this session's questions or permission prompts — see
    /// `SceneSpec::interactive`.
    pub unattended: bool,
    /// The permission mode the engine is configured to run in.
    pub base_mode: PermissionMode,
    /// The host's skill directories this session lists: readable without
    /// asking, like `read_directories`, and changed without a rebuild.
    pub skill_dirs: LiveDirs,
    /// Every host skill directory a session in this process has listed, and
    /// the directory of each of the host's own packages: no tool writes
    /// under one, in any permission mode, and no sandboxed command.
    pub protected_dirs: LiveDirs,
}

/// A list of directories shared by everything handed a clone and replaced
/// in place, so a session's tools see a change without the session being
/// rebuilt.
#[derive(Debug, Clone, Default)]
pub struct LiveDirs(Arc<std::sync::RwLock<Vec<PathBuf>>>);

impl LiveDirs {
    pub fn get(&self) -> Vec<PathBuf> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set(&self, dirs: Vec<PathBuf>) {
        *self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = dirs;
    }

    /// Adds those of `dirs` not listed yet; removes nothing.
    pub fn extend(&self, dirs: impl IntoIterator<Item = PathBuf>) {
        let mut held = self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for dir in dirs {
            if !held.contains(&dir) {
                held.push(dir);
            }
        }
    }

    /// The listed directory `path` is in or is.
    pub fn holding(&self, path: &Path) -> Option<PathBuf> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|dir| path.starts_with(dir))
            .cloned()
    }
}

impl SessionEnv {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            ..Self::default()
        }
    }
}

/// Log namespace and event under which [`DurableState`] is recorded.
pub const SESSION_STATE_NS: &str = "agix.session_state";
pub const SESSION_STATE_EVENT: &str = "change";

/// The part of a session's state that outlives its engine: what the model
/// set up for itself, which an engine brought back from the log must still
/// hold. The rest is derived from settings again or ends with the engine.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DurableState {
    #[serde(default)]
    pub plan: Option<PlanState>,
    #[serde(default)]
    pub todos: Vec<TodoItem>,
    /// The deferred tools `ToolSearch` loaded, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activated_tools: Vec<String>,
}

/// In plan mode, and where `ExitPlanMode` returns to: `None` is `Default`,
/// for a session that started out in plan mode.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanState {
    pub exit_to: Option<PermissionMode>,
}

/// What tools and permission policies see of the session they run in. The
/// layering needs the trait, not an extension point: `session-state` holds
/// the one implementation.
pub trait Session: Send + Sync + std::fmt::Debug {
    /// What the attached engine derived from its settings and scene; empty
    /// until one is attached.
    fn env(&self) -> Arc<SessionEnv>;

    /// The providers the attached engine's tool calls run on, if one is
    /// attached.
    fn exec(&self) -> Option<ExecProviders>;

    /// The tool this session's calls named `name` dispatch to, if the
    /// attached engine's registry holds it.
    fn tool(&self, name: &str) -> Option<Arc<dyn Tool>>;

    /// 读当前模式。
    fn permission_mode(&self) -> PermissionMode;

    /// 直接设模式，并忘掉进入计划模式前的那个。
    fn set_permission_mode(&self, mode: PermissionMode);

    /// 进计划模式，记下进入前的模式给 [`exit_plan_mode`](Self::exit_plan_mode)。
    /// 已经在计划模式里时什么都不改。
    fn enter_plan_mode(&self);

    /// 退出计划模式，回到进入前的模式；会话一开始就在计划模式里的回到 `Default`。
    fn exit_plan_mode(&self) -> PermissionMode;

    /// The mode outside plan a host switched the session to since its engine
    /// was attached; `None` leaves it to the engine's settings. What a
    /// delegate spawned now starts from.
    fn host_permission_mode(&self) -> Option<PermissionMode>;

    /// The model the session's next call asks for, when a host chose one;
    /// `None` leaves it on the model its engine was built with.
    fn model_choice(&self) -> Option<String>;

    /// Register work this session is running, under `cancel` — the token the
    /// work itself observes, so stopping the task stops the work.
    ///
    /// Its start is announced at once, except a shell command's or a
    /// monitor's: most of those finish in milliseconds, and a list that
    /// flickered with each one would hide the ones worth seeing. Those are
    /// announced only if still running a second later, and a task never
    /// announced ends without a word. From then on what they print goes out
    /// as `task_progress`, at most once a second, and once more as they end.
    fn register_task(
        &self,
        task_id: String,
        task_type: TaskType,
        cancel: CancellationToken,
        detail: serde_json::Value,
    ) -> Arc<RunningTask>;

    /// Record that a task's work is over, having ended with `status`: its
    /// output closes and `task_finished` goes out. A task already ended —
    /// stopped by `cancel_running_task`, say, before its own code got here —
    /// keeps the ending it has. Finished tasks beyond the newest few are
    /// forgotten.
    fn finish_task(&self, task_id: &str, status: RunningStatus);

    /// 找一个 running task。返回 Arc 让 caller 读快照（output / status）。
    fn find_running_task(&self, task_id: &str) -> Option<Arc<RunningTask>>;

    /// Stop a running task: cancel what it runs under and record it as
    /// stopped. `false` if there is no such task or it had already ended.
    ///
    /// `task_finished` waits for the work to wind down — what it prints on
    /// the way out is part of its output — up to a grace period.
    fn cancel_running_task(&self, task_id: &str) -> bool;

    /// Ids of the background work still running — sub-agents, teammates,
    /// programs, monitors; not a foreground shell command — sorted.
    fn running_background_task_ids(&self) -> Vec<String>;

    /// Every task this session knows — running, and the most recently ended —
    /// oldest first.
    fn task_infos(&self) -> Vec<TaskInfo>;

    /// Tell this session's agent something it did not ask for in this turn,
    /// such as a background sub-agent having finished. A running turn reads
    /// it at its next step boundary; otherwise the next turn opens with it.
    ///
    /// A notice under `key` can be [retracted](Self::retract_notice) before
    /// it is read: one about a task the agent has since waited for tells it
    /// nothing new.
    fn post_notice(&self, key: Option<String>, text: String);

    /// Withdraw the unread notice under `key`; `false` if there was none —
    /// never posted, or already read.
    fn retract_notice(&self, key: &str) -> bool;

    fn take_notices(&self) -> Vec<String>;

    /// Record that `Read` just read `path` over `offset`/`limit`, with its
    /// mtime: a later read of the same range of an unchanged file is answered
    /// from the cache, and `Edit`/`Write` know it was read and when.
    fn record_read_with_range(&self, path: &Path, offset: Option<usize>, limit: Option<usize>);

    /// Record that `Edit` / `Write` just changed `path`, so the next edit of
    /// it does not ask for a `Read` first or call the session's own change
    /// an outside one.
    fn record_write(&self, path: &Path);

    /// 检查文件是否已读且未过期。
    ///
    /// - 文件不存在于磁盘 → 跳过检查（返回 None）
    /// - 文件已读且 mtime 一致 → 返回 None（通过）
    /// - 文件未读 → 返回 "must be read" 错误信息
    /// - 文件已读但 mtime 不同 → 返回 "modified since read" 错误信息
    fn check_read_staleness(&self, path: &Path) -> Option<&'static str>;

    /// Check read dedup with explicit offset/limit range.
    /// Only deduplicates when both the mtime AND the exact offset/limit range match.
    fn check_read_dedup_with_range(
        &self,
        path: &Path,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> bool;

    /// 给定路径是否与最近一次读取的文件一致。
    /// 仅检查路径相等；mtime 一致性由调用方通过 `check_read_dedup_with_range` 保证。
    fn is_consecutive_read(&self, path: &Path) -> bool;

    /// 读取当前 todo 列表（克隆给调用方）
    fn todos(&self) -> Vec<TodoItem>;

    /// TodoWrite 工具用：一次性替换整个列表。
    /// 如果全部 completed 则自动清空。
    fn set_todos(&self, items: Vec<TodoItem>);

    /// 当前已激活的 deferred 工具名（克隆 set；调用方只读）。
    fn activated_tools(&self) -> HashSet<String>;

    /// ToolSearch 命中后调用：把工具加进 activated 集合。
    /// 多次调用 idempotent；空集合也允许。
    fn activate_tools(&self, names: Vec<String>);

    fn ledger(&self) -> std::sync::MutexGuard<'_, SessionLedger>;

    /// The goal as hosts and the model see it.
    fn goal_view(&self) -> Option<GoalView>;

    fn goal_create(
        &self,
        objective: String,
        limits: super::goal::GoalLimits,
        origin: TurnOrigin,
    ) -> Result<GoalView, GoalError>;

    /// An edit may give the goal no more than `max_rounds_limit` rounds.
    fn goal_update(
        &self,
        goal_id: &str,
        revision: u64,
        action: GoalAction,
        origin: TurnOrigin,
        max_rounds_limit: u32,
    ) -> Result<GoalView, GoalError>;
}

/// Stop `task` from outside its lease, posting `notice` as its end's
/// notice before it ends. Of this and the lease, whichever comes first
/// ends the task and notifies; the other leaves both to it.
pub fn stop_with_notice(
    session: &dyn Session,
    task: &RunningTask,
    notice: impl FnOnce(&RunningTask) -> String,
) {
    task.cancel.cancel();
    if !task.claim_end() {
        return;
    }
    if !task.has_ended() {
        session.post_notice(Some(task.task_id.clone()), notice(task));
    }
    session.cancel_running_task(&task.task_id);
}

/// A task that lasts as long as this value: registered on creation, ended on
/// drop — stopped if its token was cancelled, done otherwise. For work bounded
/// by one call, like a shell command.
pub struct TaskScope {
    session: Arc<dyn Session>,
    task: Arc<RunningTask>,
}

impl TaskScope {
    pub fn new(
        session: Arc<dyn Session>,
        task_id: impl Into<String>,
        task_type: TaskType,
        cancel: tokio_util::sync::CancellationToken,
        detail: serde_json::Value,
    ) -> Self {
        let task = session.register_task(task_id.into(), task_type, cancel, detail);
        Self { session, task }
    }

    pub fn task(&self) -> &Arc<RunningTask> {
        &self.task
    }
}

impl Drop for TaskScope {
    fn drop(&mut self) {
        let status = if self.task.cancel.is_cancelled() {
            RunningStatus::Cancelled
        } else {
            RunningStatus::Completed
        };
        self.session.finish_task(&self.task.task_id, status);
    }
}

/// A task whose holder says how it ended: registered on creation, ended by
/// [`conclude`](Self::conclude). One dropped without concluding — its work
/// panicked, was dropped from a queue, or its holder simply went away — ends
/// as stopped, and the parent hears so when the lease was taken
/// [notifying](Self::notifying) it. No task held by a lease is left running.
pub struct TaskLease {
    session: Arc<dyn Session>,
    task: Arc<RunningTask>,
    stopped_notice: Option<StoppedNotice>,
    concluded: bool,
}

type StoppedNotice = Box<dyn Fn(&RunningTask) -> String + Send + Sync>;

impl TaskLease {
    pub fn new(
        session: Arc<dyn Session>,
        task_id: impl Into<String>,
        task_type: TaskType,
        cancel: tokio_util::sync::CancellationToken,
        detail: serde_json::Value,
    ) -> Self {
        let task = session.register_task(task_id.into(), task_type, cancel, detail);
        Self {
            session,
            task,
            stopped_notice: None,
            concluded: false,
        }
    }

    /// The notice the parent is posted when the lease is dropped unconcluded.
    pub fn notifying(
        mut self,
        notice: impl Fn(&RunningTask) -> String + Send + Sync + 'static,
    ) -> Self {
        self.stopped_notice = Some(Box::new(notice));
        self
    }

    pub fn task(&self) -> &Arc<RunningTask> {
        &self.task
    }

    /// End the task with `status`. `notice` is posted to the session first,
    /// so whoever wakes on the task's end finds it there to withdraw; a task
    /// already stopped from outside gets none, since whoever stopped it knows.
    pub fn conclude(mut self, status: RunningStatus, notice: Option<String>) {
        self.concluded = true;
        if !self.task.claim_end() {
            return;
        }
        if let Some(notice) = notice.filter(|_| !self.task.has_ended()) {
            self.session
                .post_notice(Some(self.task.task_id.clone()), notice);
        }
        self.session.finish_task(&self.task.task_id, status);
    }
}

impl Drop for TaskLease {
    fn drop(&mut self) {
        if self.concluded || !self.task.claim_end() {
            return;
        }
        if let Some(notice) = self.stopped_notice.as_ref() {
            if !self.task.has_ended() {
                self.session
                    .post_notice(Some(self.task.task_id.clone()), notice(&self.task));
            }
        }
        self.session
            .finish_task(&self.task.task_id, RunningStatus::Cancelled);
    }
}
