//! 压缩：自动/手动/恢复三种触发、PreCompact 钩子、压缩后的恢复上下文与上报。

use super::process::{hook_permission_mode, running_task_summary};
use super::{holds_session_context, CompactSummary, CompactTrigger, NotCompacted, TurnError};
use crate::agent::Agent;
use base::context::Session;
use base::event::AgentEvent;
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
use std::collections::HashSet;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

mod backoff;
mod manual;
mod summary;
use backoff::AutoCompactBackoff;
pub(crate) use manual::CompactArgs;

/// Compaction and micro-compaction, and what they remember between turns.
pub(crate) struct Compaction {
    compactor: Arc<dyn compaction::compact::Compactor>,
    compaction_state: compaction::early_trim::CompactionState,
    /// Reset after compaction so the warning can fire again.
    compact_warning_issued: bool,
    /// Gates the `function_result_clearing` prompt section, so the model is
    /// not told results were cleared when none were.
    tool_results_ever_cleared: bool,
    time_based_mc_config: compaction::time_based_mc::TimeBasedMcConfig,
    pub(crate) cached_mc: compaction::cached::CachedMicroCompact,
    /// The system prompt and tool definitions of the last request, added to
    /// the message total by `context_tokens()`: without it the threshold check
    /// sees only message content and under-counts by a large constant.
    request_overhead_tokens: usize,
    auto_backoff: AutoCompactBackoff,
}

impl Compaction {
    pub(crate) fn new(
        compactor: Arc<dyn compaction::compact::Compactor>,
        cached_mc_enabled: bool,
    ) -> Self {
        Self {
            compactor,
            compaction_state: compaction::early_trim::CompactionState::default(),
            compact_warning_issued: false,
            tool_results_ever_cleared: false,
            time_based_mc_config: compaction::time_based_mc::TimeBasedMcConfig::default(),
            cached_mc: compaction::cached::CachedMicroCompact::new(
                compaction::cached::CachedMcConfig {
                    enabled: cached_mc_enabled,
                    ..Default::default()
                },
            ),
            request_overhead_tokens: 0,
            auto_backoff: Default::default(),
        }
    }

    pub(crate) fn overhead(&self) -> usize {
        self.request_overhead_tokens
    }

    pub(crate) fn set_overhead(&mut self, tokens: usize) {
        self.request_overhead_tokens = tokens;
    }

    pub(crate) fn ever_cleared(&self) -> bool {
        self.tool_results_ever_cleared
    }

    /// The cache edits pending, lent to a request until it is accepted.
    pub(crate) fn lend_cache_edits(&self) -> Vec<String> {
        self.cached_mc.lend_pending_edits()
    }

    pub(crate) fn consume_cache_edits(&mut self, sent: &[String]) {
        self.cached_mc.confirm_sent(sent);
    }

    #[cfg(test)]
    pub(crate) fn set_time_based(&mut self, config: compaction::time_based_mc::TimeBasedMcConfig) {
        self.time_based_mc_config = config;
    }

    #[cfg(test)]
    pub(crate) fn pend_cache_edits(&mut self, ids: Vec<String>) {
        self.cached_mc.record_pass(ids);
    }

    #[cfg(test)]
    pub(crate) fn backoff_misses(&self) -> u32 {
        self.auto_backoff.misses()
    }
}

/// A stale-result trim already applied to the messages, not yet accounted for.
struct PendingStaleTrim {
    trim: compaction::trim::StaleTrim,
    messages: usize,
    tokens_before: usize,
    tokens_after: usize,
    took: std::time::Duration,
}

/// An early trim already applied to the messages, not yet accounted for.
struct PendingEarlyTrim {
    summary: CompactSummary,
    took: std::time::Duration,
}

/// The transcript and the flags a compaction step changes, as they were
/// before it.
struct Checkpoint {
    messages: Vec<ModelMessage>,
    pushed: usize,
    ever_cleared: bool,
    warned: bool,
}

impl Agent {
    /// Routed under the `"compact"` task type, so `model.tasks.compact` can
    /// point summarization at another model.
    pub(super) fn compaction_summarizer(&self) -> Option<compaction::llm_summary::LlmSummarizer> {
        let (model, model_name) = self.model_for_task("compact");
        if model_name.is_empty() {
            return None;
        }
        Some(
            compaction::llm_summary::LlmSummarizer::new(model, model_name)
                .for_session(self.session.session_id.clone()),
        )
    }

