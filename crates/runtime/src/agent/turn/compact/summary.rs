//! 压缩里"摘要"那一步：先请模型写摘要，写不成再走级联；手动指定了截止点时没有级联可退。

use super::{CompactTrigger, NotCompacted};
use crate::agent::Agent;
use base::interface::model::ModelMessage;
use tokio_util::sync::CancellationToken;

/// What asking for an LLM summary came to, short of an interrupt.
pub(super) enum SummaryAttempt {
    Written(Vec<ModelMessage>, compaction::compact::CompactResult),
    /// Not wanted, no summarizer, or nothing the summary could have told.
    Skipped,
    ModelFailed(base::interface::model::ModelError),
}

impl Agent {
    pub(super) async fn summarized_or_cascaded(
        &self,
        pre_compact: &[ModelMessage],
        target: usize,
        keep: usize,
        trigger: CompactTrigger,
        shape: &compaction::llm_summary::SummaryShape,
        compact_start: std::time::Instant,
        cancel: &CancellationToken,
    ) -> Result<(Vec<ModelMessage>, compaction::compact::CompactResult), NotCompacted> {
        Ok(match self
            .try_llm_summary(pre_compact, target, keep, trigger, shape, cancel)
            .await?
        {
            SummaryAttempt::Written(compacted, result) => (compacted, result),
            SummaryAttempt::Skipped if shape.through.is_some() => {
                return Err(NotCompacted::Failed(
                    "compacting up to a chosen point needs a summary model and found nothing to summarize".into(),
                ))
            }
            SummaryAttempt::ModelFailed(error) if shape.through.is_some() => {
                return Err(NotCompacted::SummaryFailed(error))
            }
            SummaryAttempt::Skipped => {
                self.cascade_compact(pre_compact, target, keep, compact_start)
                    .await?
            }
            // The cascade still gets its turn; when it cannot help either, the
            // summary's failure is why nothing was compacted.
            SummaryAttempt::ModelFailed(error) => match self
                .cascade_compact(pre_compact, target, keep, compact_start)
                .await
            {
                Ok((_, result))
                    if result.strategy == compaction::compact::CompactStrategy::NoOp =>
                {
                    return Err(NotCompacted::SummaryFailed(error))
                }
                Ok(ok) => ok,
                Err(_) => return Err(NotCompacted::SummaryFailed(error)),
            },
        })
    }

    pub(super) async fn try_llm_summary(
        &self,
        pre_compact: &[ModelMessage],
        target: usize,
        keep: usize,
        trigger: CompactTrigger,
        shape: &compaction::llm_summary::SummaryShape,
        cancel: &CancellationToken,
    ) -> Result<SummaryAttempt, NotCompacted> {
        // Ahead of the cascade because every cascade layer destroys
        // information; a summary written after any of them could only
        // describe the wreckage. Every failure falls through to the cascade,
        // so the summary can fail to help but never fails a turn.
        let compactor = self.compaction.compactor.clone();
        Ok(match self.compaction_summarizer() {
            Some(summarizer)
                if trigger == CompactTrigger::Manual
                    || compactor.wants_llm_summary(
                        pre_compact,
                        target,
                        keep,
                        self.session.token_counter().as_ref(),
                    ) =>
            {
                match compaction::llm_summary::full_compact_with(
                    &summarizer,
                    pre_compact.to_vec(),
                    target,
                    keep,
                    self.session.token_counter().as_ref(),
                    shape,
                    cancel,
                )
                .await
                {
                    Ok((compacted, result)) => {
                        tracing::info!("compaction: LLM summary produced (FullCompact)");
                        SummaryAttempt::Written(compacted, result)
                    }
                    // An interrupt can also surface as the summary request's
                    // own error, depending on where the stream was when it hit.
                    Err(e)
                        if cancel.is_cancelled()
                            || matches!(e, compaction::compact::CompactError::Cancelled) =>
                    {
                        tracing::info!("compaction: interrupted while summarizing; left as it was");
                        return Err(NotCompacted::Cancelled);
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "compaction: LLM summary unavailable — falling back to the non-LLM cascade"
                        );
                        match e {
                            compaction::compact::CompactError::Model(error) => {
                                SummaryAttempt::ModelFailed(error)
                            }
                            _ => SummaryAttempt::Skipped,
                        }
                    }
                }
            }
            _ => SummaryAttempt::Skipped,
        })
    }

    pub(super) async fn cascade_compact(
        &self,
        pre_compact: &[ModelMessage],
        target: usize,
        keep: usize,
        compact_start: std::time::Instant,
    ) -> Result<(Vec<ModelMessage>, compaction::compact::CompactResult), NotCompacted> {
        let messages_before = self.session.messages().len();
        match self
            .compaction
            .compactor
            .compact(
                pre_compact.to_vec(),
                target,
                keep,
                self.session.token_counter().as_ref(),
            )
            .await
        {
            Ok(ok) => Ok(ok),
            Err(e) => {
                tracing::warn!(error = %e, "failed to compact messages, continuing with full context");
                let tokens = self.context_tokens() as u64;
                self.telemetry
                    .record(telemetry::TelemetryEvent::compact_action(
                        &self.session.session_id,
                        self.session.turn_count,
                        Some(self.current_turn_id.clone()),
                        telemetry::CompactActionPayload {
                            kind: telemetry::CompactKind::Compact,
                            strategy: "error".into(),
                            before_tokens: tokens,
                            after_tokens: tokens,
                            before_message_count: messages_before,
                            after_message_count: messages_before,
                            success: false,
                            latency_ms: compact_start.elapsed().as_millis() as u64,
                        },
                    ));
                Err(NotCompacted::Failed(e.to_string()))
            }
        }
    }
}
