//! 守护进程级的事件出口 `DaemonEvents`：`daemon.subscribeEvents` 的广播，加上池级
//! 遥测（`pool.jsonl`）。克隆很便宜，要发事件的协作者各持一份。不持锁。

use crate::session_pool::api::DaemonEvent;
use crate::session_pool::pool_telemetry::otel_sink;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tracing::warn;

/// The broadcast and what a subscriber that arrives late is still owed.
#[derive(Clone)]
struct Channel {
    /// `send` returning `Err` only means nobody listens right now.
    tx: broadcast::Sender<DaemonEvent>,
    /// The one question the daemon asks at startup, before any client can
    /// have connected: kept for subscribers that arrive later, until it is
    /// answered.
    import_question: Arc<Mutex<Option<DaemonEvent>>>,
}

#[derive(Clone)]
pub(in crate::session_pool) struct DaemonEvents {
    channel: Channel,
    /// For what happens before any session's own handle exists — a resume
    /// refused, or answered by a session already live — which has nowhere
    /// else to record to. Every per-session event goes through that
    /// session's handle.
    telemetry: telemetry::TelemetryHandle,
}

impl DaemonEvents {
    pub(in crate::session_pool) fn new(settings: &settings::LoadedSettings) -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            channel: Channel {
                tx,
                import_question: Arc::new(Mutex::new(None)),
            },
            telemetry: build_pool_telemetry(settings),
        }
    }

    pub(in crate::session_pool) fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.channel.tx.subscribe()
    }

    pub(in crate::session_pool) fn emit(&self, event: DaemonEvent) {
        if let Some(recorded) = event.telemetry() {
            let _ = self.telemetry.record(recorded);
        }
        let _ = self.channel.tx.send(event);
    }

    pub(in crate::session_pool) fn ask_import(&self, event: DaemonEvent) {
        let mut question = self
            .channel
            .import_question
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *question = Some(event.clone());
        self.emit(event);
    }

    /// A subscription and the question still waiting, taken together so the
    /// question reaches the subscriber exactly once: asked before, it is
    /// handed back here; asked after, it arrives on the subscription.
    pub(in crate::session_pool) fn subscribe_with_question(
        &self,
    ) -> (broadcast::Receiver<DaemonEvent>, Option<DaemonEvent>) {
        let question = self
            .channel
            .import_question
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (self.subscribe(), question.clone())
    }

    pub(in crate::session_pool) fn import_question(&self) -> Option<DaemonEvent> {
        self.channel
            .import_question
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(in crate::session_pool) fn import_answered(&self) {
        self.channel
            .import_question
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    pub(in crate::session_pool) fn record(&self, event: telemetry::TelemetryEvent) {
        let _ = self.telemetry.record(event);
    }

    pub(in crate::session_pool) fn record_startup(&self, timing: telemetry::StartupTimingPayload) {
        self.record(telemetry::TelemetryEvent::startup_timing(
            "", 0, None, timing,
        ));
    }
}

/// The pool-level telemetry handle: the per-session pipeline's shape
/// (`FileRecorder` plus the OTel sink when one is configured), writing to one
/// shared `pool.jsonl` in the project's `state/telemetry/`. A no-op when telemetry is
/// off or `local_data_dir` is not an absolute path.
pub(in crate::session_pool) fn build_pool_telemetry(
    settings: &settings::LoadedSettings,
) -> telemetry::TelemetryHandle {
    #[cfg(feature = "otel")]
    for name in OTLP_HEADER_VARIABLES {
        base::interface::exec::env::hold_back_credential(name);
    }
    if !settings.values.telemetry.enabled {
        return telemetry::TelemetryHandle::noop();
    }
    let dir = &settings.paths.local_data_dir;
    if !dir.is_absolute() {
        // Same reasoning as `create()`'s per-session path: recording
        // nothing is better than scattering a `telemetry/` directory into
        // whatever the process's working directory happens to be.
        return telemetry::TelemetryHandle::noop();
    }
    let path = settings
        .paths
        .config_paths()
        .telemetry_dir()
        .join("pool.jsonl");
    let rec = match telemetry::file_recorder::FileRecorder::new(&path) {
        Ok(rec) => {
            base::paths::ensure_local_data_gitignore(dir);
            rec
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "could not open pool-level telemetry file; pool-level events record nothing");
            return telemetry::TelemetryHandle::noop();
        }
    };
    let mut recorders: Vec<std::sync::Arc<dyn telemetry::TelemetryRecorder>> =
        vec![std::sync::Arc::new(rec)];
    if let Some(otel) = otel_sink(settings) {
        recorders.push(otel);
    }
    let config = telemetry::TelemetryConfig {
        enabled: true,
        mode: telemetry::TelemetryMode::Enabled,
        default_event_enabled: true,
        ..Default::default()
    };
    match telemetry::spawn(config, recorders) {
        Ok((handle, consumer)) => {
            tokio::spawn(consumer);
            handle
        }
        Err(e) => {
            warn!(error = %e, "failed to start pool-level telemetry pipeline");
            telemetry::TelemetryHandle::noop()
        }
    }
}

/// What the OTLP exporter reads its request headers from — where a
/// collector's API key goes — so a build that exports holds them back from
/// every child process, whether or not this pool exports.
#[cfg(feature = "otel")]
const OTLP_HEADER_VARIABLES: &[&str] = &[
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
    "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
];