    /// Compact session messages if token budget is exceeded.
    ///
    /// The whole step is one transaction on the transcript: trim, the tool
    /// result budget, the predictive pass, the warning and the compaction the
    /// budget forces. Each is applied in turn, because each is judged on what
    /// the one before left, but none is accounted for — reported, its clock
    /// restarted — until the rest went through. A compaction that was
    /// interrupted or failed puts the transcript back as it was.
    pub(crate) async fn fit_context(&mut self, cancel: &CancellationToken) {
        let checkpoint = self.transcript_checkpoint();
        let compactor = self.compaction.compactor.clone();
        let stale = self.trim_stale();
        self.trim_tool_results(&compactor);
        let budget = self.context_budget();
        let early = self.trim_early(&compactor, &budget);
        let abandoned = match self.compact_to_budget(&compactor, &budget, cancel).await {
            Ok(()) => false,
            Err(
                NotCompacted::Cancelled | NotCompacted::Failed(_) | NotCompacted::SummaryFailed(_),
            ) => true,
            Err(NotCompacted::Declined | NotCompacted::NothingToDrop) => cancel.is_cancelled(),
        };
        if abandoned {
            tracing::info!("compaction did not go through; the transcript is put back");
            self.restore_transcript(checkpoint);
            return;
        }
        if let Some(early) = early {
            self.commit_early_trim(early);
        }
        self.commit_stale_trim(stale);
    }

    fn count(&self, messages: &[ModelMessage]) -> usize {
        self.session.token_counter().count_messages(messages)
    }

    fn transcript_checkpoint(&self) -> Checkpoint {
        Checkpoint {
            messages: self.session.messages().to_vec(),
            pushed: self.session.message_timestamps.len(),
            ever_cleared: self.compaction.tool_results_ever_cleared,
            warned: self.compaction.compact_warning_issued,
        }
    }

    fn restore_transcript(&mut self, checkpoint: Checkpoint) {
        let Checkpoint {
            messages,
            pushed,
            ever_cleared,
            warned,
        } = checkpoint;
        self.session.restore_messages(messages);
        self.session.message_timestamps.truncate(pushed);
        self.compaction.tool_results_ever_cleared = ever_cleared;
        self.compaction.compact_warning_issued = warned;
    }

    fn trim_stale(&mut self) -> PendingStaleTrim {
        let compactor = self.compaction.compactor.clone();
        let ages = self.session.message_ages();
        let started = std::time::Instant::now();
        let tokens_before = self.count(self.session.messages());
        let input = compaction::trim::StaleTrimInput {
            message_ages: &ages,
            time_based: &self.compaction.time_based_mc_config,
            cached_due: self.compaction.cached_mc.should_run(),
            cached_keep_recent: self.compaction.cached_mc.keep_recent(),
        };
        let trim = self
            .session
            .trim_in_place(|messages| compactor.trim_stale(messages, &input));
        let tokens_after = if trim.time_based.cleared > 0 {
            self.count(self.session.messages())
        } else {
            tokens_before
        };
        PendingStaleTrim {
            messages: self.session.messages().len(),
            tokens_before,
            tokens_after,
            took: started.elapsed(),
            trim,
        }
    }

    fn commit_stale_trim(&mut self, stale: PendingStaleTrim) {
        let PendingStaleTrim {
            trim,
            messages,
            tokens_before,
            tokens_after,
            took,
        } = stale;
        if trim.cleared() > 0 {
            self.compaction.tool_results_ever_cleared = true;
        }
        if trim.time_based.cleared > 0 {
            tracing::info!(
                cleared = trim.time_based.cleared,
                skipped = trim.time_based.skipped,
                "time-based micro-compact applied"
            );
            let now = std::time::Instant::now();
            self.report_trim(
                &CompactSummary {
                    strategy: format!("{:?}", compaction::compact::CompactStrategy::MicroCompact),
                    messages_before: messages,
                    messages_after: messages,
                    tokens_before,
                    tokens_after,
                },
                now.checked_sub(took).unwrap_or(now),
            );
        }
        if let Some(ids) = trim.cache_edits {
            tracing::info!(
                cleared = trim.cached_cleared,
                "cached micro-compact applied — pending cache_edits generated"
            );
            self.compaction.cached_mc.record_pass(ids);
            self.compaction
                .cached_mc
                .forget_edits_missing_from(self.session.messages());
        }
    }

