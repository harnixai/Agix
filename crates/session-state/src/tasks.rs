//! 会话的任务表：后台任务的登记、结束、取消与事件，以及绑定任务生命周期的 `TaskScope` / `TaskLease`。

use base::context::task::{RunningStatus, RunningTask, TaskEvent, TaskInfo, TaskType};
use std::collections::HashMap;
use tokio::sync::broadcast;

#[derive(Debug)]
pub(crate) struct TaskTable {
    running: std::sync::Mutex<HashMap<String, std::sync::Arc<RunningTask>>>,
    events: broadcast::Sender<TaskEvent>,
}

impl TaskTable {
    pub(crate) fn new() -> Self {
        Self {
            running: std::sync::Mutex::new(HashMap::new()),
            events: tokio::sync::broadcast::channel(256).0,
        }
    }

    pub(crate) fn register_task(
        &self,
        task_id: String,
        task_type: TaskType,
        cancel: tokio_util::sync::CancellationToken,
        detail: serde_json::Value,
    ) -> std::sync::Arc<RunningTask> {
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

    pub(crate) fn finish_task(&self, task_id: &str, status: RunningStatus) {
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
            *ended = Some(base::clock::unix_ms());
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

    pub(crate) fn task_infos(&self) -> Vec<TaskInfo> {
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

    pub(crate) fn subscribe_tasks(&self) -> broadcast::Receiver<TaskEvent> {
        self.events.subscribe()
    }

    pub(crate) fn find_running_task(&self, task_id: &str) -> Option<std::sync::Arc<RunningTask>> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(task_id)
            .cloned()
    }

    pub(crate) fn list_running_tasks(&self) -> Vec<(String, RunningStatus)> {
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

    pub(crate) fn running_background_task_ids(&self) -> Vec<String> {
        self.running_background_tasks()
            .iter()
            .map(|task| task.task_id.clone())
            .collect()
    }

    pub(crate) fn running_background_tasks(&self) -> Vec<std::sync::Arc<RunningTask>> {
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

    pub(crate) fn cancel_running_task(&self, task_id: &str) -> bool {
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
