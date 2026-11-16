//! 一个会话的可变状态：权限模式、todo、任务表、通知、读缓存、目标、记账；
//! `base::context::Session` 的唯一实现。runtime 与会话池持具体类型，工具与策略经
//! `dyn Session` 看到它。

mod notices;
mod read_cache;
mod tasks;
#[cfg(feature = "testing")]
pub mod testing;

use base::context::goal::{
    Goal, GoalAction, GoalError, GoalRound, GoalSlot, GoalView, TurnOrigin, UnrecordedGoalChanges,
};
use base::context::{
    DurableState, PlanState, RunningStatus, RunningTask, Session, SessionEnv, SessionLedger,
    TaskEvent, TaskInfo, TaskType, TodoItem, TodoStatus,
};
use base::interface::exec::{ConfinementReport, ExecProviders};
use base::permission::PermissionMode;
use base::tool::{Tool, ToolRegistry};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use tokio::sync::broadcast;

use self::notices::Notices;
use self::read_cache::ReadCache;
use self::tasks::TaskTable;

/// 单次会话的可变状态。
///
/// 字段全部私有，走内部可变性。由设置推出来的部分在 [`SessionEnv`] 里，引擎
/// [`attach`](Self::attach) 时整体替换。
#[derive(Debug)]
pub struct SessionState {
    env: std::sync::RwLock<Arc<SessionEnv>>,
    engine: std::sync::RwLock<Option<EngineView>>,
    /// 权限模式（运行时可变；EnterPlanMode / ExitPlanMode 工具会切）。
    /// 私有 + Mutex 内部可变 —— 调用方走 `permission_mode()` / `set_permission_mode()`
    /// / `enter_plan_mode()` / `exit_plan_mode()`。
    permission_mode: std::sync::Mutex<ModeSlot>,
    /// 模型自己维护的 todo 列表（TodoWrite 工具更新；/tasks 命令读取）。
    todos: std::sync::Mutex<Vec<TodoItem>>,
    /// The [`DurableState`] the session log last had; `None` once a write
    /// of it failed, so the next one writes it whatever it is.
    recorded_durable: std::sync::Mutex<Option<DurableState>>,
    /// 本会话用 `ToolSearch` 取过 schema 的 deferred 工具。schema 留在转录里，
    /// 工具表不变（保住 prompt cache）；压缩后的恢复提醒按这份名单让模型重新取。
    activated_tools: std::sync::Mutex<HashSet<String>>,
    /// 后台跑的 sub-agent 任务及其事件。
    tasks: TaskTable,
    /// This session's goal — see [`base::context::goal`].
    goal: std::sync::Mutex<GoalSlot>,
    /// Goal snapshots changed but not yet written to the session log; `None`
    /// records a cleared goal.
    unrecorded_goal_changes: std::sync::Mutex<Vec<Option<Goal>>>,
    goal_recording: Arc<tokio::sync::Mutex<()>>,
    goal_events: broadcast::Sender<Option<GoalView>>,
    /// What the session's own agent should read at its next step boundary,
    /// each under the key it can be retracted by.
    notices: Notices,
    /// 已读取文件缓存，供 staleness 与连续重复读取检测。
    read_cache: ReadCache,
    ledger: std::sync::Mutex<SessionLedger>,
    /// The model a host chose for this session since it was built, read by
    /// whichever engine runs the next turn.
    model_choice: std::sync::Mutex<Option<String>>,
    reasoning_choice: std::sync::Mutex<ReasoningChoice>,
}

/// The reasoning effort and thinking mode a host chose for this session; a
/// field left `None` follows the settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReasoningChoice {
    pub effort: Option<base::interface::model::Effort>,
    pub thinking_mode: Option<base::interface::model::ThinkingMode>,
}

