//! `session.setPermissionMode`：宿主切换会话的权限模式，立即生效、按设置夹紧、记进
//! 会话日志；以及把宿主与模型切模式的每一次推给订阅者。

use crate::session_pool::*;
use base::context::{DurableState, PlanState, Session, SESSION_STATE_EVENT, SESSION_STATE_NS};
use base::permission::PermissionMode;

const NOTICE_KEY: &str = "permission_mode";

/// How long the reply waits for an idle engine to write a plan-mode change
/// into the log.
const RECORD_WAIT: Duration = Duration::from_secs(5);

impl SessionPool {
    /// `session.setPermissionMode` — the mode every permission decision of
    /// the session takes from now on, a call later in a running turn
    /// included.
    ///
    /// Clamped as `options.permission_mode` is, against the settings the
    /// session runs under. Into `plan` it enters plan from the current mode,
    /// which `ExitPlanMode` returns to; to any other mode it leaves plan. A
    /// mode other than plan is recorded in the session's spec, over
    /// `options.permission_mode`, and plan state in its durable state, so a
    /// rebuild, an eviction or a restart keeps both. A session that is not in
    /// memory only has its log changed.
    pub async fn set_permission_mode(
        &self,
        session_id: &str,
        requested: PermissionMode,
    ) -> Result<api::PermissionModeChoice, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        if !requested.is_user_settable() {
            return Err(PoolError::InvalidRequest(format!(
                "mode: `{}` is set by the engine, not by a host",
                mode_name(requested)
            )));
        }
        if !self.session_exists(session_id).await {
            return Err(PoolError::session_not_found(session_id));
        }
        let mode = self
            .live
            .with_session_lock(session_id, async {
                self.specs.load(session_id).await;
                if let Some(core) = self.live.core_of(session_id).await {
                    if let Some(mode) = self.switch_live_mode(&core, requested).await {
                        return mode;
                    }
                }
                let mode = self.switch_recorded_mode(session_id, requested).await;
                self.specs.release(session_id);
                mode
            })
            .await?;
        Ok(api::PermissionModeChoice {
            session_id: session_id.to_string(),
            mode,
            requested,
            clamped: mode != requested,
        })
    }

    /// `None` when the session left memory while the settings were looked up.
    async fn switch_live_mode(
        &self,
        core: &Arc<SessionCore>,
        requested: PermissionMode,
    ) -> Option<Result<PermissionMode, PoolError>> {
        let binding = self.binding_for(core.project(), &core.scene().id).await;
        if !self.live.holds(core).await {
            return None;
        }
        let mode = effective_permission_mode(&binding.settings, Some(requested));
        Some(self.switch_held_mode(core, mode).await)
    }

    async fn switch_held_mode(
        &self,
        core: &SessionCore,
        mode: PermissionMode,
    ) -> Result<PermissionMode, PoolError> {
        if mode != PermissionMode::Plan {
            self.record_host_mode(core.id(), mode).await?;
        }
        let state = core.state();
        let was_planning = state.permission_mode() == PermissionMode::Plan;
        if state.switch_permission_mode(mode) {
            state.retract_notice(NOTICE_KEY);
            state.post_notice(Some(NOTICE_KEY.into()), notice_of(state.permission_mode()));
            if was_planning != (state.permission_mode() == PermissionMode::Plan) {
                self.record_plan_state(core).await?;
            }
        }
        Ok(state.permission_mode())
    }

    /// The idle engine of `core` writes the plan state it now holds; a running
    /// turn writes it as it ends. Not written, the session counts as ahead of
    /// its log, as after a turn its log did not take.
    async fn record_plan_state(&self, core: &SessionCore) -> Result<(), PoolError> {
        let sid = core.id();
        let idle_input = self
            .live
            .with_live(sid, |live| {
                live.turn
                    .busy(sid)
                    .is_none()
                    .then(|| live.engine.input_tx.clone())
            })
            .await
            .flatten();
        let Some(input_tx) = idle_input else {
            return Ok(());
        };
        let (reply, written) = tokio::sync::oneshot::channel();
        if input_tx.send(InputMessage::Flush { reply }).is_err() {
            return Ok(());
        }
        let failure = match tokio::time::timeout(RECORD_WAIT, written).await {
            Ok(Ok(Ok(()))) | Ok(Err(_)) => return Ok(()),
            Ok(Ok(Err(e))) => format!("writing it failed ({e})"),
            Err(_) => format!(
                "its engine did not write it within {}s",
                RECORD_WAIT.as_secs()
            ),
        };
        core.log().set_behind(true);
        Err(persist_failed(format!(
            "session `{sid}` is now in `{}`, but not in its log: {failure}; it is written \
             again as its next turn ends",
            mode_name(core.state().permission_mode())
        )))
    }

    async fn switch_recorded_mode(
        &self,
        sid: &str,
        requested: PermissionMode,
    ) -> Result<PermissionMode, PoolError> {
        let log = self.directory.load_entries(sid).await?;
        let (binding, _, _) = self.binding_of_log(&log).await;
        let settings = &binding.settings;
        let mode = effective_permission_mode(settings, Some(requested));
        if mode != PermissionMode::Plan {
            self.record_host_mode(sid, mode).await?;
        }
        let mut durable = recorded_durable_state(&log);
        let outside_plan = effective_permission_mode(
            settings,
            self.specs
                .get(sid)
                .and_then(|spec| spec.requested_permission_mode()),
        );
        let plan = match durable.plan {
            _ if mode != PermissionMode::Plan => None,
            None if outside_plan != PermissionMode::Plan => Some(PlanState {
                exit_to: Some(outside_plan),
            }),
            kept => kept,
        };
        if plan != durable.plan {
            durable.plan = plan;
            self.append_durable_state(sid, &durable).await?;
        }
        Ok(mode)
    }

    /// Put `mode` in the spec of `sid` and its log; not in the log, the spec
    /// stays as it was.
    async fn record_host_mode(&self, sid: &str, mode: PermissionMode) -> Result<(), PoolError> {
        let (before, spec) = self
            .specs
            .update(sid, |spec| spec.permission_mode = Some(mode));
        if before.as_ref().and_then(|spec| spec.permission_mode) == Some(mode) {
            return Ok(());
        }
        if let Err(e) = self.specs.try_persist(sid, &spec).await {
            self.specs.restore(sid, before);
            return Err(persist_failed(format!(
                "the mode of session `{sid}` was left as it was: recording `{}` failed ({e})",
                mode_name(mode)
            )));
        }
        Ok(())
    }

    async fn append_durable_state(
        &self,
        sid: &str,
        durable: &DurableState,
    ) -> Result<(), PoolError> {
        let store = self.directory.require_store()?;
        let id = base::session::SessionId::parse(sid)
            .map_err(|_| PoolError::InvalidRequest(format!("invalid session_id: {sid}")))?;
        let payload = serde_json::to_value(durable)
            .map_err(|e| PoolError::Internal(format!("the session state: {e}")))?;
        let entry = history::entry::LogEntry::Extension {
            ns: SESSION_STATE_NS.to_string(),
            event: SESSION_STATE_EVENT.to_string(),
            payload,
        };
        store.append(id, entry).await.map_err(|e| {
            persist_failed(format!(
                "the plan state of session `{sid}` was left as it was: recording it failed ({e})"
            ))
        })?;
        if let Err(e) = store.sync(id).await {
            warn!(session_id = %sid, error = %e, "the recorded plan state was not synced");
        }
        Ok(())
    }

    /// Push the mode changes of `core` to whoever watches the session, for as
    /// long as the session is in memory — across every engine it runs on.
    pub(in crate::session_pool) fn forward_mode_events(core: &SessionCore) {
        let mut events = core.state().subscribe_permission_mode();
        let hub = Arc::downgrade(core.hub());
        let session_id = core.id().to_string();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(change) => {
                        let Some(hub) = hub.upgrade() else { return };
                        hub.publish(&frame_between_turns(
                            &session_id,
                            SessionEvent::PermissionModeChanged(change),
                        ));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }
}

/// The durable state `log` last recorded and still holds after rewinds.
fn recorded_durable_state(log: &[history::entry::EnvelopedEntry]) -> DurableState {
    let voided = history::transcript::voided(log);
    log.iter()
        .zip(&voided)
        .rev()
        .filter(|(_, voided)| !**voided)
        .find_map(|(env, _)| match &env.entry {
            history::entry::LogEntry::Extension { ns, event, payload }
                if ns == SESSION_STATE_NS && event == SESSION_STATE_EVENT =>
            {
                serde_json::from_value(payload.clone()).ok()
            }
            _ => None,
        })
        .unwrap_or_default()
}

fn persist_failed(message: String) -> PoolError {
    PoolError::Engine(base::event::TurnFailure::new(
        base::event::FailureKind::PersistFailed,
        message,
    ))
}

fn mode_name(mode: PermissionMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn notice_of(mode: PermissionMode) -> String {
    match mode {
        PermissionMode::Plan => {
            "The user switched this session to plan mode: only read-only tools \
             run until you present a plan with ExitPlanMode and it is approved."
                .to_string()
        }
        mode => format!(
            "The user switched this session's permission mode to `{}`.",
            mode_name(mode)
        ),
    }
}
