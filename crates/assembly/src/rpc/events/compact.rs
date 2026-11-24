//! 压缩三帧的投影：`compact_started`、`compact_ended`，以及压成了或修剪了的 `compact`。

use super::SessionEventPayload;
use base::event::AgentEvent;

impl SessionEventPayload {
    pub(super) fn from_compaction(event: &AgentEvent) -> Option<Self> {
        Some(match event {
            AgentEvent::CompactStarted {
                turn_id: _,
                trigger,
                tokens_before,
                strategy,
            } => Self::CompactStarted {
                trigger: *trigger,
                tokens_before: *tokens_before,
                strategy: strategy.clone(),
            },
            AgentEvent::CompactEnded {
                turn_id: _,
                outcome,
                reason,
            } => Self::CompactEnded {
                outcome: *outcome,
                reason: reason.clone(),
            },
            AgentEvent::CompactAction {
                strategy,
                messages_before,
                messages_after,
                turn_id: _,
                dropped_rounds,
                dropped_messages,
                estimated_tokens_saved,
                tokens_before,
                tokens_after,
                trim,
            } => Self::Compact {
                strategy: strategy.clone(),
                messages_before: *messages_before,
                messages_after: *messages_after,
                tokens_before: *tokens_before,
                tokens_after: *tokens_after,
                dropped_rounds: *dropped_rounds,
                dropped_messages: *dropped_messages,
                estimated_tokens_saved: *estimated_tokens_saved,
                trim: *trim,
            },
            _ => return None,
        })
    }
}
