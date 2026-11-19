//! `session.rewind` 与 `session.checkpoints`：池只占槽、转发、发帧，回滚本身由引擎做——引擎
//! 是会话日志的唯一写者。占槽从检查到答复一直是 `Exclusive(Rewind)`，回合、重建、上下文操作
//! 都进不来；后台任务或团队成员在跑也拒，它们会在回滚中途写文件和根会话日志。

use crate::session_pool::facade::helpers::frame_between_turns;
use crate::session_pool::parts::phase::SessionOp;
use crate::session_pool::*;

impl SessionPool {
    /// `session.rewind`. Not bounded by a timeout: a rewind half done must
    /// not be given up on by the one waiting for it. An engine that goes
    /// away answers `engine_unreachable`, and the `begin` it left is what
    /// `session.checkpoints` reports as interrupted.
    pub async fn rewind_session(
        &self,
        request: api::RewindRequest,
    ) -> Result<api::RewindOutcome, PoolError> {
        let sid = request.session_id.clone();
        let target = request.cut.target().map_err(PoolError::InvalidRequest)?;
        if !self.session_exists(&sid).await {
            return Err(PoolError::no_session(&sid));
        }
        if self.directory.facts_of(&sid).await.session_kind
            == history::entry::SessionKind::Sidechain
        {
            return Err(PoolError::InvalidRequest(format!(
                "`{sid}` is a delegate's session; its writes are recorded in its root session — rewind that"
            )));
        }
        self.refuse_scene_mismatch(&sid).await?;
        self.ensure_live(&sid, None).await?;
        self.wait_out_finishing_turn(&sid).await;
        let (input_tx, core, generation) = self.hold_for_rewind(&sid).await?;
        let pool = self.this.clone();
        let fork_first = request.fork_first && !request.dry_run;
        let operation = tokio::spawn(async move {
            let Some(this) = pool.upgrade() else {
                return Err(PoolError::engine_unreachable("the daemon is stopping"));
            };
            let forked = match fork_first {
                true => this
                    .fork_session(core.id(), None)
                    .await
                    .map(|f| Some(f.session_id)),
                false => Ok(None),
            };
            let answer = match forked {
                Ok(fork) => ask_engine(input_tx, &request, target)
                    .await
                    .map(|r| (r, fork)),
                Err(e) => Err(e),
            };
            this.end_rewind(&core, generation).await;
            let (report, fork) = answer?;
            this.finish_rewind(&core, request, report, fork).await
        });
        operation
            .await
            .unwrap_or_else(|e| Err(PoolError::Internal(format!("the rewind failed: {e}"))))
    }

    async fn hold_for_rewind(
        &self,
        sid: &str,
    ) -> Result<(InputSender, Arc<SessionCore>, Generation), PoolError> {
        self.live
            .with_live_mut(sid, |live| {
                if let Some(busy) = live.busy(sid) {
                    return Err(PoolError::SessionBusy(busy));
                }
                live.turn
                    .begin_exclusive(SessionOp::Rewind)
                    .map_err(|_| PoolError::Internal("an idle slot refused".into()))?;
                Ok((
                    live.engine.input_tx.clone(),
                    live.core.clone(),
                    live.engine.generation,
                ))
            })
            .await
            .ok_or_else(|| PoolError::no_session(sid))?
    }

    async fn end_rewind(&self, core: &Arc<SessionCore>, generation: Generation) {
        self.live
            .with_live_mut(core.id(), |live| {
                if live.runs(core, generation) {
                    live.turn.end_exclusive(SessionOp::Rewind);
                }
            })
            .await;
    }

