//! TaskStopTool — stop a running background task by ID.
//!
//! Accepts `task_id` (required).
//!
//! Finds the task among the session's and cancels it via its
//! CancellationToken.

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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskStopInput {
    /// The ID of the background task to stop
    #[serde(default)]
    pub task_id: Option<String>,
}

/// Stop a running background task by ID.
///
/// Checks that the task exists and is running before sending the cancel signal.
#[derive(Debug, Default, Clone, Copy)]
pub struct TaskStopTool;

#[async_trait]
impl Tool for TaskStopTool {
    fn description(&self) -> &str {
        "Stop a running background task by ID"
    }

    fn name(&self) -> &str {
        "TaskStop"
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<TaskStopInput>()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().concurrency_safe(true)
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<TaskStopInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) => {
                let id = p.task_id;
                match id {
                    Some(id) if id.trim().is_empty() => {
                        ValidationResult::err("Missing required parameter: task_id", 1)
                    }
                    Some(_) => ValidationResult::Ok,
                    None => ValidationResult::err("Missing required parameter: task_id", 1),
                }
            }
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
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
        let input: TaskStopInput = serde_json::from_value(input)?;
        let id = input.task_id.unwrap_or_default();

        let Some(task) = ctx.session.find_running_task(&id) else {
            return Ok(ToolResult::error_text(format!(
                "No task found with ID: {id}"
            )));
        };
        let status = task
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if status != RunningStatus::Running {
            return Ok(ToolResult::error_text(format!(
                "Task {id} is not running (status: {status:?})"
            )));
        }
        if ctx.session.cancel_running_task(&id) {
            Ok(ToolResult::text(format!("Successfully stopped task: {id}"))
                .with_structured(json!({"task_id": id})))
        } else {
            Ok(ToolResult::error_text(format!("Failed to stop task: {id}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::context::TaskType;
    use serde_json::json;
    use std::path::Path;

    /// A context whose session runs `tasks`, each ended as its status says.
    fn ctx_with_tasks(tasks: &[(&str, RunningStatus)]) -> ToolContext {
        let ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            Path::new("/").to_path_buf(),
        );
        for (id, status) in tasks {
            ctx.session.register_task(
                id.to_string(),
                TaskType::Code,
                tokio_util::sync::CancellationToken::new(),
                serde_json::Value::Null,
            );
            if *status != RunningStatus::Running {
                ctx.session.finish_task(id, status.clone());
            }
        }
        ctx
    }

    #[tokio::test]
    async fn name_is_task_stop() {
        let tool = TaskStopTool;
        assert_eq!(tool.name(), "TaskStop");
        assert!(!tool.call_traits(&Value::Null).read_only);
        assert!(tool.call_traits(&Value::Null).concurrency_safe);
    }

    #[tokio::test]
    async fn requires_at_least_one_id() {
        let tool = TaskStopTool;
        let r = tool.validate_input(&json!({}), &ctx_with_tasks(&[])).await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn empty_task_id_is_rejected() {
        let tool = TaskStopTool;
        let r = tool
            .validate_input(&json!({"task_id": ""}), &ctx_with_tasks(&[]))
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn stops_running_task() {
        let ctx = ctx_with_tasks(&[("ag-test", RunningStatus::Running)]);
        let tool = TaskStopTool;
        let r = tool
            .call(
                json!({"task_id": "ag-test"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(ref t) => {
                assert!(t.contains("Successfully stopped"), "got: {t}");
            }
            _ => panic!("expected Text content"),
        }
    }

    #[tokio::test]
    async fn errors_on_not_found() {
        let ctx = ctx_with_tasks(&[]);
        let tool = TaskStopTool;
        let r = tool
            .call(
                json!({"task_id": "nonexistent"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(ref t) => {
                assert!(t.contains("No task found"), "got: {t}");
            }
            _ => panic!("expected Text content"),
        }
    }

    #[tokio::test]
    async fn errors_on_not_running() {
        let ctx = ctx_with_tasks(&[("ag-completed", RunningStatus::Completed)]);
        let tool = TaskStopTool;
        let r = tool
            .call(
                json!({"task_id": "ag-completed"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        match r.content {
            base::tool::ToolResultContent::Text(ref t) => {
                assert!(t.contains("not running"), "got: {t}");
            }
            _ => panic!("expected Text content"),
        }
    }
}