    fn trim_tool_results(&mut self, compactor: &Arc<dyn compaction::compact::Compactor>) {
        let saved_results = self.config.tool_results.dir.as_deref();
        let modified = self
            .session
            .trim_in_place(|messages| compactor.trim_tool_results(messages, saved_results));
        if modified > 0 {
            self.compaction.tool_results_ever_cleared = true;
            tracing::debug!(modified, "tool result budget enforced");
        }
    }

    /// The warning and the compaction the budget forces. `Ok` when no
    /// compaction was due or one went through.
    async fn compact_to_budget(
        &mut self,
        compactor: &Arc<dyn compaction::compact::Compactor>,
        budget: &base::interface::budget_policy::ContextBudget,
        cancel: &CancellationToken,
    ) -> Result<(), NotCompacted> {
        let threshold = budget.compact_threshold;
        self.warn_about_context(compactor, threshold);
        // Asked again after the reminder, not before it: the reminder is
        // itself context, and a conversation one message short of the
        // threshold crosses it here rather than a turn later.
        if !compactor.should_compact(&compaction::compact::BudgetState {
            context_tokens: self.context_tokens(),
            threshold,
            warned_already: self.compaction.compact_warning_issued,
        }) {
            return Ok(());
        }
        // The threshold is on the whole request, so the conversation gets what
        // the fixed part of it leaves. Where that is nothing, no conversation
        // could fit, and the threshold bounds the conversation by itself.
        let room = match threshold.checked_sub(self.compaction.request_overhead_tokens) {
            Some(room) if room > 0 => room,
            _ if self.session.token_count() > threshold => threshold,
            _ => return Ok(()),
        };
        let context_tokens = self.context_tokens();
        if self.compaction.auto_backoff.holds(context_tokens) {
            return Ok(());
        }
        let target = room.saturating_sub(self.recovery_reserve());
        match self
            .run_compaction(
                target,
                budget.compact_keep_recent,
                CompactTrigger::Auto,
                cancel,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(NotCompacted::Cancelled) => Err(NotCompacted::Cancelled),
            Err(e) => {
                self.compaction
                    .auto_backoff
                    .missed(context_tokens, threshold);
                tracing::warn!(
                    misses = self.compaction.auto_backoff.misses(),
                    context_tokens,
                    threshold,
                    "automatic compaction {e}; not tried again for a while"
                );
                Err(e)
            }
        }
    }

    fn trim_early(
        &mut self,
        compactor: &Arc<dyn compaction::compact::Compactor>,
        budget: &base::interface::budget_policy::ContextBudget,
    ) -> Option<PendingEarlyTrim> {
        let context_tokens = self.context_tokens();
        let early_start = std::time::Instant::now();
        let early_from = self.count(self.session.messages());
        let counter = self.session.token_counter().clone();
        let input = compaction::compact::EarlyTrimInput {
            context_tokens,
            counter: counter.as_ref(),
            context_limit: budget.compact_threshold,
            keep_recent: budget.compact_keep_recent,
            enabled: self.settings().values.compaction.reactive,
            circuit_open: self.compaction.compaction_state.circuit_open,
        };
        match self
            .session
            .trim_in_place(|messages| compactor.trim_early(messages, &input))
        {
            compaction::compact::EarlyTrimOutcome::Trimmed => {
                let messages = self.session.messages().len();
                tracing::info!(context_tokens, "early trim completed");
                Some(PendingEarlyTrim {
                    summary: CompactSummary {
                        strategy: format!(
                            "{:?}",
                            compaction::compact::CompactStrategy::MicroCompact
                        ),
                        messages_before: messages,
                        messages_after: messages,
                        tokens_before: early_from,
                        tokens_after: self.count(self.session.messages()),
                    },
                    took: early_start.elapsed(),
                })
            }
            compaction::compact::EarlyTrimOutcome::NoEffect => {
                self.compaction.compaction_state.record_failure();
                tracing::warn!(
                    failures = self.compaction.compaction_state.consecutive_failures,
                    "early trim had no effect"
                );
                None
            }
            compaction::compact::EarlyTrimOutcome::NotTriggered => None,
        }
    }

    fn commit_early_trim(&mut self, early: PendingEarlyTrim) {
        self.compaction.tool_results_ever_cleared = true;
        self.compaction.compaction_state.record_success();
        let now = std::time::Instant::now();
        self.report_trim(&early.summary, now.checked_sub(early.took).unwrap_or(now));
    }

