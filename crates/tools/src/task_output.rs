//! TaskOutputTool — retrieve output from a running or completed background task.
//!
//! Accepts `task_id` (required), `block` (default: true), and `timeout` (default: 30000ms,
//! max: 600000ms). When `block=true`, polls the running task store until the task completes
//! or the timeout expires.
//!
//! Returns structured content with `retrieval_status`, `task_id`, optional `output` and `status`.
//!
//! Finds the task among the session's and polls it for completion.

use async_trait::async_trait;
use base::context::RunningStatus;
use base::error::ToolError;
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::tool::{
    PathStyle, ProgressSender, Tool, ToolContext, ToolResult, ToolSpec, ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskOutputInput {
    /// The ID of the background task to retrieve output for
    pub task_id: String,
    /// Whether to block until the task completes (default: true)
    #[serde(default = "default_true")]
    pub block: bool,
    /// Maximum time to wait in milliseconds (default: 30000, max: 600000)
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    30_000
}

/// Retrieve output from a running or completed background task.
///
/// When `block=true` (default), the tool polls until the task finishes or the
/// timeout expires. Returns structured content with `retrieval_status`,
/// `task_id`, `output`, and `status`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TaskOutputTool;

#[async_trait]
impl Tool for TaskOutputTool {
    fn description(&self) -> &str {
        "Retrieve output from a running or completed background task"
    }

    fn name(&self) -> &str {
        "TaskOutput"
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<TaskOutputInput>()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().concurrency_safe(true).read_only(true)
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<TaskOutputInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.task_id.trim().is_empty() => {
                ValidationResult::err("Missing required parameter: task_id", 1)
            }
            Ok(p) if p.timeout > 600_000 => {
                ValidationResult::err("timeout must not exceed 600000 ms", 2)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 3),
        }
    }

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::SessionOnly)
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: TaskOutputInput = serde_json::from_value(input)?;
        let result = self.retrieve(&input, &ctx).await;
        if let Ok(r) = &result {
            let ended = r
                .structured_content
                .as_ref()
                .and_then(|c| c.get("status"))
                .and_then(Value::as_str)
                .is_some_and(|s| s != "running");
            if ended {
                ctx.session.retract_notice(&input.task_id);
            }
        }
        result
    }
}

impl TaskOutputTool {
    async fn retrieve(
        &self,
        input: &TaskOutputInput,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let find = || {
            ctx.session.find_running_task(&input.task_id).map(|task| {
                let status = task
                    .status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                (task.output_text(), status)
            })
        };
        let Some((output, status)) = find() else {
            return Ok(not_found(&input.task_id, "task not found"));
        };

        if status == RunningStatus::Running && input.block {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(input.timeout);
            let mut current_output = output;

            while tokio::time::Instant::now() < deadline {
                sleep(Duration::from_millis(500)).await;

                let Some((new_output, new_status)) = find() else {
                    return Ok(not_found(&input.task_id, "task disappeared during wait"));
                };
                current_output = new_output;
                if new_status != RunningStatus::Running {
                    return Ok(snapshot(
                        &input.task_id,
                        "success",
                        current_output,
                        status_to_str(&new_status),
                    ));
                }
            }

            return Ok(snapshot(
                &input.task_id,
                "not_ready",
                current_output,
                "running",
            ));
        }

        Ok(snapshot(
            &input.task_id,
            "success",
            output,
            status_to_str(&status),
        ))
    }
}

fn not_found(task_id: &str, message: &str) -> ToolResult {
    ToolResult::error_text(message.to_string()).with_structured(json!({
        "retrieval_status": "not_found",
        "task_id": task_id,
    }))
}

fn snapshot(task_id: &str, retrieval_status: &str, output: String, status: &str) -> ToolResult {
    ToolResult::text(output.clone()).with_structured(json!({
        "retrieval_status": retrieval_status,
        "task_id": task_id,
        "output": output,
        "status": status,
    }))
}

