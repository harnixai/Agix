//! Holding health checks that come from outside the engine to a time limit, so
//! a plugin's check that blocks costs a report seconds rather than forever.

use base::interface::health::{CheckResult, HealthCheck, HealthChecks, HealthReport};
use std::sync::Arc;
use std::time::Duration;

/// Runs a check on its own thread; the thread is left to finish by itself.
pub struct TimeBounded {
    inner: Arc<dyn HealthCheck>,
    limit: Duration,
}

impl TimeBounded {
    pub fn new(inner: Arc<dyn HealthCheck>, limit: Duration) -> Self {
        Self { inner, limit }
    }
}

impl HealthCheck for TimeBounded {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn check(&self) -> CheckResult {
        let (tx, rx) = std::sync::mpsc::channel();
        let inner = self.inner.clone();
        let spawned = std::thread::Builder::new()
            .name("health-check".into())
            .spawn(move || {
                let _ = tx.send(inner.check());
            });
        if spawned.is_err() {
            return CheckResult::degraded("could not start a thread to run this check");
        }
        match rx.recv_timeout(self.limit) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                CheckResult::degraded(format!("did not answer within {}s", self.limit.as_secs()))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                CheckResult::degraded("the check panicked")
            }
        }
    }
}

/// `checks.report()` with every check held to `limit`.
pub fn report_within(checks: &HealthChecks, limit: Duration) -> HealthReport {
    checks
        .checks()
        .iter()
        .fold(HealthChecks::new(), |bounded, check| {
            bounded.with(Arc::new(TimeBounded::new(check.clone(), limit)))
        })
        .report()
}