    fn warn_about_context(
        &mut self,
        compactor: &Arc<dyn compaction::compact::Compactor>,
        threshold: usize,
    ) {
        if let Some(reminder) = compactor.context_warning(&compaction::compact::BudgetState {
            context_tokens: self.context_tokens(),
            threshold,
            warned_already: self.compaction.compact_warning_issued,
        }) {
            self.session.push_message(ModelMessage {
                role: MessageRole::User,
                content: vec![ModelContentBlock::Text { text: reminder }],
            });
            self.compaction.compact_warning_issued = true;
        }
    }

    /// Every trigger takes this path, so what a compaction has to put right
    /// afterwards is put right whichever one ran.
    ///
    /// `target` is what the conversation is cut to before recovery goes back
    /// in front of it. A compaction that leaves the conversation no smaller —
    /// nothing to drop, or recovery costing what was dropped — is not applied.
    pub(crate) async fn run_compaction(
        &mut self,
        target: usize,
        keep: usize,
        trigger: CompactTrigger,
        cancel: &CancellationToken,
    ) -> Result<CompactSummary, NotCompacted> {
        self.run_compaction_shaped(
            target,
            keep,
            trigger,
            &compaction::llm_summary::SummaryShape::default(),
            cancel,
        )
        .await
    }

    /// A summary that must cover exactly what `shape.through` names has no
    /// stand-in: the cascade would drop rounds unsummarized or keep ones the
    /// host asked to have summarized, so without a summary the step fails.
    pub(crate) async fn run_compaction_shaped(
        &mut self,
        target: usize,
        keep: usize,
        trigger: CompactTrigger,
        shape: &compaction::llm_summary::SummaryShape,
        cancel: &CancellationToken,
    ) -> Result<CompactSummary, NotCompacted> {
        if !self.pre_compact_hooks_allow(target, trigger, cancel).await {
            return Err(NotCompacted::Declined);
        }
        let messages_before = self.session.messages().len();
        // The summarizer must see what is about to be destroyed, and recovery
        // re-attaches files from reads that are about to be dropped.
        let pre_compact = self.session.messages().to_vec();

        let compact_start = std::time::Instant::now();
        let (mut compacted, result) = self
            .summarized_or_cascaded(
                &pre_compact,
                target,
                keep,
                trigger,
                shape,
                compact_start,
                cancel,
            )
            .await?;
        if result.strategy == compaction::compact::CompactStrategy::NoOp {
            return Err(NotCompacted::NothingToDrop);
        }

        let recovery =
            self.post_compact_recovery(&pre_compact, &compacted, result.strategy.drops_history());
        compacted.splice(0..0, recovery);
        let tokens_before = self.count(&pre_compact);
        let tokens_after = self.count(&compacted);
        if tokens_after >= tokens_before {
            tracing::info!(
                strategy = ?result.strategy,
                tokens_before,
                tokens_after,
                "compaction would not have shrunk the conversation; left as it was"
            );
            return Err(NotCompacted::NothingToDrop);
        }
        if result.strategy == compaction::compact::CompactStrategy::MicroCompact {
            self.compaction.tool_results_ever_cleared = true;
        }
        let messages_after = compacted.len();
        if let Err(e) = self
            .session
            .replace_messages_after_compact(compacted, tokens_before as u64, tokens_after as u64)
            .await
        {
            tracing::warn!(
                error = %e,
                "the compaction marker is not on disk yet; the next persist writes it"
            );
        }
        let summary = CompactSummary {
            strategy: format!("{:?}", result.strategy),
            messages_before,
            messages_after,
            tokens_before,
            tokens_after,
        };
        self.report_compaction(&summary, result.projection.as_ref(), compact_start);
        self.reset_after_compaction(&summary, result.strategy, trigger);
        self.fire_post_compact_hook(&summary, cancel).await;
        Ok(summary)
    }

    fn reset_after_compaction(
        &mut self,
        summary: &CompactSummary,
        strategy: compaction::compact::CompactStrategy,
        trigger: CompactTrigger,
    ) {
        let CompactSummary {
            messages_before,
            messages_after,
            tokens_before,
            tokens_after,
            ..
        } = *summary;
        // The warning can fire again once the budget fills up again.
        self.compaction.compact_warning_issued = false;
        self.compaction.auto_backoff = AutoCompactBackoff::default();
        self.compaction
            .cached_mc
            .forget_edits_missing_from(self.session.messages());

        // The opening context message and recalled memories are ordinary
        // compaction victims and not part of the recovery above. Resetting
        // their flags injects nothing here; it makes the next turn's
        // injection paths eligible again. A strategy that kept the opening
        // message keeps its flag.
        self.prompt
            .reset_context(holds_session_context(self.session.messages()));

        tracing::info!(
            strategy = ?strategy,
            messages_before,
            messages_after,
            tokens_before,
            tokens_after,
            trigger = ?trigger,
            "compaction completed"
        );
        tracing::debug!(
            "{}",
            compaction::compact::format_context_analysis(&compaction::compact::analyze_context(
                self.session.messages(),
                self.session.token_counter().as_ref()
            ))
        );
    }

