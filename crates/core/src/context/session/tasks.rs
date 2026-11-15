//! 会话的任务表：后台任务的登记、结束、取消与事件，以及绑定任务生命周期的 `TaskScope` / `TaskLease`。

use super::SessionState;
use crate::context::task::{RunningStatus, RunningTask, TaskEvent, TaskInfo, TaskType};
use std::collections::HashMap;
use tokio::sync::broadcast;

#[derive(Debug)]
pub(super) struct TaskTable {
    running: std::sync::Mutex<HashMap<String, std::sync::Arc<RunningTask>>>,
    events: broadcast::Sender<TaskEvent>,
}

impl TaskTable {
    pub(super) fn new() -> Self {
        Self {
            running: std::sync::Mutex::new(HashMap::new()),
            events: tokio::sync::broadcast::channel(256).0,
        }
    }

    pub(super) fn register_task(
        &self,
        task_id: impl Into<String>,
        task_type: TaskType,
        cancel: tokio_util::sync::CancellationToken,
        detail: serde_json::Value,
    ) -> std::sync::Arc<RunningTask> {
        let task_id = task_id.into();
        let task =
            std::sync::Arc::new(RunningTask::with_cancel(task_id.clone(), task_type, cancel));
        *task.detail.lock().unwrap_or_else(|e| e.into_inner()) = detail;
        task.report_to(self.events.clone());
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(task_id, task.clone());
        let quiet = matches!(task_type, TaskType::Shell | TaskType::Monitor);
        if !quiet {
            self.announce(&task);
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (task, events) = (task.clone(), self.events.clone());
            runtime.spawn(async move {
                let a_second = std::time::Duration::from_secs(1);
                tokio::select! {
                    _ = tokio::time::sleep(a_second) => {}
                    _ = task.wait_ended() => return,
                }
                if task.has_ended() {
                    return;
                }
                Self::announce_on(&events, &task);
                loop {
                    task.report_progress();
                    tokio::select! {
                        _ = tokio::time::sleep(a_second) => {}
                        _ = task.wait_ended() => return,
                    }
                }
            });
        }
        task
    }

    fn announce(&self, task: &RunningTask) {
        Self::announce_on(&self.events, task);
    }

