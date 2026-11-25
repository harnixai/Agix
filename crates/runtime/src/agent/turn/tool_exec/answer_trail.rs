//! 宿主的一次调用（`plugin.call`、`mcp.callTool`）记下 `tool.around` 与 `tool.result` 对它的
//! 答复做了什么：宿主拿到的若不是工具自己的答复，就不能当成工具的结果交出去。

use super::ToolExecCtx;
use base::interface::tool_middleware::ToolOutcome;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// The trail of the call `tool_use_id` alone: the calls it makes in turn
/// carry the same context under ids of their own, and leave nothing here.
pub(crate) struct AnswerTrail {
    tool_use_id: String,
    inner: Mutex<Option<ToolOutcome>>,
    transformed: AtomicBool,
}

impl AnswerTrail {
    pub(crate) fn new(tool_use_id: &str) -> Self {
        Self {
            tool_use_id: tool_use_id.to_string(),
            inner: Mutex::new(None),
            transformed: AtomicBool::new(false),
        }
    }

    fn of(ctx: &ToolExecCtx) -> Option<&AnswerTrail> {
        ctx.answer_trail
            .as_deref()
            .filter(|trail| trail.tool_use_id == ctx.tool_use_id)
    }

    /// What the pipeline inside the `tool.around` rings answered.
    pub(super) fn note_inner(ctx: &ToolExecCtx, outcome: &ToolOutcome) {
        if let Some(trail) = Self::of(ctx) {
            *trail.inner.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome.clone());
        }
    }

    pub(super) fn note_transformed(ctx: &ToolExecCtx, changed: bool) {
        if let (Some(trail), true) = (Self::of(ctx), changed) {
            trail.transformed.store(true, Ordering::SeqCst);
        }
    }

    pub(super) fn watches(ctx: &ToolExecCtx) -> bool {
        Self::of(ctx).is_some()
    }

    /// Whether `answered`, what the call came to, is not the tool's own: a
    /// transformer changed it, or the rings around it (`wrapped`) answered
    /// something other than what came out of them.
    pub(crate) fn by_middleware(&self, answered: &ToolOutcome, wrapped: bool) -> bool {
        if self.transformed.load(Ordering::SeqCst) {
            return true;
        }
        wrapped
            && self
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                != Some(answered)
    }
}
