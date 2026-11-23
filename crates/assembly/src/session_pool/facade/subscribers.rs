//! 会话的订阅者与待答提示：订阅/退订、本轮终止帧、中断、`respond_to_prompt`。

use crate::session_pool::*;

impl SessionPool {
    /// The hub of live `session_id`.
    pub(in crate::session_pool) async fn hub_of(
        &self,
        session_id: &str,
    ) -> Option<Arc<SessionHub>> {
        self.live
            .core_of(session_id)
            .await
            .map(|core| core.hub().clone())
    }

    /// Hand `event`, which belongs to no turn, to whoever watches
    /// `session_id`.
    pub(in crate::session_pool) async fn publish_between_turns(
        &self,
        session_id: &str,
        event: SessionEvent,
    ) {
        if let Some(hub) = self.hub_of(session_id).await {
            hub.publish(&frame_between_turns(session_id, event));
        }
    }

    /// Push what `core`'s state reports — its tasks, its goal, its permission
    /// mode — to whoever watches the session, for as long as it is in memory.
    pub(in crate::session_pool) fn forward_state_events(&self, core: &SessionCore) {
        Self::forward_task_events(core, self.directory.store().cloned());
        Self::forward_goal_events(core);
        Self::forward_mode_events(core);
    }

    /// Register `watcher` as a watcher of `session_id`.
    ///
    /// Returns the transcript watermark and any unanswered asks, which
    /// together are what a newly opened tab needs to catch up: subscribe
    /// first, then read history up to `last_seq`, then apply the frames that
    /// arrived meanwhile. Reading first and subscribing second loses
    /// everything produced in between — the reason this returns a watermark
    /// at all.
    pub async fn subscribe_session(
        &self,
        session_id: &str,
        watcher: Arc<dyn SessionWatcher>,
    ) -> Result<Subscribed, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        let hub = self
            .hub_of(session_id)
            .await
            .ok_or_else(|| PoolError::session_not_found(session_id))?;
        Ok(self.subscribe_to(&hub, session_id, watcher).await)
    }

    async fn subscribe_to(
        &self,
        hub: &SessionHub,
        session_id: &str,
        watcher: Arc<dyn SessionWatcher>,
    ) -> Subscribed {
        let pending = hub.subscribe(watcher);
        Subscribed {
            session_id: session_id.to_string(),
            last_seq: self.transcript_len(session_id).await,
            pending,
            resumed: false,
        }
    }

    /// [`Self::subscribe_session`], resuming `session_id` first when it is
    /// not in memory — with `session.resume`'s refusals, and no `options`.
    ///
    /// Not what a plain subscribe does: resuming reads the transcript and
    /// takes a slot in a capped pool, and a host drawing a session list
    /// subscribes to rows it is only looking at.
    pub async fn subscribe_resuming(
        &self,
        session_id: &str,
        watcher: Arc<dyn SessionWatcher>,
    ) -> Result<Subscribed, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        if let Some(hub) = self.hub_of(session_id).await {
            return Ok(self.subscribe_to(&hub, session_id, watcher).await);
        }
        let (resumed, pending) = self
            .resume_watched(session_id, false, None, Some(watcher))
            .await?;
        Ok(Subscribed {
            session_id: session_id.to_string(),
            last_seq: resumed.message_count,
            pending,
            resumed: true,
        })
    }

    /// Stop handing this session's frames to `watcher`.
    pub async fn unsubscribe_session(&self, session_id: &str, watcher: WatcherId) {
        if let Some(hub) = self.hub_of(session_id).await {
            hub.unsubscribe(watcher);
        }
    }

    /// A watcher went away — a connection closed: forget it everywhere,
    /// and what it provided. Its tools and hooks are gone from the registry
    /// before its open calls are ended — those it withdrew earlier included —
    /// so a call made meanwhile finds no provider and ends at once.
    ///
    /// That is all a disconnect means: the turn keeps running, because
    /// another connection may be watching and a teardown would cascade into
    /// deleting the session's sidechain transcripts.
    pub async fn drop_watcher(&self, watcher: WatcherId) {
        let host = base::interface::host_call::HostId(watcher.0);
        self.factory.hosts().remove(host);
        for hub in self.live.map_all(|live| live.core.hub().clone()).await {
            hub.end_calls_of(host);
            hub.unsubscribe(watcher);
        }
    }

    /// How many messages this session's transcript holds — its watermark, in
    /// the same positions `session.history` pages by and `step_end` /
    /// `turn_complete` report as `seq`.
    pub(in crate::session_pool) async fn transcript_len(&self, session_id: &str) -> usize {
        let Ok(store) = self.directory.require_store() else {
            return 0;
        };
        self.directory
            .load_entries(session_id)
            .await
            .map(|entries| {
                history::transcript::projected_lengths_with(&entries, store.projection())
                    .last()
                    .copied()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    /// Send the running turn's last frame, if the ticket's entry is still the
    /// session's. Marked under the same lock `session.close` takes the
    /// session out with, so every watcher gets exactly one: this one, or the
    /// one the close sends ([`Self::end_cut_off_turn`]). Returns whether the
    /// session was still there to send it to.
    pub(in crate::session_pool) async fn publish_terminal(
        &self,
        ticket: &TurnTicket,
        frame: &SessionFrame,
        except: Option<WatcherId>,
    ) -> bool {
        self.live
            .with_live_mut(ticket.sid(), |live| {
                if !live.runs(&ticket.core, ticket.generation) {
                    return false;
                }
                live.turn.mark_terminated();
                live.core.hub().publish_except(frame, except);
                true
            })
            .await
            .unwrap_or(false)
    }

    /// `live`, just taken out of the map, was running a turn whose last
    /// frame has not gone out: send its watchers an `error` frame in its
    /// place — `event` says why. Its initiator learns the same from its
    /// `run_turn` result.
    pub(in crate::session_pool) fn end_cut_off_turn(
        &self,
        session_id: &str,
        live: &LiveSession,
        event: SessionEvent,
    ) {
        let Some(turn) = live.turn.running().filter(|turn| !turn.terminated) else {
            return;
        };
        let frame = SessionFrame {
            session_id: Arc::from(session_id),
            turn_id: Some(Arc::from(turn.turn_id.as_str())),
            event,
        };
        live.core.hub().publish_except(&frame, turn.origin);
    }

    /// The turn currently running on `session_id`, if any. `session.get`
    /// reports it as `turn_state`; a missing or unknown session reads as idle,
    /// same as one sitting between turns.
    pub async fn current_turn_of(&self, session_id: &str) -> Option<String> {
        self.live
            .with_live(session_id, |live| live.turn.current_turn_id())
            .await
            .flatten()
    }

    /// Cancel the in-flight turn, keeping the session.
    ///
    /// `EngineCommand::CancelTurn` cancels the turn's own child token, not
    /// the session's — the agent loop keeps running and the next `run_turn`
    /// is served normally. It names the turn, so it lands on that one even
    /// when its message has not reached the engine yet, and on nothing when
    /// it has already ended. Disarming the goal is what stops a goal round
    /// from following a turn that has ended but not yet given its slot back.
    /// Returns whether there was a turn to interrupt; there being none is an
    /// answer, not an error.
    pub async fn interrupt_session(&self, session_id: &str) -> Result<bool, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        self.interrupt_turn(session_id).await
    }

    pub(in crate::session_pool) async fn interrupt_turn(
        &self,
        session_id: &str,
    ) -> Result<bool, PoolError> {
        let (input_tx, running, core) = self
            .live
            .with_live(session_id, |live| {
                (
                    live.engine.input_tx.clone(),
                    live.turn.current_turn_id(),
                    live.core.clone(),
                )
            })
            .await
            .ok_or_else(|| PoolError::session_not_found(session_id))?;
        let Some(turn_id) = running else {
            return Ok(false);
        };
        core.state().goal_disarm();
        let _ = input_tx.send(InputMessage::System {
            kind: EngineCommand::CancelTurn {
                turn_id: Some(turn_id),
            },
            content: String::new(),
        });
        Ok(true)
    }

    /// `session.respondToPrompt` — deliver the host's answer to a pending
    /// `prompt` frame (see `run_turn`'s event loop). Unlike `shutdown_session`,
    /// an unknown `session_id` here is a real caller error (answering a prompt
    /// for a session that isn't running can't silently no-op the way closing
    /// an already-closed session can). An answer of the other `prompt_type`
    /// than the open prompt it names is refused rather than dropped, so a
    /// client that confused the two learns of it.
    pub async fn respond_to_prompt(
        &self,
        session_id: &str,
        prompt_id: String,
        answer: PromptAnswer,
    ) -> Result<(), PoolError> {
        let (input_tx, hub) = self
            .live
            .with_live(session_id, |live| {
                (live.engine.input_tx.clone(), live.core.hub().clone())
            })
            .await
            .ok_or_else(|| PoolError::session_not_found(session_id))?;
        if let Some(asked) = hub
            .ask_kind(&prompt_id)
            .filter(|asked| *asked != answer.kind())
        {
            return Err(PoolError::InvalidRequest(format!(
                "prompt `{prompt_id}` is a {} prompt, not a {}",
                asked.as_str(),
                answer.kind().as_str()
            )));
        }

        // Straight to the engine, whose input demultiplexer wakes the blocked
        // tool call. A `prompt_id` nobody is waiting on (already answered,
        // timed out, stale) is a silent no-op — the documented behaviour.
        // The ask is forgotten when the engine says it ended
        // (`PromptResolved`), not here: only the engine knows whether this
        // answer was the one it took.
        let _ = input_tx.send(match answer {
            PromptAnswer::Permission(decision) => InputMessage::PermissionResponse {
                prompt_id: prompt_id.clone(),
                decision,
            },
            PromptAnswer::Question(answer) => InputMessage::QuestionResponse {
                prompt_id: prompt_id.clone(),
                answer,
            },
            PromptAnswer::McpElicitation(answer) => {
                if let Some(frame) = hub.ask_frame(&prompt_id) {
                    check_against_requested_schema(&frame, &answer)?;
                }
                InputMessage::McpElicitationResponse {
                    prompt_id: prompt_id.clone(),
                    answer,
                }
            }
        });
        Ok(())
    }

    /// `session.respondToolCall` — the host's answer to a `host_tool_call`
    /// frame. A `call_id` nobody waits on is a silent no-op, as for
    /// `respondToPrompt`; one that names an open prompt is refused, and so
    /// is one a host connection provides: that connection answers it with
    /// [`Self::respond_tool_call_from`].
    pub async fn respond_tool_call(
        &self,
        session_id: &str,
        call_id: String,
        reply: base::interface::host_call::HostReply,
    ) -> Result<(), PoolError> {
        self.respond_tool_call_from(None, session_id, call_id, reply)
            .await
    }

    /// [`Self::respond_tool_call`] as `from` answers it.
    pub async fn respond_tool_call_from(
        &self,
        from: Option<WatcherId>,
        session_id: &str,
        call_id: String,
        reply: base::interface::host_call::HostReply,
    ) -> Result<(), PoolError> {
        let input_tx = self
            .answerable(session_id, &call_id, AskKind::HostToolCall, from)
            .await?;
        let _ = input_tx.send(InputMessage::HostToolResponse {
            call_id,
            end: base::interface::host_call::HostCallEnd::Replied(reply),
        });
        Ok(())
    }
}

/// An accepted answer must be what the server asked for: content that fits a
/// form's schema, none for a URL. The schema is the server's; one that does
/// not compile is left for the server to judge.
fn check_against_requested_schema(
    frame: &SessionFrame,
    answer: &base::interface::elicitation::McpElicitationAnswer,
) -> Result<(), PoolError> {
    let base::interface::elicitation::McpElicitationAnswer::Accept { content } = answer else {
        return Ok(());
    };
    let SessionEvent::Agent(AgentEvent::McpElicitationPrompt {
        requested_schema, ..
    }) = &frame.event
    else {
        return Ok(());
    };
    let problems: Vec<String> = match (requested_schema, content) {
        (None, None) => Vec::new(),
        (None, Some(_)) => vec!["a url question's answer carries no content".into()],
        (Some(_), None) => vec!["an accepted form needs `content`".into()],
        (Some(schema), Some(content)) => match jsonschema::validator_for(schema) {
            Ok(validator) => validator
                .iter_errors(content)
                .map(|e| {
                    let at = e.instance_path().to_string();
                    if at.is_empty() {
                        e.to_string()
                    } else {
                        format!("{at}: {e}")
                    }
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "an MCP server's requested schema does not compile");
                Vec::new()
            }
        },
    };
    if problems.is_empty() {
        Ok(())
    } else {
        Err(PoolError::AnswerOutsideSchema { problems })
    }
}
