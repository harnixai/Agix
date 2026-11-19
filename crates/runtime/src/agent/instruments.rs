//! Where a session's telemetry goes and how long its parts took.

use std::sync::Arc;
use telemetry::perf::PerfCollector;
use telemetry::{TelemetryEvent, TelemetryHandle};

pub(crate) struct Telemetry {
    handle: TelemetryHandle,
    perf: Arc<PerfCollector>,
}

impl Telemetry {
    pub(crate) fn new(handle: TelemetryHandle) -> Self {
        Self {
            handle,
            perf: Arc::new(PerfCollector::new()),
        }
    }

    pub(crate) fn record(&self, event: TelemetryEvent) {
        let _ = self.handle.record(event);
    }

    pub(crate) fn handle(&self) -> &TelemetryHandle {
        &self.handle
    }

    pub(crate) fn perf(&self) -> &PerfCollector {
        &self.perf
    }

    pub(crate) fn timer(&self, category: &str, name: &str) -> impl FnOnce() -> f64 {
        self.perf.start_timer(category, name)
    }
}

/// Tells the session's host and its telemetry where its model calls moved.
pub(crate) struct SwitchReporter {
    pub(crate) events: crate::agent::messages::EventSender,
    pub(crate) telemetry: TelemetryHandle,
    pub(crate) session_id: String,
}

impl base::provider::RouteObserver for SwitchReporter {
    fn switched(&self, switch: &base::provider::ProviderSwitch) {
        let _ = self.telemetry.record(TelemetryEvent::provider_switched(
            &self.session_id,
            0,
            None,
            telemetry::ProviderSwitchedPayload {
                from: switch.from.clone(),
                to: switch.to.clone(),
                reason: switch.reason.clone(),
                task: switch.task.clone(),
            },
        ));
        let _ = self
            .events
            .send(base::event::AgentEvent::ProviderSwitched(switch.clone()));
    }
}
