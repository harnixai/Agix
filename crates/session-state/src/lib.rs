//! 一个会话的可变状态：权限模式、todo、任务表、通知、读缓存、目标、记账；
//! `base::context::Session` 的唯一实现。runtime 与会话池持具体类型，工具与策略经
//! `dyn Session` 看到它。

mod notices;
mod read_cache;
mod tasks;
#[cfg(feature = "testing")]
pub mod testing;

use base::context::goal::{
    Goal, GoalAction, GoalError, GoalLimits, GoalRound, GoalSlot, GoalView, TurnOrigin,
    UnrecordedGoalChanges,
};
use base::context::{
    DurableState, GoalSpendRecord, PlanState, RunningStatus, RunningTask, Session, SessionEnv,
    SessionLedger, SpendRecord, TaskEvent, TaskInfo, TaskType, TodoItem, TodoStatus,
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
    mode_events: broadcast::Sender<ModeChange>,
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
}

/// A session's permission mode changed, to `mode`, by `source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ModeChange {
    pub mode: PermissionMode,
    pub source: ModeChangeSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ModeChangeSource {
    /// `session.setPermissionMode`.
    Host,
    /// `EnterPlanMode` / `ExitPlanMode`.
    Model,
}

/// The widest mode a host could put a session in under an engine's settings:
/// the configured one, or any where clients may override it; a
/// `bypassPermissions` the managed layer turns off is `default`. A mode the
/// session did not get from that engine — the one plan mode returns to, as a
/// log or an earlier engine left it — is held to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeCeiling {
    widest: PermissionMode,
    bypass_disabled: bool,
}

impl ModeCeiling {
    pub fn new(widest: PermissionMode, bypass_disabled: bool) -> Self {
        Self {
            widest,
            bypass_disabled,
        }
    }

    /// `mode` held to the ceiling; one only the engine sets is `base`, the
    /// mode the engine is configured with, unless it is that mode.
    fn hold(self, mode: PermissionMode, base: PermissionMode) -> PermissionMode {
        if mode == base {
            return mode;
        }
        if !mode.is_user_settable() {
            return base;
        }
        match mode.min_permissive(self.widest) {
            PermissionMode::BypassPermissions if self.bypass_disabled => PermissionMode::Default,
            held => held,
        }
    }
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
    ceiling: Option<ModeCeiling>,
}

impl EngineAttachment {
    pub fn new(env: SessionEnv, exec: ExecProviders, generation: u64) -> Self {
        Self {
            env: Arc::new(env),
            tools: None,
            exec,
            generation,
            ceiling: None,
        }
    }

    pub fn with_mode_ceiling(mut self, ceiling: ModeCeiling) -> Self {
        self.ceiling = Some(ceiling);
        self
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
    /// See [`Session::host_permission_mode`].
    host_choice: Option<PermissionMode>,
    /// How many times `current` has changed: a permission asked for under one
    /// mode is not answered under another.
    changes: u64,
    /// The attached engine's mode and [`ModeCeiling`].
    bounds: Option<(PermissionMode, ModeCeiling)>,
}

impl ModeSlot {
    fn set(&mut self, current: PermissionMode, before_plan: Option<PermissionMode>) -> bool {
        let changed = self.current != current;
        self.current = current;
        self.before_plan = before_plan;
        if changed {
            self.changes += 1;
        }
        changed
    }

    /// Take up `base`, the mode an engine is configured with, unless in plan
    /// mode: that stays on, exiting to where it would have as far as
    /// `ceiling` allows. The engine's settings already hold what a host
    /// chose.
    fn rebase(&mut self, base: PermissionMode, ceiling: Option<ModeCeiling>) {
        self.host_choice = None;
        self.bounds = ceiling.map(|ceiling| (base, ceiling));
        if self.current != PermissionMode::Plan {
            self.set(base, None);
        } else {
            self.before_plan = self.before_plan.map(|mode| self.held(mode));
        }
    }