    async fn fire_post_compact_hook(&self, summary: &CompactSummary, cancel: &CancellationToken) {
        let CompactSummary {
            messages_before,
            messages_after,
            tokens_before,
            tokens_after,
            ..
        } = *summary;
        if self
            .gate
            .hooks()
            .has_hooks_for(hooks::config::HookEvent::PostCompact)
        {
            let _ = self
                .gate
                .hooks()
                .run(
                    hooks::config::HookEvent::PostCompact,
                    &hooks::HookInput {
                        hook_event_name: "PostCompact".into(),
                        session_id: self.session.session_id.clone(),
                        cwd: self.session_config.working_dir().display().to_string(),
                        permission_mode: hook_permission_mode(self.session_state.permission_mode()),
                        depth: self.identity.agent_depth(),
                        tool_input: Some(serde_json::json!({
                            "strategy": summary.strategy,
                            "messages_before": messages_before,
                            "messages_after": messages_after,
                            "tokens_before": tokens_before,
                            "tokens_after": tokens_after,
                        })),
                        spend: self.open_turn.side_call_node(),
                        ..Default::default()
                    },
                    cancel,
                )
                .await;
        }
    }

    /// Whether the `PreCompact` hooks let a compaction aimed at `target` run.
    pub(super) async fn pre_compact_hooks_allow(
        &self,
        target: usize,
        trigger: CompactTrigger,
        stop: &CancellationToken,
    ) -> bool {
        if !self
            .gate
            .hooks()
            .has_hooks_for(hooks::config::HookEvent::PreCompact)
        {
            return true;
        }
        let hook_result = self
            .gate
            .hooks()
            .run(
                hooks::config::HookEvent::PreCompact,
                &hooks::HookInput {
                    hook_event_name: "PreCompact".into(),
                    session_id: self.session.session_id.clone(),
                    cwd: self.session_config.working_dir().display().to_string(),
                    permission_mode: hook_permission_mode(self.session_state.permission_mode()),
                    depth: self.identity.agent_depth(),
                    tool_input: Some(serde_json::json!({
                        "messages_before": self.session.messages().len(),
                        "token_count": self.context_tokens(),
                        "threshold": target,
                        "trigger": trigger.as_str(),
                    })),
                    tool_name: None,
                    tool_use_id: None,
                    tool_result: None,
                    is_error: None,
                    user_prompt: None,
                    spend: self.open_turn.side_call_node(),
                    ..Default::default()
                },
                stop,
            )
            .await;
        if hook_result.discontinued() {
            tracing::info!("PreCompact hook discontinued — skipping compaction");
            return false;
        }
        if hook_result.blocked().is_some() {
            tracing::info!("PreCompact hook blocked — skipping compaction");
            return false;
        }
        true
    }

    /// What goes back in front of what a compaction `kept` of `pre_compact`:
    /// the files whose reads it dropped, the skills invoked, the deferred
    /// tools loaded, and — when it dropped turns — plan mode and the
    /// background tasks still running.
    pub(super) fn post_compact_recovery(
        &self,
        pre_compact: &[ModelMessage],
        kept: &[ModelMessage],
        drops_history: bool,
    ) -> Vec<ModelMessage> {
        let still_there: HashSet<(String, String)> =
            compaction::compact::extract_recent_reads(kept)
                .into_iter()
                .collect();
        let recent_files = compaction::compact::extract_recent_reads(pre_compact)
            .into_iter()
            .filter(|read| !still_there.contains(read))
            .collect();
        // Each invoked skill's *current* content is resolved here because the
        // `compaction` crate does not depend on `skills`. A name that no
        // longer resolves still gets a record, so it is mentioned by name.
        let invoked_skills = self
            .session_state
            .ledger()
            .invoked_skills
            .clone()
            .into_iter()
            .map(|name| compaction::compact::InvokedSkillRecord {
                content: self.tooling.skills().body(&name),
                last_seq: self.tooling.skills().last_invoked_seq(&name),
                name,
            })
            .collect();
        let mut activated_tools: Vec<String> =
            self.session_state.activated_tools().into_iter().collect();
        activated_tools.sort();
        compaction::compact::build_post_compact_recovery(&compaction::compact::PostCompactContext {
            recent_files,
            invoked_skills,
            in_plan_mode: drops_history
                && self.session_state.permission_mode() == base::permission::PermissionMode::Plan,
            activated_tools,
            running_tasks: if drops_history {
                self.session_state
                    .running_background_tasks()
                    .iter()
                    .map(|task| (task.task_id.clone(), running_task_summary(task)))
                    .collect()
            } else {
                Vec::new()
            },
        })
    }

