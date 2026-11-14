//! `DelegateRun`: one sub-agent run as its parent's hooks, telemetry and
//! history see it. Every way of running a delegate (a one-shot call, a
//! persistent team member) starts one when the delegate is built and ends it
//! exactly once; the start and end sequences live nowhere else.

use std::sync::{Arc, Weak};

use base::interface::delegate::{CallSite, DelegateEnd};
use history::store::HistoryStore;
use tokio_util::sync::CancellationToken;

use super::Delegator;
use crate::agent_tool::{
    mark_sidechain_terminal, stop_reason_of, Delegate, SpawnTelemetry, SubagentPayload,
};

pub(crate) struct DelegateRun {
    payload: SubagentPayload,
    spawned: SpawnTelemetry,
    hooks: Weak<hooks::HookRunner>,
    ended: CancellationToken,
    history: Option<Arc<dyn HistoryStore>>,
}

pub(crate) struct Start<'a> {
    pub(crate) stops_with: &'a CancellationToken,
    pub(crate) called_from: Option<&'a CallSite>,
    pub(crate) background: bool,
    pub(crate) stage: Option<String>,
}

pub(crate) enum StopReason {
    OfTheEnd,
    Shutdown,
}

impl DelegateRun {
    pub(crate) async fn start(d: &Delegator, delegate: &Delegate, start: Start<'_>) -> Self {
        let run = Self {
            payload: delegate.payload.clone(),
            spawned: SpawnTelemetry::start(
                d,
                start.called_from,
                &delegate.payload.session_id,
                delegate.payload.agent_type.as_deref(),
                &delegate.settings.values.model.model_name,
                start.background,
                start.stage,
            ),
            hooks: d.env.hooks.clone(),
            ended: d.env.parent.ended.clone(),
            history: delegate.history_store.clone(),
        };
        fire(
            &run.hooks,
            hooks::HookEvent::SubagentStart,
            &run.payload,
            start.stops_with,
        )
        .await;
        run
    }

    /// `SubagentStop` stops with the parent session's end, not with the run's
    /// own token: a delegate that token stopped still reports stopping.
    pub(crate) async fn finish(self, end: &DelegateEnd, reason: StopReason) {
        let Self {
            payload,
            spawned,
            hooks,
            ended,
            history,
        } = self;
        spawned.finish(matches!(end, DelegateEnd::Failed(_)));
        let reason = match reason {
            StopReason::OfTheEnd => stop_reason_of(end),
            StopReason::Shutdown => "shutdown",
        };
        fire(
            &hooks,
            hooks::HookEvent::SubagentStop,
            &payload.clone().stopped(reason),
            &ended,
        )
        .await;
        mark_sidechain_terminal(history.as_ref(), &payload.session_id, end).await;
    }
}

async fn fire(
    hooks: &Weak<hooks::HookRunner>,
    event: hooks::HookEvent,
    payload: &SubagentPayload,
    stop: &CancellationToken,
) {
    if let Some(runner) = hooks.upgrade() {
        payload.fire(&runner, event, stop).await;
    }
}