    async fn finish_rewind(
        &self,
        core: &Arc<SessionCore>,
        request: api::RewindRequest,
        report: runtime::agent::RewindReport,
        fork_session_id: Option<String>,
    ) -> Result<api::RewindOutcome, PoolError> {
        if !report.dry_run {
            let seq = self.session_history(core.id(), 0, 0).await?.total;
            let files = report.files.as_ref();
            let rewound = api::Rewound {
                rewind_id: report.rewind_id.clone(),
                scope: request.scope,
                cut: request.cut.clone(),
                seq,
                files_restored: files.map_or(0, |f| f.restored.len()),
                files_skipped: files.map_or(0, |f| f.skipped.len()),
                overwrote_external: files.map_or_else(Vec::new, |f| f.overwrote_external.clone()),
            };
            core.hub().publish(&frame_between_turns(
                core.id(),
                SessionEvent::Rewound(rewound),
            ));
        }
        Ok(api::RewindOutcome {
            session_id: request.session_id,
            rewind_id: report.rewind_id,
            dry_run: report.dry_run,
            scope: request.scope,
            cut: request.cut,
            conversation: report.conversation,
            files: report.files,
            fork_session_id,
        })
    }

    /// `session.checkpoints` — read from the log, so a session not in
    /// memory is answered without being brought back.
    pub async fn session_checkpoints(
        &self,
        session_id: &str,
    ) -> Result<api::CheckpointList, PoolError> {
        let entries = self.directory.load_entries(session_id).await?;
        let store = self.directory.require_store()?;
        let enabled = base::session::SessionId::parse(session_id)
            .ok()
            .and_then(|sid| store.checkpoint_objects(sid))
            .is_some();
        let voided = history::transcript::voided(&entries);
        let pruned_through = history::checkpoint::plan::pruned_through(&entries);
        let live = history::checkpoint::plan::live_turns(&entries, &voided);
        let pruned_before_turn = pruned_through.and_then(|at| {
            live.iter()
                .find(|turn| turn.last >= at)
                .map(|turn| turn.turn_id.clone())
        });
        let turns = live
            .into_iter()
            .map(|turn| api::CheckpointTurn {
                restorable: enabled && pruned_through.is_none_or(|at| turn.last >= at),
                started_at: rfc3339(turn.started_at),
                prompt: prompt_text(&turn.prompt),
                files: turn
                    .files
                    .into_iter()
                    .map(|f| api::CheckpointFile {
                        path: f.path.as_str().to_string(),
                        tool: f.tool,
                        agent: f.agent,
                    })
                    .collect(),
                turn_id: turn.turn_id,
            })
            .collect();
        let interrupted_rewind =
            history::checkpoint::plan::interrupted_rewind(&entries).map(|(rewind_id, ts)| {
                api::InterruptedRewind {
                    rewind_id,
                    ts: rfc3339(ts),
                }
            });
        Ok(api::CheckpointList {
            session_id: session_id.to_string(),
            turns,
            pruned_before_turn,
            interrupted_rewind,
            enabled,
        })
    }
}

async fn ask_engine(
    input_tx: InputSender,
    request: &api::RewindRequest,
    target: history::checkpoint::RewindTarget,
) -> Result<runtime::agent::RewindReport, PoolError> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    let request = runtime::agent::RewindRequest {
        target,
        scope: request.scope.into(),
        force: request.force,
        allow_partial: request.allow_partial,
        dry_run: request.dry_run,
    };
    input_tx
        .send(InputMessage::Rewind { request, reply })
        .map_err(|_| PoolError::engine_unreachable("the session's engine has stopped"))?;
    answer
        .await
        .map_err(|_| PoolError::engine_unreachable("the engine went away"))?
        .map_err(|e| match e {
            runtime::agent::RewindError::Invalid(why) => PoolError::InvalidRequest(why),
            runtime::agent::RewindError::Precondition(issues) => {
                PoolError::RewindPrecondition(issues)
            }
            runtime::agent::RewindError::Failed(failure) => PoolError::RewindFailed(failure),
        })
}

fn rfc3339(ts: time::OffsetDateTime) -> String {
    ts.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn prompt_text(blocks: &[base::message::ContentBlock]) -> String {
    let text: String = blocks
        .iter()
        .filter_map(|b| match b {
            base::message::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    text.chars().take(200).collect()
}
