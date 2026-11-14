//! The score a host keeps of hook runs: one report per run of an event's
//! hooks, handed to the observer the host installed.

use std::sync::Arc;
use std::time::Duration;

use super::HookRunResult;
use super::HookRunner;
use crate::config::HookEvent;
use crate::payload::HookInput;

/// What one run of an event's hooks amounted to.
#[derive(Debug, Clone)]
pub struct HookRunReport {
    pub event: HookEvent,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub hooks: usize,
    pub duration: Duration,
    pub blocked: bool,
    pub errors: usize,
}

impl HookRunner {
    /// Builder: told, after every run that had hooks to run, what it did.
    pub fn with_observer(mut self, observer: Arc<dyn Fn(HookRunReport) + Send + Sync>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub(super) fn report(
        &self,
        event: HookEvent,
        input: &HookInput,
        hooks: usize,
        duration: Duration,
        result: &HookRunResult,
    ) {
        let Some(observe) = &self.observer else {
            return;
        };
        observe(HookRunReport {
            event,
            session_id: input.session_id.clone(),
            turn_id: input.turn_id.clone(),
            tool_use_id: input.tool_use_id.clone(),
            hooks,
            duration,
            blocked: result.blocked().is_some(),
            errors: result.outcomes.iter().filter(|o| o.is_error()).count(),
        });
    }
}