    fn held(&self, mode: PermissionMode) -> PermissionMode {
        match self.bounds {
            Some((base, ceiling)) => ceiling.hold(mode, base),
            None => mode,
        }
    }

    fn enter_plan(&mut self) -> bool {
        self.current != PermissionMode::Plan && self.set(PermissionMode::Plan, Some(self.current))
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
            mode_events: broadcast::channel(16).0,
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
        }
    }

    /// A state working in `cwd`, as a test needs one: no engine attached.
    pub fn for_tests(cwd: PathBuf) -> Self {
        Self::for_tests_in(SessionEnv::new(cwd))
    }

    /// A state with `env`, as a test needs one: no engine attached.
    pub fn for_tests_in(env: SessionEnv) -> Self {
        let state = Self::new();
        state.mode_slot().rebase(env.base_mode, None);
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
    /// unless the session is in plan mode, which returns to no wider than the
    /// engine's [`ModeCeiling`].
    pub fn attach(&self, engine: EngineAttachment) {
        let EngineAttachment {
            env,
            tools,
            exec,
            generation,
            ceiling,
        } = engine;
        self.mode_slot().rebase(env.base_mode, ceiling);
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
    /// Going back into plan is the host's change of the mode.
    pub fn restore_rewound_state(&self, durable: Option<DurableState>) {
        if self.restore_durable_state(durable.unwrap_or_default()) {
            self.mode_changed(PermissionMode::Plan, ModeChangeSource::Host);
        }
    }

    pub fn set_model_choice(&self, model: Option<String>) {
        *lock(&self.model_choice) = model;
    }

    /// A host's switch to `mode`. Into plan it is entering plan from the
    /// current mode, which `ExitPlanMode` returns to; to any other mode it
    /// leaves plan, forgetting where that would have returned. `true` when
    /// the mode changed.
    pub fn switch_permission_mode(&self, mode: PermissionMode) -> bool {
        let mut slot = self.mode_slot();
        let changed = if mode == PermissionMode::Plan {
            slot.enter_plan()
        } else {
            slot.host_choice = Some(mode);
            slot.set(mode, None)
        };
        if changed {
            self.mode_changed(mode, ModeChangeSource::Host);
        }
        changed
    }

    /// Counts every change of the mode; equal before and after a wait, the
    /// mode did not change in between.
    pub fn permission_mode_changes(&self) -> u64 {
        self.mode_slot().changes
    }

    /// Each change a host or the model makes to the mode, as it is made.
    pub fn subscribe_permission_mode(&self) -> broadcast::Receiver<ModeChange> {
        self.mode_events.subscribe()
    }

    fn mode_changed(&self, mode: PermissionMode, source: ModeChangeSource) {
        let _ = self.mode_events.send(ModeChange { mode, source });
    }

    /// Put back what the session log recorded: the goal, disarmed, the
    /// durable state, and what the session has spent. For a state no turn has
    /// run on yet.
    pub fn restore_from_log(
        &self,
        goal: Option<Goal>,
        durable: Option<DurableState>,
        spent: Option<SpendRecord>,
    ) {
        let goal_spent = spent.as_ref().and_then(|record| record.goal.clone());
        if let Some(spent) = spent {
            let mut ledger = lock(&self.ledger);
            ledger.spent.restore(spent.session);
            ledger.spent_recorded = Some(spent);
        }
        self.restore_goal(goal, goal_spent);
        if let Some(durable) = durable {
            self.restore_durable_state(durable);
        }
    }

    pub fn goal_clear(&self, origin: TurnOrigin) -> Result<(), GoalError> {
        let mut slot = lock(&self.goal);
        slot.clear(origin)?;
        lock(&self.ledger).goal_spent = None;
        self.goal_changed(&slot, true);
        Ok(())
    }

    /// Stop automatic rounds, keeping the goal as it is — unless its rounds
    /// have spent its token budget, which blocks it.
    pub fn goal_disarm(&self) {
        let spent = self.goal_spent();
        let mut slot = lock(&self.goal);
        if slot.block_if_spent(spent, 0) {
            self.goal_changed(&slot, true);
        } else if slot.disarm() {
            self.goal_changed(&slot, false);
        }
    }

    /// Block the goal on its token budget when what is left cannot pay for
    /// its round's next call, `input` of which is estimated.
    pub fn goal_block_unaffordable(&self, input: u64) {
        let spent = self.goal_spent();
        let mut slot = lock(&self.goal);
        if slot.block_if_spent(spent, input) {
            self.goal_changed(&slot, true);
        }
    }

    /// What the goal's rounds have spent, as its budget counts it.
    fn goal_spent(&self) -> u64 {
        lock(&self.ledger)
            .goal_spent
            .as_ref()
            .map_or(0, |goal| goal.node.read().counted_tokens())
    }

    /// The next automatic round — see [`GoalSlot::next_round`].
    pub fn goal_next_round(&self) -> Option<GoalRound> {
        let spent = self.goal_spent();
        let mut slot = lock(&self.goal);
        let (round, changed) = slot.next_round(spent);
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

    /// Put back the goal a session log recorded, disarmed, and what its
    /// rounds had spent.
    fn restore_goal(&self, goal: Option<Goal>, spent: Option<GoalSpendRecord>) {
        if let Some(goal) = &goal {
            let spent = spent
                .filter(|record| record.id == goal.id)
                .map(|record| record.spent)
                .unwrap_or_default();
            lock(&self.ledger).open_goal(&goal.id, goal.token_budget, spent);
        }
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
        let view = slot.view(self.goal_spent());
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
    /// keeps the mode it was built with; plan returns to no wider than the
    /// attached engine allows. `true` when it went into plan.
    fn restore_durable_state(&self, state: DurableState) -> bool {
        let entered_plan = state.plan.is_some_and(|plan| {
            let mut slot = self.mode_slot();
            let exit_to = plan.exit_to.map(|mode| slot.held(mode));
            slot.set(PermissionMode::Plan, exit_to)
        });
        lock(&self.todos).clone_from(&state.todos);
        self.activate_tools(state.activated_tools.clone());
        *lock(&self.recorded_durable) = Some(state);
        entered_plan
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
        self.mode_slot().set(mode, None);
    }

    fn enter_plan_mode(&self) {
        let mut slot = self.mode_slot();
        if slot.enter_plan() {
            self.mode_changed(slot.current, ModeChangeSource::Model);
        }
    }

    fn exit_plan_mode(&self) -> PermissionMode {
        let mut slot = self.mode_slot();
        let to = slot.before_plan.unwrap_or_default();
        if slot.set(to, None) {
            self.mode_changed(to, ModeChangeSource::Model);
        }
        to
    }

    fn host_permission_mode(&self) -> Option<PermissionMode> {
        self.mode_slot().host_choice
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
        let spent = self.goal_spent();
        lock(&self.goal).view(spent)
    }

    fn goal_create(
        &self,
        objective: String,
        limits: GoalLimits,
        origin: TurnOrigin,
    ) -> Result<GoalView, GoalError> {
        let mut slot = lock(&self.goal);
        let goal = slot.create(objective, limits, origin)?;
        lock(&self.ledger).open_goal(&goal.id, goal.token_budget, Default::default());
        self.goal_changed(&slot, true).ok_or(GoalError::NoGoal)
    }

    fn goal_update(
        &self,
        goal_id: &str,
        revision: u64,
        action: GoalAction,
        origin: TurnOrigin,
        max_rounds_limit: u32,
    ) -> Result<GoalView, GoalError> {
        let spent = self.goal_spent();
        let mut slot = lock(&self.goal);
        let goal = slot.update(goal_id, revision, action, origin, spent, max_rounds_limit)?;
        if let Some(own) = &lock(&self.ledger).goal_spent {
            own.node.set_limit(goal.token_budget);
        }
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