/// What one engine hangs on the session's state while it runs the session:
/// its [`SessionEnv`], the registry its tool calls dispatch against, and the
/// providers they run on — so that a permission judging a call on this
/// session's behalf (a sub-agent's, judged by its parent) judges the tool that
/// will run, on the machine it will run on.
#[derive(Debug, Clone)]
pub struct EngineAttachment {
    env: Arc<SessionEnv>,
    tools: Option<Weak<dyn ToolRegistry>>,
    exec: ExecProviders,
    generation: u64,
}

impl EngineAttachment {
    pub fn new(env: SessionEnv, exec: ExecProviders, generation: u64) -> Self {
        Self {
            env: Arc::new(env),
            tools: None,
            exec,
            generation,
        }
    }

    /// Held weakly: the registry's tools may hold this session.
    pub fn with_tools(mut self, tools: &Arc<dyn ToolRegistry>) -> Self {
        self.tools = Some(Arc::downgrade(tools));
        self
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Debug)]
struct EngineView {
    tools: Option<Weak<dyn ToolRegistry>>,
    exec: ExecProviders,
    generation: u64,
    confinement: Arc<std::sync::OnceLock<ConfinementReport>>,
}

#[derive(Debug, Default)]
struct ModeSlot {
    current: PermissionMode,
    before_plan: Option<PermissionMode>,
}

impl ModeSlot {
    /// Take up `base`, the mode an engine is configured with, unless in plan
    /// mode: that stays on, exiting to where it would have.
    fn rebase(&mut self, base: PermissionMode) {
        if self.current != PermissionMode::Plan {
            *self = ModeSlot {
                current: base,
                before_plan: None,
            };
        }
    }
}