    fn announce_on(events: &broadcast::Sender<TaskEvent>, task: &RunningTask) {
        if !task
            .announced
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let _ = events.send(TaskEvent::Started { task: task.info() });
        }
    }

    pub(super) fn finish_task(&self, task_id: &str, status: RunningStatus) {
        let Some(task) = self.find_running_task(task_id) else {
            return;
        };
        Self::mark_ended(&task, status);
        if Self::close_on(&self.events, &task) {
            self.forget_old_tasks();
        }
    }

    /// Set the task's end and `status`, unless it has ended already.
    fn mark_ended(task: &RunningTask, status: RunningStatus) {
        {
            let mut ended = task.ended_ms.lock().unwrap_or_else(|e| e.into_inner());
            if ended.is_some() {
                return;
            }
            *ended = Some(crate::clock::unix_ms());
            let mut current = task.status.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(*current, RunningStatus::Running) {
                *current = status;
            }
        }
        task.ended.notify_waiters();
    }

    /// Close the task's output and announce `task_finished`; `false` if it
    /// was closed already.
    fn close_on(events: &broadcast::Sender<TaskEvent>, task: &RunningTask) -> bool {
        if !task.close_output() {
            return false;
        }
        if task.announced.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = events.send(TaskEvent::Finished { task: task.info() });
        }
        true
    }

    /// How long stopped work has to wind down and reach its own
    /// [`finish_task`](Self::finish_task) before its output is closed for it;
    /// longer than the 10 s a stopped program gets to wrap up.
    const STOPPED_WORK_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

    const FINISHED_TASKS_KEPT: usize = 20;

    fn forget_old_tasks(&self) {
        let mut tasks = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let mut finished: Vec<(i64, String)> = tasks
            .values()
            .filter(|t| t.output_closed())
            .filter_map(|t| {
                let ended = (*t.ended_ms.lock().unwrap_or_else(|e| e.into_inner()))?;
                Some((ended, t.task_id.clone()))
            })
            .collect();
        if finished.len() <= Self::FINISHED_TASKS_KEPT {
            return;
        }
        finished.sort();
        for (_, id) in finished
            .iter()
            .take(finished.len() - Self::FINISHED_TASKS_KEPT)
        {
            tasks.remove(id);
        }
    }

    pub(super) fn task_infos(&self) -> Vec<TaskInfo> {
        let mut tasks: Vec<std::sync::Arc<RunningTask>> = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        tasks.sort_by_key(|t| t.started_ms);
        tasks.iter().map(|t| t.info()).collect()
    }

    pub(super) fn subscribe_tasks(&self) -> broadcast::Receiver<TaskEvent> {
        self.events.subscribe()
    }

    pub(super) fn find_running_task(&self, task_id: &str) -> Option<std::sync::Arc<RunningTask>> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(task_id)
            .cloned()
    }

    pub(super) fn list_running_tasks(&self) -> Vec<(String, RunningStatus)> {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.status.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                )
            })
            .collect()
    }

    pub(super) fn running_background_task_ids(&self) -> Vec<String> {
        self.running_background_tasks()
            .iter()
            .map(|task| task.task_id.clone())
            .collect()
    }

    pub(super) fn running_background_tasks(&self) -> Vec<std::sync::Arc<RunningTask>> {
        let mut tasks: Vec<std::sync::Arc<RunningTask>> = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|task| {
                !matches!(task.task_type, TaskType::Shell | TaskType::McpTask)
                    && matches!(
                        *task.status.lock().unwrap_or_else(|e| e.into_inner()),
                        RunningStatus::Running
                    )
            })
            .cloned()
            .collect();
        tasks.sort_by(|a, b| a.task_id.cmp(&b.task_id));
        tasks
    }

    pub(super) fn cancel_running_task(&self, task_id: &str) -> bool {
        let Some(task) = self.find_running_task(task_id) else {
            return false;
        };
        if task.has_ended() {
            return false;
        }
        task.cancel.cancel();
        Self::mark_ended(&task, RunningStatus::Cancelled);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let events = self.events.clone();
                runtime.spawn(async move {
                    tokio::time::sleep(Self::STOPPED_WORK_GRACE).await;
                    Self::close_on(&events, &task);
                });
            }
            Err(_) => {
                Self::close_on(&self.events, &task);
            }
        }
        true
    }
}

/// A task that lasts as long as this value: registered on creation, ended on
/// drop — stopped if its token was cancelled, done otherwise. For work bounded
/// by one call, like a shell command.
pub struct TaskScope {
    session: std::sync::Arc<SessionState>,
    task: std::sync::Arc<RunningTask>,
}

impl TaskScope {
    pub fn new(
        session: std::sync::Arc<SessionState>,
        task_id: impl Into<String>,
        task_type: TaskType,
        cancel: tokio_util::sync::CancellationToken,
        detail: serde_json::Value,
    ) -> Self {
        let task = session.register_task(task_id, task_type, cancel, detail);
        Self { session, task }
    }

    pub fn task(&self) -> &std::sync::Arc<RunningTask> {
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
    session: std::sync::Arc<SessionState>,
    task: std::sync::Arc<RunningTask>,
    stopped_notice: Option<StoppedNotice>,
    concluded: bool,
}

type StoppedNotice = Box<dyn Fn(&RunningTask) -> String + Send + Sync>;

impl TaskLease {
    pub fn new(
        session: std::sync::Arc<SessionState>,
        task_id: impl Into<String>,
        task_type: TaskType,
        cancel: tokio_util::sync::CancellationToken,
        detail: serde_json::Value,
    ) -> Self {
        let task = session.register_task(task_id, task_type, cancel, detail);
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

    pub fn task(&self) -> &std::sync::Arc<RunningTask> {
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
