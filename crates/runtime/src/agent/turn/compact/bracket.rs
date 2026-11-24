//! 一次改写历史的压缩在事件流上的首尾：动手前发 `CompactStarted`，没压成发 `CompactEnded`，
//! 压成了由 `CompactAction` 收尾；压缩的 future 半路被丢掉时补一帧 `cancelled`，开始了就一定有结束。

use super::{CompactTrigger, NotCompacted};
use crate::agent::{Agent, EventSender};
use base::event::{AgentEvent, CompactCause, CompactOutcome};

pub(super) struct Bracket {
    open: Option<(EventSender, String)>,
}

impl Bracket {
    pub(super) fn open(
        agent: &Agent,
        trigger: CompactTrigger,
        shape: &compaction::llm_summary::SummaryShape,
    ) -> Self {
        agent.emit(AgentEvent::CompactStarted {
            turn_id: agent.current_turn_id.clone(),
            trigger: cause_of(trigger),
            tokens_before: agent.count(agent.session.messages()),
            strategy: shape
                .through
                .map(|_| format!("{:?}", compaction::compact::CompactStrategy::FullCompact)),
        });
        Self {
            open: Some((
                agent.channels.event_tx.clone(),
                agent.current_turn_id.clone(),
            )),
        }
    }

    /// The `CompactAction` the caller sends next is the end.
    pub(super) fn compacted(mut self) {
        self.open = None;
    }

    pub(super) fn not_compacted(mut self, why: &NotCompacted) {
        let (outcome, reason) = match why {
            NotCompacted::Declined => (CompactOutcome::Declined, None),
            NotCompacted::NothingToDrop => (CompactOutcome::NothingToCompact, None),
            NotCompacted::Failed(e) => (CompactOutcome::Failed, Some(e.clone())),
            NotCompacted::SummaryFailed(e) => (
                CompactOutcome::Failed,
                Some(format!("summarization failed: {e}")),
            ),
            NotCompacted::Cancelled => (CompactOutcome::Cancelled, None),
        };
        self.end(outcome, reason);
    }

    fn end(&mut self, outcome: CompactOutcome, reason: Option<String>) {
        if let Some((events, turn_id)) = self.open.take() {
            let _ = events.send(AgentEvent::CompactEnded {
                turn_id,
                outcome,
                reason,
            });
        }
    }
}

impl Drop for Bracket {
    fn drop(&mut self) {
        self.end(CompactOutcome::Cancelled, None);
    }
}

fn cause_of(trigger: CompactTrigger) -> CompactCause {
    match trigger {
        CompactTrigger::Auto => CompactCause::Threshold,
        CompactTrigger::Manual => CompactCause::Manual,
        CompactTrigger::Recovery => CompactCause::Overflow,
    }
}