/// Convert a RunningStatus to a serializable &'static str.
fn status_to_str(status: &RunningStatus) -> &'static str {
    match status {
        RunningStatus::Running => "running",
        RunningStatus::Completed => "completed",
        RunningStatus::Cancelled => "cancelled",
        RunningStatus::Failed(_) => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::context::TaskType;
    use serde_json::json;
    use std::path::Path;

    /// A context whose session runs `tasks` — id, output so far, status.
    fn ctx_with_tasks(tasks: &[(&str, &str, RunningStatus)]) -> ToolContext {
        let ctx = platform::tool_context_for_test(Path::new("/").to_path_buf());
        for (id, output, status) in tasks {
            let task = ctx.session.register_task(
                *id,
                TaskType::Code,
                tokio_util::sync::CancellationToken::new(),
                serde_json::Value::Null,
            );
            task.append_output(output);
            if *status != RunningStatus::Running {
                ctx.session.finish_task(id, status.clone());
            }
        }
        ctx
    }

    #[tokio::test]
    async fn name_is_task_output() {
        let tool = TaskOutputTool;
        assert_eq!(tool.name(), "TaskOutput");
        assert!(tool.call_traits(&Value::Null).read_only);
        assert!(tool.call_traits(&Value::Null).concurrency_safe);
    }

    #[tokio::test]
    async fn requires_task_id() {
        let r = TaskOutputTool
            .validate_input(&json!({}), &ctx_with_tasks(&[]))
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn empty_task_id_rejected() {
        let r = TaskOutputTool
            .validate_input(&json!({"task_id": ""}), &ctx_with_tasks(&[]))
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn timeout_cap_enforced() {
        let r = TaskOutputTool
            .validate_input(
                &json!({"task_id": "t1", "timeout": 999999}),
                &ctx_with_tasks(&[]),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    async fn output_of(ctx: ToolContext, input: Value) -> ToolResult {
        TaskOutputTool
            .call(input, ctx, ProgressSender::noop("t"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn not_found_returns_error() {
        let r = output_of(ctx_with_tasks(&[]), json!({"task_id": "nonexistent"})).await;
        assert!(r.is_error);
        let sc = r.structured_content.unwrap();
        assert_eq!(sc["retrieval_status"], "not_found");
        assert_eq!(sc["task_id"], "nonexistent");
    }

    #[tokio::test]
    async fn completed_task_returns_output() {
        let ctx = ctx_with_tasks(&[("ag-done", "output text", RunningStatus::Completed)]);
        let r = output_of(ctx, json!({"task_id": "ag-done", "block": false})).await;
        assert!(!r.is_error);
        let sc = r.structured_content.unwrap();
        assert_eq!(sc["retrieval_status"], "success");
        assert_eq!(sc["output"], "output text");
        assert_eq!(sc["status"], "completed");
    }

    #[tokio::test]
    async fn running_task_without_block_returns_not_ready() {
        let ctx = ctx_with_tasks(&[("ag-running", "partial output", RunningStatus::Running)]);
        let r = output_of(ctx, json!({"task_id": "ag-running", "block": false})).await;
        assert!(!r.is_error);
        let sc = r.structured_content.unwrap();
        assert_eq!(sc["retrieval_status"], "success");
        assert_eq!(sc["output"], "partial output");
        assert_eq!(sc["status"], "running");
    }

    #[tokio::test]
    async fn failed_task_returns_status() {
        let ctx = ctx_with_tasks(&[("ag-fail", "error log", RunningStatus::Failed("oops".into()))]);
        let r = output_of(ctx, json!({"task_id": "ag-fail", "block": false})).await;
        assert!(!r.is_error);
        let sc = r.structured_content.unwrap();
        assert_eq!(sc["retrieval_status"], "success");
        assert_eq!(sc["output"], "error log");
        assert_eq!(sc["status"], "failed");
    }

    #[tokio::test]
    async fn running_with_block_waits_for_completion() {
        let ctx = ctx_with_tasks(&[("ag-blocking", "initial output", RunningStatus::Running)]);
        let session = ctx.session.clone();
        let call = tokio::spawn(output_of(
            ctx,
            json!({"task_id": "ag-blocking", "block": true, "timeout": 10000}),
        ));
        let task = session.find_running_task("ag-blocking").unwrap();
        task.append_output(" [final]");
        session.finish_task("ag-blocking", RunningStatus::Completed);

        let r = call.await.unwrap();
        assert!(!r.is_error);
        let sc = r.structured_content.unwrap();
        assert_eq!(sc["retrieval_status"], "success");
        assert_eq!(sc["output"], "initial output [final]");
        assert_eq!(sc["status"], "completed");
    }
}
