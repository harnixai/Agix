//! A call's result, as the model reads it and as the host is told it: one
//! `tool_result` block for the transcript, one `AgentEvent::ToolResult`.

use crate::agent::EventSender;
use base::event::AgentEvent;
use base::interface::model::ModelContentBlock;

/// One outcome, read the two ways the loop needs it: what the model is shown,
/// and whether that is a failure.
///
/// The second is not `is_err`. A tool that ran and calls its own result a
/// failure — a non-zero exit, an MCP server's `isError`, a trapped guest —
/// belongs on the wire with `is_error: true` and does *not* cancel the
/// siblings sharing its batch, which is what an `Err` here means and what the
/// caller keeps reading `is_err` for.
pub(super) fn read_outcome(
    outcome: &base::interface::tool_middleware::ToolOutcome,
) -> (Answered, Option<Vec<serde_json::Value>>) {
    match outcome {
        Ok(a) => (
            Answered {
                content: a.text.clone(),
                is_error: a.reported_failure,
                file_changes: a.file_changes.clone(),
            },
            a.new_messages.clone(),
        ),
        Err(e) => (Answered::plain(e.clone(), true), None),
    }
}

/// What the model reads of a call, and what only the host is told.
pub(super) struct Answered {
    content: String,
    is_error: bool,
    file_changes: Vec<base::tool::FileChange>,
}

impl Answered {
    pub(super) fn plain(content: impl Into<String>, is_error: bool) -> Self {
        Self {
            content: content.into(),
            is_error,
            file_changes: Vec::new(),
        }
    }
}

pub(super) const DUPLICATE_CALL: &str =
    "[Duplicate tool call skipped — identical call was already made this turn.]";

/// `idx` is the submission index, restoring a deterministic order at write
/// time whatever the completion order was.
#[allow(clippy::too_many_arguments)]
pub(super) fn buffer_result(
    event_tx: &EventSender,
    turn_id: &str,
    pending_result_blocks: &mut Vec<(usize, ModelContentBlock)>,
    idx: usize,
    tool_use_id: &str,
    tool_name: &str,
    Answered {
        content,
        is_error,
        file_changes,
    }: Answered,
) {
    pending_result_blocks.push((
        idx,
        ModelContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: content.clone(),
            is_error: Some(is_error),
        },
    ));
    let _ = event_tx.send(AgentEvent::ToolResult {
        id: tool_use_id.to_string(),
        name: tool_name.to_string(),
        content,
        is_error: Some(is_error),
        turn_id: turn_id.to_string(),
        file_changes,
    });
}
