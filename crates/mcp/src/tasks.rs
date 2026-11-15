//! How a tool call waits when its server turns it into a task: the task is
//! listed in the session's task table as `mcp_task`, polled with `tasks/get`
//! until it ends, its input requests are answered through the call's
//! elicitor, and its result is the call's result.
//!
//! The server is asked to cancel only from [`CancelOnDrop`]. An interrupted
//! turn drops the whole call future at its own `select!`, before any branch
//! in here could run; `Drop` is the one thing that always does. Stopping the
//! task and the deadline end the wait the same way, so they cancel through it
//! too.

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use base::context::{RunningStatus, RunningTask, SessionState, TaskLease, TaskType};
use rmcp::model::{CallToolResult, DetailedTask, InputResponses, Task, TaskPayload};
use tokio_util::sync::CancellationToken;

use crate::connect::{Service, StdioMcpClient};
use crate::elicit::{answer_input_requests, McpCallScope};
use crate::error::McpError;

const POLL_DEFAULT: Duration = Duration::from_secs(1);
const POLL_MIN: Duration = Duration::from_millis(100);
const POLL_MAX: Duration = Duration::from_secs(5);
const POLL_RETRIES: u32 = 3;
const FAILURE_MESSAGE_CHARS: usize = 2_000;

/// Where a call's task is listed: the session of the tool call that started it.
#[derive(Clone)]
pub struct McpTaskHost {
    pub session: Arc<SessionState>,
    pub tool: String,
    pub tool_use_id: String,
    pub cancel: CancellationToken,
    /// Set once the task is listed, so the elicitor can mark it waiting.
    pub listed: Arc<OnceLock<Arc<RunningTask>>>,
}

#[async_trait]
pub(crate) trait TaskPeer: Send + Sync {
    async fn get(&self, task_id: &str) -> Result<DetailedTask, McpError>;
    async fn update(&self, task_id: &str, responses: InputResponses) -> Result<(), McpError>;
    /// Ask the server to cancel, without waiting for it.
    fn cancel_detached(&self, task_id: &str);
}

struct CancelOnDrop<'a> {
    peer: &'a dyn TaskPeer,
    task_id: String,
    armed: bool,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.peer.cancel_detached(&self.task_id);
        }
    }
}

struct Ticket {
    lease: Option<TaskLease>,
    stop: CancellationToken,
}

impl Ticket {
    fn open(host: Option<&McpTaskHost>, server: &str, task: &Task) -> Self {
        let Some(host) = host else {
            // No session to list it in, so nothing can stop it: a token that
            // is never cancelled keeps the wait's `select!` uniform.
            return Self {
                lease: None,
                stop: CancellationToken::new(),
            };
        };
        let stop = host.cancel.child_token();
        let mut detail = serde_json::json!({
            "server": server,
            "tool": host.tool,
            "tool_use_id": host.tool_use_id,
            "mcp_task_id": task.task_id,
            "mcp_status": status_name(task),
        });
        for (key, value) in [
            (
                "status_message",
                task.status_message.clone().map(Into::into),
            ),
            ("poll_interval_ms", task.poll_interval_ms.map(Into::into)),
            ("ttl_ms", task.ttl_ms.map(Into::into)),
        ] {
            if let Some(value) = value {
                detail[key] = value;
            }
        }
        let lease = TaskLease::new(
            host.session.clone(),
            format!("mcp-{}", base::id::Id::new()),
            TaskType::McpTask,
            stop.clone(),
            detail,
        );
        let _ = host.listed.set(lease.task().clone());
        let ticket = Self {
            lease: Some(lease),
            stop,
        };
        ticket.line(task);
        ticket
    }

    fn line(&self, task: &Task) {
        let Some(lease) = &self.lease else { return };
        let message = task.status_message.as_deref().unwrap_or_default();
        lease
            .task()
            .append_output(&format!("[{}] {message}\n", status_name(task)));
        lease.task().report_progress();
    }

    fn update(&self, task: &Task) {
        let Some(lease) = &self.lease else { return };
        self.line(task);
        lease.task().update_detail(|detail| {
            detail["mcp_status"] = status_name(task).into();
            match &task.status_message {
                Some(message) => detail["status_message"] = message.clone().into(),
                None => {
                    if let Some(map) = detail.as_object_mut() {
                        map.remove("status_message");
                    }
                }
            }
        });
    }

    fn conclude(&mut self, status: RunningStatus) {
        if let Some(lease) = self.lease.take() {
            lease.conclude(status, None);
        }
    }
}

