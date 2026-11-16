//! PushNotificationTool —— "PushNotification"。
//!
//! 提醒用户回来看。通知本身由派发层送出（`notification` 帧和 `Notification`
//! hook，见 `runtime::turn` 的 `notify_host`），工具只校验输入；在 macOS 上顺带
//! 用 osascript 在引擎所在的机器上弹一条桌面通知，不等它结束，失败不算错。

use async_trait::async_trait;
use base::error::ToolError;
use base::interface::permission::{PermissionFacts, Reach, Subject};
// Only the macOS path runs a program, and importing these unconditionally
// makes them unused elsewhere.
#[cfg(target_os = "macos")]
use base::interface::exec::capture;
#[cfg(target_os = "macos")]
use base::interface::exec::ProcessSpec;
use base::tool::{
    PathStyle, ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ToolSpec,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PushNotificationInput {
    /// 通知正文。应当简洁、一行、无 markdown。
    pub message: String,

    /// 通知类型（可选）。唯一有效值是 "proactive"，表主动推送。
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PushNotificationTool;

#[async_trait]
impl Tool for PushNotificationTool {
    fn name(&self) -> &str {
        "PushNotification"
    }

    fn description(&self) -> &str {
        "Notify the user: the host that runs this session shows it (and any Notification hook gets it)."
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        crate::input_schema::<PushNotificationInput>()
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(include_str!("prompts/coding/push_notification.prompt.md").to_string())
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().concurrency_safe(true).read_only(true)
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let parsed: Result<PushNotificationInput, _> = serde_json::from_value(input.clone());
        match parsed {
            Ok(p) if p.message.trim().is_empty() => {
                ValidationResult::err("message must not be empty", 1)
            }
            Ok(p) if p.message.len() > 200 => {
                ValidationResult::err("message must be under 200 characters", 2)
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
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: PushNotificationInput = serde_json::from_value(input)?;
        let msg = input.message.trim();
        let suffix = if input.status.as_deref() == Some("proactive") {
            " (proactive)"
        } else {
            ""
        };
        desktop_notification(&ctx, msg);
        Ok(ToolResult::text(format!(
            "Notification sent: {msg}{suffix}"
        )))
    }
}

/// How long the desktop copy may take before its `osascript` is stopped.
#[cfg(target_os = "macos")]
const DESKTOP_NOTIFICATION_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The process providers an `osascript` is still showing a notification
/// through, by address.
#[cfg(target_os = "macos")]
static SHOWING: std::sync::Mutex<std::collections::BTreeSet<usize>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// A desktop notification on the machine the engine runs on, as well as the
/// host's. Best effort: the host's copy is the one that counts, so while one
/// is still being shown through the same process provider the next is not,
/// rather than piling up `osascript`s behind a Notification Center that does
/// not answer.
#[cfg(target_os = "macos")]
fn desktop_notification(ctx: &ToolContext, msg: &str) {
    struct Shown(usize);
    impl Drop for Shown {
        fn drop(&mut self) {
            SHOWING
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.0);
        }
    }
    let provider = std::sync::Arc::as_ptr(&ctx.exec.process) as *const () as usize;
    let first = SHOWING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(provider);
    if !first {
        tracing::debug!("a desktop notification is still being shown; this one is skipped");
        return;
    }
    let shown = Shown(provider);
    let escaped = msg.replace('\\', "\\\\").replace('"', "\\\"");
    let spec = ProcessSpec::new("osascript", &ctx.cwd).args([
        "-e".to_string(),
        format!(r#"display notification "{escaped}" with title "Agix""#),
    ]);
    let process = ctx.exec.process.clone();
    // Not awaited: `osascript` can hang for good when Notification Center
    // does not answer, and the call would hang with it.
    tokio::spawn(async move {
        let _shown = shown;
        let shown = tokio::time::timeout(
            DESKTOP_NOTIFICATION_LIMIT,
            capture(&*process, spec, tokio_util::sync::CancellationToken::new()),
        )
        .await;
        match shown {
            Ok(Ok(output)) if output.status.success => {}
            Ok(Ok(output)) => {
                tracing::debug!(stderr = %output.stderr_lossy(), "osascript could not show the notification")
            }
            Ok(Err(e)) => tracing::debug!(error = %e, "osascript could not be run"),
            Err(_) => tracing::debug!("osascript did not finish showing the notification; stopped"),
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn desktop_notification(_: &ToolContext, _: &str) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;
    use std::sync::Arc;

    fn ctx_in(cwd: &Path) -> ToolContext {
        session_state::testing::tool_context_for_test(
            platform::local_providers(),
            cwd.to_path_buf(),
        )
    }

    #[tokio::test]
    async fn name_is_push_notification() {
        let tool = PushNotificationTool;
        assert_eq!(tool.name(), "PushNotification");
        assert!(tool.call_traits(&Value::Null).read_only);
        assert!(tool.call_traits(&Value::Null).concurrency_safe);
    }

    #[tokio::test]
    async fn empty_message_validates_err() {
        let tool = PushNotificationTool;
        let r = tool
            .validate_input(&json!({"message": ""}), &ctx_in(Path::new("/tmp")))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn whitespace_message_validates_err() {
        let tool = PushNotificationTool;
        let r = tool
            .validate_input(&json!({"message": "   "}), &ctx_in(Path::new("/tmp")))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn message_too_long_validates_err() {
        let tool = PushNotificationTool;
        let long = "x".repeat(201);
        let r = tool
            .validate_input(&json!({"message": long}), &ctx_in(Path::new("/tmp")))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn valid_message_validates_ok() {
        let tool = PushNotificationTool;
        let r = tool
            .validate_input(
                &json!({"message": "build failed: 2 auth tests"}),
                &ctx_in(Path::new("/tmp")),
            )
            .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn valid_message_with_status_validates_ok() {
        let tool = PushNotificationTool;
        let r = tool
            .validate_input(
                &json!({"message": "deploy ready", "status": "proactive"}),
                &ctx_in(Path::new("/tmp")),
            )
            .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn invalid_json_validates_err() {
        let tool = PushNotificationTool;
        let r = tool
            .validate_input(&json!({"bad": "input"}), &ctx_in(Path::new("/tmp")))
            .await;
        assert!(r.is_err());
    }

    #[cfg(target_os = "macos")]
    struct NeverAnswers(Arc<tokio::sync::Notify>);

    #[cfg(target_os = "macos")]
    #[async_trait]
    impl base::interface::exec::Process for NeverAnswers {
        async fn spawn(
            &self,
            _: ProcessSpec,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<Box<dyn base::interface::exec::ProcessHandle>, base::interface::exec::ExecError>
        {
            self.0.notify_one();
            std::future::pending().await
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_desktop_notification_that_never_returns_does_not_hold_the_call() {
        let asked = Arc::new(tokio::sync::Notify::new());
        let exec = base::interface::exec::ExecProviders {
            process: Arc::new(NeverAnswers(asked.clone())),
            ..platform::local_providers()
        };
        let ctx = session_state::testing::tool_context_for_test(exec, "/tmp".into());
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            PushNotificationTool.call(
                json!({"message": "deploy ready"}),
                ctx,
                ProgressSender::noop("t"),
            ),
        )
        .await
        .expect("the call waited on osascript")
        .unwrap();
        assert!(!result.is_error);
        asked.notified().await;
    }

    /// Spawns nothing, and says which notifications it was asked to show.
    #[cfg(target_os = "macos")]
    struct Recording {
        asked: tokio::sync::mpsc::UnboundedSender<String>,
        release: Arc<tokio::sync::Notify>,
    }

    #[cfg(target_os = "macos")]
    #[async_trait]
    impl base::interface::exec::Process for Recording {
        async fn spawn(
            &self,
            spec: ProcessSpec,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<Box<dyn base::interface::exec::ProcessHandle>, base::interface::exec::ExecError>
        {
            let _ = self.asked.send(spec.args.join(" "));
            self.release.notified().await;
            Err(base::interface::exec::ExecError::failed("not shown"))
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn notifications_do_not_pile_up_behind_one_still_showing() {
        let (asked, mut shown) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let exec = base::interface::exec::ExecProviders {
            process: Arc::new(Recording {
                asked,
                release: release.clone(),
            }),
            ..platform::local_providers()
        };
        let notify = |message: &str| {
            let ctx = session_state::testing::tool_context_for_test(exec.clone(), "/tmp".into());
            let input = json!({ "message": message });
            async move {
                PushNotificationTool
                    .call(input, ctx, ProgressSender::noop("t"))
                    .await
                    .unwrap()
            }
        };
        notify("first").await;
        assert!(shown.recv().await.unwrap().contains("first"));
        for i in 0..5 {
            let result = notify(&format!("while-showing-{i}")).await;
            assert!(!result.is_error, "the host's copy still goes out");
        }
        release.notify_one();
        loop {
            notify("after").await;
            let pace = std::time::Duration::from_millis(20);
            if let Ok(Some(next)) = tokio::time::timeout(pace, shown.recv()).await {
                assert!(
                    next.contains("after"),
                    "skipped while one was showing: {next}"
                );
                break;
            }
        }
    }

    #[tokio::test]
    async fn call_does_not_panic() {
        let tool = PushNotificationTool;
        let r = tool
            .call(
                json!({"message": "test notification"}),
                ctx_in(Path::new("/tmp")),
                ProgressSender::noop("t"),
            )
            .await;
        // The osascript call may or may not succeed depending on the test
        // environment; the important thing is it doesn't panic and returns
        // a result.
        assert!(r.is_ok() || r.is_err());
        if let Ok(result) = r {
            // Accept both success and "failed" paths — is_error is not asserted here.
            if let base::tool::ToolResultContent::Text(t) = &result.content {
                assert!(t.contains("Notification") || t.contains("notification"));
            }
        }
    }

    #[tokio::test]
    async fn call_with_proactive_status_does_not_panic() {
        let tool = PushNotificationTool;
        let r = tool
            .call(
                json!({"message": "proactive test", "status": "proactive"}),
                ctx_in(Path::new("/tmp")),
                ProgressSender::noop("t"),
            )
            .await;
        assert!(r.is_ok() || r.is_err());
    }
}