impl SessionState {
    /// A state no engine is attached to yet: its [`env`](Session::env) is empty
    /// until one is.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            env: Default::default(),
            engine: std::sync::RwLock::new(None),
            permission_mode: std::sync::Mutex::new(ModeSlot::default()),
            todos: std::sync::Mutex::new(Vec::new()),
            recorded_durable: std::sync::Mutex::new(Some(DurableState::default())),
            activated_tools: std::sync::Mutex::new(HashSet::new()),
            tasks: TaskTable::new(),
            notices: Notices::default(),
            goal: std::sync::Mutex::new(GoalSlot::default()),
            unrecorded_goal_changes: std::sync::Mutex::new(Vec::new()),
            goal_recording: Default::default(),
            goal_events: tokio::sync::broadcast::channel(64).0,
            read_cache: ReadCache::default(),
            ledger: Default::default(),
            model_choice: std::sync::Mutex::new(None),
            reasoning_choice: Default::default(),
        }
    }

    /// A state working in `cwd`, as a test needs one: no engine attached.
    pub fn for_tests(cwd: PathBuf) -> Self {
        Self::for_tests_in(SessionEnv::new(cwd))
    }

    /// A state with `env`, as a test needs one: no engine attached.
    pub fn for_tests_in(env: SessionEnv) -> Self {
        let state = Self::new();
        state.mode_slot().rebase(env.base_mode);
        *write(&state.env) = Arc::new(env);
        state
    }

    /// Builder：设初始权限模式（CLI 启动时从 settings/CLI 注入）。
    pub fn with_permission_mode(self, mode: PermissionMode) -> Self {
        self.set_permission_mode(mode);
        self
    }

    /// Hang `engine` on this state: its [`SessionEnv`] replaces the one
    /// before, and the permission mode becomes the one it is configured with
    /// unless the session is in plan mode.
    pub fn attach(&self, engine: EngineAttachment) {
        let EngineAttachment {
            env,
            tools,
            exec,
            generation,
        } = engine;
        self.mode_slot().rebase(env.base_mode);
        *write(&self.env) = env;
        *write(&self.engine) = Some(EngineView {
            tools,
            exec,
            generation,
            confinement: Default::default(),
        });
    }

    /// Take off the engine attached as `generation`; one attached since stays.
    /// The env stays too: what that engine left running still works where it
    /// did.
    pub fn detach(&self, generation: u64) {
        let mut engine = write(&self.engine);
        if engine.as_ref().is_some_and(|e| e.generation == generation) {
            *engine = None;
        }
    }

    /// What the attached engine's sandbox holds a command to: `probe`d the
    /// first time it is asked, the same answer after until another engine
    /// attaches. With none attached nothing is kept.
    pub fn confinement(&self, probe: impl FnOnce() -> ConfinementReport) -> ConfinementReport {
        let kept = read(&self.engine).as_ref().map(|e| e.confinement.clone());
        match kept {
            Some(kept) => kept.get_or_init(probe).clone(),
            None => probe(),
        }
    }

    /// Its tasks' events as they happen.
    pub fn subscribe_tasks(&self) -> broadcast::Receiver<TaskEvent> {
        self.tasks.subscribe_tasks()
    }

    /// 列出所有 running tasks 的 (id, status) 快照。
    pub fn list_running_tasks(&self) -> Vec<(String, RunningStatus)> {
        self.tasks.list_running_tasks()
    }

    /// The background work still running, as
    /// [`running_background_task_ids`](Session::running_background_task_ids)
    /// picks it, sorted by id.
    pub fn running_background_tasks(&self) -> Vec<Arc<RunningTask>> {
        self.tasks.running_background_tasks()
    }

    /// Forget that `paths` were read — every file when `None` — so the next
    /// `Edit` or `Write` of one asks for a `Read` first: what the model saw of
    /// it is no longer what is there, or no longer in its context.
    pub fn forget_reads(&self, paths: Option<&[PathBuf]>) {
        self.read_cache.forget(paths)
    }

    /// Put back the durable state a rewind returned the log to: the todos
    /// and plan mode as they were then, or none of them when the log had
    /// recorded nothing yet. Outside plan mode the mode stays as it is.
    pub fn restore_rewound_state(&self, durable: Option<DurableState>) {
        self.restore_durable_state(durable.unwrap_or_default());
    }

    pub fn set_model_choice(&self, model: Option<String>) {
        *lock(&self.model_choice) = model;
    }

    /// Read at every model call of the session's own engine, never written
    /// into its settings, so a choice taken back falls through to them.
    pub fn reasoning_choice(&self) -> ReasoningChoice {
        lock(&self.reasoning_choice).clone()
    }

    pub fn set_reasoning_choice(&self, choice: ReasoningChoice) {
        *lock(&self.reasoning_choice) = choice;
    }

    /// Put back what the session log recorded: the goal, disarmed, and the
    /// durable state. For a state no turn has run on yet.
    pub fn restore_from_log(&self, goal: Option<Goal>, durable: Option<DurableState>) {
        self.restore_goal(goal);
        if let Some(durable) = durable {
            self.restore_durable_state(durable);
        }
    }

    pub fn goal_clear(&self, origin: TurnOrigin) -> Result<(), GoalError> {
        let mut slot = lock(&self.goal);
        slot.clear(origin)?;
        self.goal_changed(&slot, true);
        Ok(())
    }

    /// Stop automatic rounds, keeping the goal as it is.
    pub fn goal_disarm(&self) {
        let mut slot = lock(&self.goal);
        if slot.disarm() {
            self.goal_changed(&slot, false);
        }
    }

    /// The next automatic round — see [`GoalSlot::next_round`].
    pub fn goal_next_round(&self) -> Option<GoalRound> {
        let mut slot = lock(&self.goal);
        let (round, changed) = slot.next_round();
        if changed {
            self.goal_changed(&slot, true);
        }
        round
    }

    /// Start the outstanding round `turn_id` — see [`GoalSlot::claim_round`].
    pub fn goal_claim_round(&self, turn_id: &str) -> bool {
        let mut slot = lock(&self.goal);
        let claimed = slot.claim_round(turn_id);
        if claimed {
            self.goal_changed(&slot, true);
        }
        claimed
    }

    /// Give up the outstanding round `turn_id` without running it.
    pub fn goal_abandon_round(&self, turn_id: &str) {
        lock(&self.goal).abandon_round(turn_id);
    }

    /// Put back the goal a session log recorded, disarmed.
    fn restore_goal(&self, goal: Option<Goal>) {
        *lock(&self.goal) = GoalSlot::restored(goal);
    }

    /// Goal changes the session log does not have yet, oldest first, for the
    /// caller to write while it holds what this returns. Until then every
    /// other caller waits, so the log gets the changes in the order they were
    /// made, and a caller handed none knows none are still being written.
    pub async fn unrecorded_goal_changes(&self) -> UnrecordedGoalChanges {
        let writing = self.goal_recording.clone().lock_owned().await;
        UnrecordedGoalChanges::new(
            std::mem::take(&mut *lock(&self.unrecorded_goal_changes)),
            writing,
        )
    }

    /// The changes of `unrecorded` from `from` on did not reach the log: they
    /// go back ahead of any made since, for the next writer to write first.
    pub fn goal_changes_not_recorded(&self, mut unrecorded: UnrecordedGoalChanges, from: usize) {
        let mut queued = lock(&self.unrecorded_goal_changes);
        let later = std::mem::take(&mut *queued);
        queued.extend(
            unrecorded
                .changes
                .drain(from.min(unrecorded.changes.len())..),
        );
        queued.extend(later);
    }

    /// The goal's view each time it changes.
    pub fn subscribe_goal(&self) -> broadcast::Receiver<Option<GoalView>> {
        self.goal_events.subscribe()
    }

    fn goal_changed(&self, slot: &GoalSlot, record: bool) -> Option<GoalView> {
        if record {
            lock(&self.unrecorded_goal_changes).push(slot.goal.clone());
        }
        let view = slot.view();
        let _ = self.goal_events.send(view.clone());
        view
    }

    fn mode_slot(&self) -> std::sync::MutexGuard<'_, ModeSlot> {
        self.permission_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub fn durable_state(&self) -> DurableState {
        let plan = {
            let slot = self.mode_slot();
            (slot.current == PermissionMode::Plan).then_some(PlanState {
                exit_to: slot.before_plan,
            })
        };
        let mut activated_tools: Vec<String> = self.activated_tools().into_iter().collect();
        activated_tools.sort();
        DurableState {
            plan,
            todos: self.todos(),
            activated_tools,
        }
    }

    /// The durable state, when it differs from what the log last had. The
    /// caller writes it; from here on it counts as recorded.
    pub fn unrecorded_durable_state(&self) -> Option<DurableState> {
        let now = self.durable_state();
        let mut recorded = lock(&self.recorded_durable);
        if recorded.as_ref() == Some(&now) {
            return None;
        }
        *recorded = Some(now.clone());
        Some(now)
    }

    /// The durable state [`Self::unrecorded_durable_state`] handed out did
    /// not reach the log: the next call hands it out again.
    pub fn durable_state_not_recorded(&self) {
        *lock(&self.recorded_durable) = None;
    }

    /// Put back what the session log recorded. Outside plan mode the session
    /// keeps the mode it was built with.
    fn restore_durable_state(&self, state: DurableState) {
        if let Some(plan) = state.plan {
            *self.mode_slot() = ModeSlot {
                current: PermissionMode::Plan,
                before_plan: plan.exit_to,
            };
        }
        lock(&self.todos).clone_from(&state.todos);
        self.activate_tools(state.activated_tools.clone());
        *lock(&self.recorded_durable) = Some(state);
    }
}