fn status_name(task: &Task) -> String {
    serde_json::to_value(task.status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

fn poll_every(task: &Task) -> Duration {
    task.poll_interval_ms
        .map_or(POLL_DEFAULT, Duration::from_millis)
        .clamp(POLL_MIN, POLL_MAX)
}

pub(crate) async fn await_task(
    server: &str,
    peer: &dyn TaskPeer,
    task: Task,
    scope: &McpCallScope,
    limit: Duration,
) -> Result<CallToolResult, McpError> {
    let mut ticket = Ticket::open(scope.tasks.as_ref(), server, &task);
    let mut guard = CancelOnDrop {
        peer,
        task_id: task.task_id.clone(),
        armed: true,
    };
    let stop = ticket.stop.clone();
    let outcome = tokio::select! {
        biased;
        () = stop.cancelled() => Err(McpError::TaskStopped {
            name: server.to_string(),
            task_id: task.task_id.clone(),
        }),
        () = tokio::time::sleep(limit) => Err(McpError::Timeout {
            name: server.to_string(),
            op: "tasks/get",
            after: limit,
        }),
        ended = drive(server, peer, &task, scope, &ticket) => ended,
    };
    match &outcome {
        Ok(_) => {
            guard.armed = false;
            ticket.conclude(RunningStatus::Completed);
        }
        Err(McpError::TaskEnded { how, .. }) => {
            guard.armed = false;
            ticket.conclude(if how.starts_with("failed") {
                RunningStatus::Failed(how.trim_start_matches("failed: ").to_string())
            } else {
                RunningStatus::Cancelled
            });
        }
        Err(_) => {}
    }
    outcome
}

async fn drive(
    server: &str,
    peer: &dyn TaskPeer,
    created: &Task,
    scope: &McpCallScope,
    ticket: &Ticket,
) -> Result<CallToolResult, McpError> {
    let id = created.task_id.as_str();
    let ended = |how: String| McpError::TaskEnded {
        name: server.to_string(),
        task_id: id.to_string(),
        how,
    };
    let mut every = poll_every(created);
    let mut seen = (created.status, created.status_message.clone());
    let mut answered = BTreeSet::new();
    let mut timeouts = 0;
    loop {
        tokio::time::sleep(every).await;
        let got = match peer.get(id).await {
            Ok(got) => got,
            Err(McpError::Timeout { .. }) if timeouts < POLL_RETRIES => {
                timeouts += 1;
                continue;
            }
            Err(e) => return Err(e),
        };
        timeouts = 0;
        every = poll_every(&got.task);
        if (got.task.status, got.task.status_message.clone()) != seen {
            seen = (got.task.status, got.task.status_message.clone());
            ticket.update(&got.task);
        }
        match got.payload {
            TaskPayload::Working => {}
            TaskPayload::InputRequired { input_requests } => {
                let fresh: rmcp::model::InputRequests = input_requests
                    .into_iter()
                    .filter(|(key, _)| !answered.contains(key))
                    .collect();
                if fresh.is_empty() {
                    continue;
                }
                answered.extend(fresh.keys().cloned());
                let responses = answer_input_requests(server, fresh, scope).await?;
                peer.update(id, responses).await?;
            }
            TaskPayload::Completed { result } => {
                return Ok(serde_json::from_value(serde_json::Value::Object(result))?);
            }
            TaskPayload::Failed { error } => {
                let message = error.get("message").and_then(|m| m.as_str()).map_or_else(
                    || serde_json::Value::Object(error.clone()).to_string(),
                    str::to_string,
                );
                let message: String = message.chars().take(FAILURE_MESSAGE_CHARS).collect();
                return Err(ended(format!("failed: {message}")));
            }
            TaskPayload::Cancelled => return Err(ended("was cancelled by the server".into())),
            _ => {
                return Err(McpError::RmcpService(format!(
                    "MCP server `{server}` reported a task status this client does not know"
                )))
            }
        }
    }
}

pub(crate) struct Polling<'a> {
    pub(crate) client: &'a StdioMcpClient,
    pub(crate) svc: Arc<Service>,
}

#[async_trait::async_trait]
impl crate::tasks::TaskPeer for Polling<'_> {
    async fn get(&self, task_id: &str) -> Result<rmcp::model::DetailedTask, McpError> {
        let params = rmcp::model::GetTaskParams::new(task_id);
        self.client
            .request(
                "tasks/get",
                self.client.request_limit(),
                &self.svc,
                self.svc.get_task(params),
            )
            .await
            .map(|got| got.task)
    }

    async fn update(
        &self,
        task_id: &str,
        responses: rmcp::model::InputResponses,
    ) -> Result<(), McpError> {
        let params = rmcp::model::UpdateTaskParams::new(task_id, responses);
        self.client
            .request(
                "tasks/update",
                self.client.request_limit(),
                &self.svc,
                self.svc.update_task(params),
            )
            .await
    }

    fn cancel_detached(&self, task_id: &str) {
        let svc = self.svc.clone();
        let params = rmcp::model::CancelTaskParams::new(task_id);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(5), svc.cancel_task(params)).await;
            });
        }
    }
}