    /// What any compaction puts back whatever it drops — skills, plan mode,
    /// tasks, loaded tools. Taken off the target, so that the recovery does
    /// not carry the conversation straight back over the threshold. Files are
    /// left out: one is re-attached only when its read was dropped, and the
    /// read cost at least as much.
    pub(super) fn recovery_reserve(&self) -> usize {
        self.count(&self.post_compact_recovery(&[], &[], true))
    }

    pub(super) fn report_compaction(
        &self,
        summary: &CompactSummary,
        projection: Option<&compaction::compact::SnipProjection>,
        started: std::time::Instant,
    ) {
        self.report(summary, projection, started, false);
    }

    fn report_trim(&self, summary: &CompactSummary, started: std::time::Instant) {
        self.report(summary, None, started, true);
    }

    fn report(
        &self,
        summary: &CompactSummary,
        projection: Option<&compaction::compact::SnipProjection>,
        started: std::time::Instant,
        trim: bool,
    ) {
        self.emit(AgentEvent::CompactAction {
            strategy: summary.strategy.clone(),
            messages_before: summary.messages_before,
            messages_after: summary.messages_after,
            turn_id: self.current_turn_id.clone(),
            dropped_rounds: projection.map(|p| p.dropped_rounds),
            dropped_messages: projection.map(|p| p.dropped_messages),
            estimated_tokens_saved: projection.map(|p| p.estimated_tokens_saved),
            tokens_before: summary.tokens_before,
            tokens_after: summary.tokens_after,
            trim,
        });
        self.telemetry
            .record(telemetry::TelemetryEvent::compact_action(
                &self.session.session_id,
                self.session.turn_count,
                Some(self.current_turn_id.clone()),
                telemetry::CompactActionPayload {
                    kind: if trim {
                        telemetry::CompactKind::Trim
                    } else {
                        telemetry::CompactKind::Compact
                    },
                    strategy: summary.strategy.clone(),
                    before_tokens: summary.tokens_before as u64,
                    after_tokens: summary.tokens_after as u64,
                    before_message_count: summary.messages_before,
                    after_message_count: summary.messages_after,
                    success: true,
                    latency_ms: started.elapsed().as_millis() as u64,
                },
            ));
    }

    /// Shorten the conversation after the provider refused it for size.
    ///
    /// The refusal is the measurement here, not the budget: the conversation
    /// that went out was too big whatever the estimate made of it, so the
    /// target is half of what was sent rather than the configured threshold,
    /// which it may well not have reached. An interrupt while it compacts is
    /// not a failure, whatever the compaction came to: the caller sees
    /// `cancel` and ends the turn as cancelled.
    pub(crate) async fn compact_for_retry(
        &mut self,
        refusal: &base::interface::model::ModelError,
        cancel: &CancellationToken,
    ) -> Result<(), TurnError> {
        tracing::warn!(error = %refusal, "refused for size; compacting, then retrying");
        let budget = self.context_budget();
        let sent = self.count(self.session.messages());
        let target = (sent / 2)
            .min(budget.compact_threshold)
            .saturating_sub(self.recovery_reserve());
        let keep = budget.compact_keep_recent.min(5);
        match self
            .run_compaction(target, keep, CompactTrigger::Recovery, cancel)
            .await
        {
            Ok(_) | Err(NotCompacted::Cancelled) => Ok(()),
            Err(_) if cancel.is_cancelled() => Ok(()),
            Err(NotCompacted::SummaryFailed(summary)) => {
                let mut error = summary.clone();
                error.message = format!(
                    "compacting the conversation to retry failed, the summary did ({}); \
                     the request had been refused for size: {refusal}",
                    summary.message
                );
                Err(TurnError::Model(error, false))
            }
            Err(why) => {
                let mut error = refusal.clone();
                error.message = match error.message.is_empty() {
                    true => format!("compacting the conversation to retry {why}"),
                    false => format!(
                        "{}; compacting the conversation to retry {why}",
                        error.message
                    ),
                };
                Err(TurnError::Model(error, false))
            }
        }
    }
}