impl Session for SessionState {
    fn env(&self) -> Arc<SessionEnv> {
        read(&self.env).clone()
    }

    fn exec(&self) -> Option<ExecProviders> {
        read(&self.engine).as_ref().map(|e| e.exec.clone())
    }

    fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        let tools = read(&self.engine).as_ref()?.tools.as_ref()?.upgrade()?;
        tools.get(name)
    }

    fn permission_mode(&self) -> PermissionMode {
        self.mode_slot().current
    }

    fn model_choice(&self) -> Option<String> {
        lock(&self.model_choice).clone()
    }

    fn set_permission_mode(&self, mode: PermissionMode) {
        *self.mode_slot() = ModeSlot {
            current: mode,
            before_plan: None,
        };
    }

    fn enter_plan_mode(&self) {
        let mut slot = self.mode_slot();
        if slot.current != PermissionMode::Plan {
            slot.before_plan = Some(slot.current);
            slot.current = PermissionMode::Plan;
        }
    }

    fn exit_plan_mode(&self) -> PermissionMode {
        let mut slot = self.mode_slot();
        slot.current = slot.before_plan.take().unwrap_or_default();
        slot.current
    }

    fn register_task(
        &self,
        task_id: String,
        task_type: TaskType,
        cancel: tokio_util::sync::CancellationToken,
        detail: serde_json::Value,
    ) -> Arc<RunningTask> {
        self.tasks.register_task(task_id, task_type, cancel, detail)
    }

    fn finish_task(&self, task_id: &str, status: RunningStatus) {
        self.tasks.finish_task(task_id, status)
    }

    fn find_running_task(&self, task_id: &str) -> Option<Arc<RunningTask>> {
        self.tasks.find_running_task(task_id)
    }

    fn cancel_running_task(&self, task_id: &str) -> bool {
        self.tasks.cancel_running_task(task_id)
    }

    fn running_background_task_ids(&self) -> Vec<String> {
        self.tasks.running_background_task_ids()
    }

    fn task_infos(&self) -> Vec<TaskInfo> {
        self.tasks.task_infos()
    }

    fn post_notice(&self, key: Option<String>, text: String) {
        self.notices.post(key, text)
    }

    fn retract_notice(&self, key: &str) -> bool {
        self.notices.retract(key)
    }

    fn take_notices(&self) -> Vec<String> {
        self.notices.take()
    }

    fn record_read_with_range(&self, path: &Path, offset: Option<usize>, limit: Option<usize>) {
        self.read_cache.record_read(path, offset, limit)
    }

    fn record_write(&self, path: &Path) {
        self.read_cache.record_write(path)
    }

    fn check_read_staleness(&self, path: &Path) -> Option<&'static str> {
        self.read_cache.check_staleness(path)
    }

    fn check_read_dedup_with_range(
        &self,
        path: &Path,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> bool {
        self.read_cache.check_dedup(path, offset, limit)
    }

    fn is_consecutive_read(&self, path: &Path) -> bool {
        self.read_cache.is_consecutive(path)
    }

    fn todos(&self) -> Vec<TodoItem> {
        lock(&self.todos).clone()
    }

    fn set_todos(&self, items: Vec<TodoItem>) {
        let all_completed = items.iter().all(|t| t.status == TodoStatus::Completed);
        if all_completed && !items.is_empty() {
            *lock(&self.todos) = Vec::new();
        } else {
            *lock(&self.todos) = items;
        }
    }

    fn activated_tools(&self) -> HashSet<String> {
        lock(&self.activated_tools).clone()
    }

    fn activate_tools(&self, names: Vec<String>) {
        lock(&self.activated_tools).extend(names);
    }

    fn ledger(&self) -> std::sync::MutexGuard<'_, SessionLedger> {
        lock(&self.ledger)
    }

    fn goal_view(&self) -> Option<GoalView> {
        lock(&self.goal).view()
    }

    fn goal_create(
        &self,
        objective: String,
        max_rounds: Option<u32>,
        origin: TurnOrigin,
    ) -> Result<GoalView, GoalError> {
        let mut slot = lock(&self.goal);
        slot.create(objective, max_rounds, origin)?;
        self.goal_changed(&slot, true).ok_or(GoalError::NoGoal)
    }

    fn goal_update(
        &self,
        goal_id: &str,
        revision: u64,
        action: GoalAction,
        origin: TurnOrigin,
    ) -> Result<GoalView, GoalError> {
        let mut slot = lock(&self.goal);
        slot.update(goal_id, revision, action, origin)?;
        self.goal_changed(&slot, true).ok_or(GoalError::NoGoal)
    }
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn read<T>(l: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(l: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod wire_golden;