/// The call's result: what the server completed with, or, when it turned the
/// call into a task, what the task ends with.
pub(crate) async fn settle(
    client: &StdioMcpClient,
    svc: &Arc<Service>,
    end: crate::elicit::CallEnd,
    scope: &McpCallScope,
    limit: Duration,
) -> Result<CallToolResult, McpError> {
    match end {
        crate::elicit::CallEnd::Complete(result) => Ok(result),
        crate::elicit::CallEnd::Task(task) => {
            let peer = Polling {
                client,
                svc: svc.clone(),
            };
            await_task(&client.server_name, &peer, task, scope, limit).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::TaskStatus;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakePeer {
        script: Mutex<VecDeque<DetailedTask>>,
        updates: Mutex<Vec<InputResponses>>,
        cancels: Mutex<Vec<String>>,
    }

    impl FakePeer {
        fn saying(script: Vec<DetailedTask>) -> Self {
            Self {
                script: Mutex::new(script.into()),
                ..Default::default()
            }
        }
        fn cancels(&self) -> usize {
            self.cancels.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl TaskPeer for FakePeer {
        async fn get(&self, _: &str) -> Result<DetailedTask, McpError> {
            let mut script = self.script.lock().unwrap();
            Ok(if script.len() > 1 {
                script.pop_front().unwrap()
            } else {
                script.front().cloned().expect("a scripted answer")
            })
        }
        async fn update(&self, _: &str, responses: InputResponses) -> Result<(), McpError> {
            self.updates.lock().unwrap().push(responses);
            Ok(())
        }
        fn cancel_detached(&self, task_id: &str) {
            self.cancels.lock().unwrap().push(task_id.to_string());
        }
    }

    fn created() -> Task {
        let mut task = Task::new("t1", TaskStatus::Working, "2026-01-01", "2026-01-01");
        task.poll_interval_ms = Some(100);
        task
    }

    fn at(message: &str, payload: TaskPayload) -> DetailedTask {
        let mut task = created();
        task.status_message = Some(message.into());
        DetailedTask::new(task, payload)
    }

    fn done(text: &str) -> DetailedTask {
        let result = CallToolResult::success(vec![rmcp::model::ContentBlock::text(text)]);
        let serde_json::Value::Object(result) = serde_json::to_value(result).unwrap() else {
            unreachable!()
        };
        at("finished", TaskPayload::Completed { result })
    }

    fn hosted() -> (Arc<SessionState>, McpCallScope, CancellationToken) {
        let session = Arc::new(SessionState::for_tests("/tmp".into()));
        let cancel = CancellationToken::new();
        let scope = McpCallScope {
            elicitor: None,
            tasks: Some(McpTaskHost {
                session: session.clone(),
                tool: "mcp__demo__work".into(),
                tool_use_id: "tu1".into(),
                cancel: cancel.clone(),
                listed: Default::default(),
            }),
        };
        (session, scope, cancel)
    }

    const LONG: Duration = Duration::from_secs(60);

    #[tokio::test(start_paused = true)]
    async fn waits_through_working_to_the_result() {
        let (session, scope, _) = hosted();
        let peer = FakePeer::saying(vec![
            at("step 1/2", TaskPayload::Working),
            at("step 2/2", TaskPayload::Working),
            done("all done"),
        ]);
        let result = await_task("demo", &peer, created(), &scope, LONG)
            .await
            .unwrap();
        assert_eq!(result.content.len(), 1);
        let tasks = session.task_infos();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].kind, TaskType::McpTask);
        assert_eq!(tasks[0].status.as_str(), "done");
        assert_eq!(peer.cancels(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn each_status_change_is_one_output_line_and_a_detail_update() {
        let (session, scope, _) = hosted();
        let peer = FakePeer::saying(vec![
            at("step 1/2", TaskPayload::Working),
            at("step 1/2", TaskPayload::Working),
            at("step 2/2", TaskPayload::Working),
            done("x"),
        ]);
        await_task("demo", &peer, created(), &scope, LONG)
            .await
            .unwrap();
        let task = session
            .find_running_task(&session.task_infos()[0].task_id)
            .unwrap();
        let lines = task.output_text();
        assert_eq!(
            lines,
            "[working] \n[working] step 1/2\n[working] step 2/2\n[completed] finished\n"
        );
        assert_eq!(task.info().detail["mcp_status"], "completed");
        assert_eq!(task.info().detail["mcp_task_id"], "t1");
    }

    fn asking(keys: &[&str]) -> DetailedTask {
        let request: rmcp::model::InputRequest = serde_json::from_value(serde_json::json!({
            "method": "elicitation/create",
            "params": {"mode": "form", "message": "q", "requestedSchema": {"type": "object", "properties": {}}}
        }))
        .unwrap();
        at(
            "asking",
            TaskPayload::InputRequired {
                input_requests: keys
                    .iter()
                    .map(|k| (k.to_string(), request.clone()))
                    .collect(),
            },
        )
    }

    #[tokio::test(start_paused = true)]
    async fn an_input_request_is_answered_once_per_key() {
        let (_, scope, _) = hosted();
        let peer = FakePeer::saying(vec![
            asking(&["a"]),
            asking(&["a"]),
            asking(&["a", "b"]),
            done("x"),
        ]);
        await_task("demo", &peer, created(), &scope, LONG)
            .await
            .unwrap();
        let updates = peer.updates.lock().unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].keys().collect::<Vec<_>>(), ["a"]);
        assert_eq!(updates[1].keys().collect::<Vec<_>>(), ["b"]);
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_ends_the_task_stopped_and_cancels_on_the_server() {
        let (session, scope, _) = hosted();
        let peer = FakePeer::saying(vec![at("busy", TaskPayload::Working)]);
        let waiting = await_task("demo", &peer, created(), &scope, LONG);
        let stopper = async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let id = session.task_infos()[0].task_id.clone();
            assert!(session.cancel_running_task(&id));
        };
        let (result, ()) = tokio::join!(waiting, stopper);
        assert!(
            matches!(result, Err(McpError::TaskStopped { .. })),
            "{result:?}"
        );
        assert_eq!(peer.cancels(), 1);
        assert_eq!(session.task_infos()[0].status.as_str(), "stopped");
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_wait_cancels_on_the_server() {
        let (session, scope, _) = hosted();
        let peer = FakePeer::saying(vec![at("busy", TaskPayload::Working)]);
        let timed = tokio::time::timeout(
            Duration::from_millis(250),
            await_task("demo", &peer, created(), &scope, LONG),
        )
        .await;
        assert!(timed.is_err());
        assert_eq!(peer.cancels(), 1);
        assert_eq!(session.task_infos()[0].status.as_str(), "stopped");
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_task_is_not_cancelled() {
        let (_, scope, _) = hosted();
        let peer = FakePeer::saying(vec![done("x")]);
        await_task("demo", &peer, created(), &scope, LONG)
            .await
            .unwrap();
        let failing = FakePeer::saying(vec![at(
            "x",
            TaskPayload::Failed {
                error: serde_json::from_value(serde_json::json!({"message": "boom"})).unwrap(),
            },
        )]);
        let result = await_task("demo", &failing, created(), &scope, LONG).await;
        assert!(
            matches!(&result, Err(McpError::TaskEnded { how, .. }) if how == "failed: boom"),
            "{result:?}"
        );
        assert_eq!(peer.cancels() + failing.cancels(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_cancels_and_reports_a_timeout() {
        let (_, scope, _) = hosted();
        let peer = FakePeer::saying(vec![at("busy", TaskPayload::Working)]);
        let result = await_task("demo", &peer, created(), &scope, Duration::from_secs(2)).await;
        assert!(
            matches!(result, Err(McpError::Timeout { .. })),
            "{result:?}"
        );
        assert_eq!(peer.cancels(), 1);
    }

    #[test]
    fn the_poll_interval_is_kept_between_100ms_and_5s() {
        let every = |ms: Option<u64>| {
            let mut task = created();
            task.poll_interval_ms = ms;
            poll_every(&task)
        };
        assert_eq!(every(None), Duration::from_secs(1));
        assert_eq!(every(Some(1)), Duration::from_millis(100));
        assert_eq!(every(Some(60_000)), Duration::from_secs(5));
        assert_eq!(every(Some(300)), Duration::from_millis(300));
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_session_the_task_is_awaited_but_not_listed() {
        let peer = FakePeer::saying(vec![at("busy", TaskPayload::Working), done("x")]);
        let result = await_task("demo", &peer, created(), &McpCallScope::default(), LONG).await;
        assert!(result.is_ok());
    }
}
