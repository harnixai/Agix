//! One `orchestrate` call as a value: set the team up on disk, walk its
//! stages (dispatch, aggregate, record, report), then write the final state
//! and registry entry.

use base::context::EngineConfig;
use base::error::ToolError;
use base::interface::model::Model;
use base::tool::{ToolContext, ToolResult};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::judge::aggregate;
use super::record::{build_state_snapshot, chrono_id, team_declaration};
use super::stage::StageRun;
use super::{DefaultCoordinator, MemberEnd, OrchestrateRequest, TeammateLifecycle};
use crate::tool::TeamStage;

type Section = (String, String, bool);

pub(super) struct Run<'a> {
    coord: &'a DefaultCoordinator,
    model: Arc<dyn Model>,
    config: Arc<EngineConfig>,
    stage_count: usize,
    name: String,
    ctx: ToolContext,
    permission_mode: base::permission::PermissionMode,
    team_id: String,
    team_dir: PathBuf,
    sp_path: PathBuf,
    mailbox: Arc<crate::mailbox::MailboxStore>,
    memory: Arc<crate::team_memory::TeamMemoryStore>,
    scratch: String,
    lifecycles: HashMap<String, TeammateLifecycle>,
    any_err: bool,
    cancelled: bool,
    started: usize,
}

pub(super) async fn orchestrate(
    coord: &DefaultCoordinator,
    req: OrchestrateRequest,
) -> Result<ToolResult, ToolError> {
    let (mut run, stages) = Run::start(coord, req).await?;
    run.walk(&stages).await;
    run.finish().await
}

impl<'a> Run<'a> {
    async fn start(
        coord: &'a DefaultCoordinator,
        req: OrchestrateRequest,
    ) -> Result<(Self, Vec<TeamStage>), ToolError> {
        let OrchestrateRequest {
            model,
            config,
            stages,
            name,
            scratchpad,
            ctx,
            permission_mode,
        } = req;
        let permission_mode = permission_mode.unwrap_or(base::permission::PermissionMode::Plan);

        let all_labels: Vec<String> = stages
            .iter()
            .flat_map(|s| s.agents.iter().map(|a| a.label.clone()))
            .collect();
        let team_id = format!("team-{}-{}", name, chrono_id());
        let team_dir = ctx.session.env().cwd.join(".agix/teams").join(&team_id);
        let sp_path = team_dir.join("SCRATCHPAD.md");
        if let Some(p) = sp_path.parent() {
            let _ = tokio::fs::create_dir_all(p).await;
        }

        let declaration = team_declaration(
            &team_id,
            &name,
            coord.scene_id.as_deref(),
            ctx.session.env().cwd.display().to_string(),
            ctx.session_id.clone(),
            permission_mode,
            &stages,
        );
        write_declaration(&team_dir, &team_id, &name, &declaration).await?;

        let mailbox = Arc::new(crate::mailbox::MailboxStore::with_persistence(
            all_labels,
            team_dir.join("mailbox"),
        ));
        let memory = Arc::new(crate::team_memory::TeamMemoryStore::for_team_dir(&team_dir));

        let scratch = initial_scratch(&name, &team_id, stages.len(), scratchpad);
        let _ = tokio::fs::write(&sp_path, &scratch).await;

        let lifecycles = stages
            .iter()
            .flat_map(|s| s.agents.iter())
            .map(|a| (a.label.clone(), TeammateLifecycle::Idle))
            .collect();

        let stage_count = stages.len();
        let run = Self {
            coord,
            model,
            config,
            stage_count,
            name,
            ctx,
            permission_mode,
            team_id,
            team_dir,
            sp_path,
            mailbox,
            memory,
            scratch,
            lifecycles,
            any_err: false,
            cancelled: false,
            started: 0,
        };
        Ok((run, stages))
    }

    async fn walk(&mut self, stages: &[TeamStage]) {
        for (si, stage) in stages.iter().enumerate() {
            if self.ctx.cancel.is_cancelled() {
                self.cancelled = true;
                break;
            }
            self.started = si + 1;
            self.run_stage(si, stage, stages).await;
            if self.cancelled {
                break;
            }
        }
    }

    async fn run_stage(&mut self, si: usize, stage: &TeamStage, all: &[TeamStage]) {
        let stage_count = self.stage_count;
        let stage_start = std::time::Instant::now();
        let stage_members: Vec<String> = stage.agents.iter().map(|a| a.label.clone()).collect();

        self.coord.emit_progress(
            &self.name,
            &self.team_id,
            &stage.name,
            si,
            stage_count,
            base::event::TeamStageStatus::Started,
            stage_members.clone(),
            Vec::new(),
        );
        self.event(
            "stage_started",
            crate::persist::TeamEvent::StageStarted {
                index: si,
                name: stage.name.clone(),
                members: stage_members.clone(),
            },
        )
        .await;

        let mut sections = match &self.coord.delegator {
            Some(delegator) => {
                let later: Vec<String> = all[(si + 1).min(stage_count)..]
                    .iter()
                    .map(|s| s.name.clone())
                    .collect();
                self.dispatch(delegator.clone(), si, stage_count, stage, &later)
                    .await
            }
            None => self.without_delegator(stage),
        };
        self.cancelled |= self.ctx.cancel.is_cancelled();

        if let Some(mode) = stage.aggregate.filter(|_| !self.cancelled) {
            if !sections.is_empty() {
                sections = aggregate(
                    &*self.model,
                    &self.config.model,
                    mode,
                    &stage.name,
                    &sections,
                    Some(self.ctx.session_id.as_str()),
                    &self.ctx.cancel,
                )
                .await;
            }
        }
        self.cancelled |= self.ctx.cancel.is_cancelled();

        self.append_scratch(si, stage, &sections).await;
        self.finish_stage(
            si,
            stage_count,
            stage,
            stage_members,
            &sections,
            stage_start,
        )
        .await;
    }

    fn without_delegator(&mut self, stage: &TeamStage) -> Vec<Section> {
        tracing::warn!(
            "TeamCreate: no delegator — stage '{}' agents will not execute",
            stage.name
        );
        self.any_err |= !stage.agents.is_empty();
        stage
            .agents
            .iter()
            .map(|a| {
                (
                    a.label.clone(),
                    "[no delegator — this session cannot start team members]".to_string(),
                    true,
                )
            })
            .collect()
    }

    async fn dispatch(
        &mut self,
        delegator: Arc<dyn base::interface::delegate::Delegator>,
        si: usize,
        stage_count: usize,
        stage: &TeamStage,
        later_stages: &[String],
    ) -> Vec<Section> {
        // Every member goes Active up front: the concurrent futures do not mutate `lifecycles`.
        for agent_spec in &stage.agents {
            self.lifecycles
                .insert(agent_spec.label.clone(), TeammateLifecycle::Active);
            self.event(
                "member_spawned",
                crate::persist::TeamEvent::MemberSpawned {
                    label: agent_spec.label.clone(),
                    agent_id: None,
                    session_id: None,
                },
            )
            .await;
        }
        self.snapshot(si, "stage start").await;

        let team_memory = {
            let memory = self.memory.clone();
            tokio::task::spawn_blocking(move || memory.build_team_memory_prompt())
                .await
                .unwrap_or_default()
        };
        let sp_display = self.sp_path.display().to_string();
        let results = StageRun {
            delegator: delegator.as_ref(),
            ctx: &self.ctx,
            team: &self.name,
            team_id: &self.team_id,
            stage,
            stage_index: si,
            stage_count,
            later_stages,
            scratchpad: &sp_display,
            team_memory: &team_memory,
            mailbox: &self.mailbox,
            memory: &self.memory,
            permission_mode: self.permission_mode,
            concurrency: self.coord.stage_concurrency,
        }
        .run_members()
        .await;

        let mut sections = Vec::new();
        for (label, text, end) in results {
            let lifecycle = match end {
                MemberEnd::Cancelled => TeammateLifecycle::Cancelled,
                MemberEnd::Done | MemberEnd::Failed => TeammateLifecycle::Completed,
            };
            self.lifecycles.insert(label.clone(), lifecycle);
            let is_error = end != MemberEnd::Done;
            match end {
                MemberEnd::Done => {}
                MemberEnd::Failed => self.any_err = true,
                MemberEnd::Cancelled => self.cancelled = true,
            }
            self.event(
                "member_completed",
                crate::persist::TeamEvent::MemberCompleted {
                    label: label.clone(),
                    is_error,
                    cancelled: end == MemberEnd::Cancelled,
                },
            )
            .await;
            sections.push((label, text, is_error));
        }
        sections
    }

    async fn append_scratch(&mut self, si: usize, stage: &TeamStage, sections: &[Section]) {
        let mut md = format!("\n## {}_{}\n\n", si + 1, stage.name);
        for (l, b, e) in sections {
            md.push_str(&format!(
                "### {}{}\n{b}\n\n",
                l,
                if *e { " (ERROR)" } else { "" }
            ));
        }
        self.scratch.push_str(&md);
        let _ = tokio::fs::write(&self.sp_path, &self.scratch).await;
    }

    async fn finish_stage(
        &self,
        si: usize,
        stage_count: usize,
        stage: &TeamStage,
        stage_members: Vec<String>,
        sections: &[Section],
        stage_start: std::time::Instant,
    ) {
        let failed: Vec<String> = sections
            .iter()
            .filter(|(_, _, e)| *e)
            .map(|(l, _, _)| l.clone())
            .collect();
        self.coord.emit_progress(
            &self.name,
            &self.team_id,
            &stage.name,
            si,
            stage_count,
            base::event::TeamStageStatus::Completed,
            stage_members,
            failed.clone(),
        );
        if let Some(handle) = &self.coord.telemetry {
            let _ = handle.record(telemetry::TelemetryEvent::team_stage_complete(
                &self.ctx.session_id,
                self.ctx.turn_no,
                None,
                telemetry::TeamStageCompletePayload {
                    stage_name: stage.name.clone(),
                    agent_count: stage.agents.len(),
                    duration_ms: stage_start.elapsed().as_millis() as u64,
                    success: failed.is_empty(),
                    agents_with_errors: failed.len(),
                },
            ));
        }
        self.event(
            "stage_completed",
            crate::persist::TeamEvent::StageCompleted { index: si, failed },
        )
        .await;
        // One snapshot per stage keeps a crashed run's `state.json` at most one stage stale.
        self.snapshot(si, "stage completion").await;
    }

    async fn finish(self) -> Result<ToolResult, ToolError> {
        let final_status = if self.cancelled {
            crate::persist::TeamStatus::Cancelled
        } else if self.any_err {
            crate::persist::TeamStatus::Failed
        } else {
            crate::persist::TeamStatus::Completed
        };
        let state = build_state_snapshot(
            self.started.saturating_sub(1),
            final_status,
            &self.lifecycles,
        );
        if let Err(e) = crate::persist::write_team_state_atomic(&self.team_dir, &state).await {
            tracing::warn!(team_id = %self.team_id, error = %e, "failed to write final state.json");
        }
        self.event(
            "team_completed",
            crate::persist::TeamEvent::TeamCompleted {
                status: final_status,
            },
        )
        .await;

        if let Some(registry) = &self.coord.registry {
            registry.record_batch(crate::registry::TeamInfo {
                team_id: self.team_id.clone(),
                name: self.name.clone(),
                created_at_secs: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                team_dir: self.team_dir.clone(),
                members: self
                    .lifecycles
                    .iter()
                    .map(|(label, lifecycle)| crate::registry::TeamMemberInfo {
                        label: label.clone(),
                        lifecycle: *lifecycle,
                        idle_since_secs: None,
                    })
                    .collect(),
                // A batch call's grant applies to that call only and is not kept for reuse.
                permission_mode: None,
            });
        }

        let mut text = format!(
            "{}\n\n_(scratchpad: {})_",
            self.scratch,
            self.sp_path.display()
        );
        if self.cancelled {
            text.push_str(&format!(
                "\n\nTeam `{}` was cancelled: {} of {} stages were started.",
                self.name, self.started, self.stage_count,
            ));
        }
        Ok(ToolResult::text(text).with_error(self.any_err || self.cancelled))
    }

    async fn snapshot(&self, stage_index: usize, when: &str) {
        let snapshot = build_state_snapshot(
            stage_index,
            crate::persist::TeamStatus::Running,
            &self.lifecycles,
        );
        if let Err(e) = crate::persist::write_team_state_atomic(&self.team_dir, &snapshot).await {
            tracing::warn!(team_id = %self.team_id, error = %e, "failed to write state.json snapshot ({when})");
        }
    }

    async fn event(&self, kind: &str, event: crate::persist::TeamEvent) {
        append_event(&self.team_dir, &self.team_id, kind, event).await;
    }
}

async fn append_event(
    team_dir: &std::path::Path,
    team_id: &str,
    kind: &str,
    event: crate::persist::TeamEvent,
) {
    if let Err(e) = crate::persist::append_team_event(
        team_dir,
        event,
        crate::persist::EventsRetention::default(),
    )
    .await
    {
        tracing::warn!(team_id = %team_id, event = kind, error = %e, "failed to append team event");
    }
}

fn initial_scratch(
    name: &str,
    team_id: &str,
    stage_count: usize,
    scratchpad: Option<String>,
) -> String {
    let mut scratch = format!("# Team `{name}`\n\nTeam id: `{team_id}`\nStages: {stage_count}\n");
    if let Some(s) = scratchpad {
        scratch.push_str("\n## 0_initial_context\n\n");
        scratch.push_str(&s);
        scratch.push('\n');
    }
    scratch
}

/// Unlike `state.json` and `events.jsonl`, `team.json` has no snapshot to
/// fall back to, so a failed write must fail the creation outright.
async fn write_declaration(
    team_dir: &std::path::Path,
    team_id: &str,
    name: &str,
    declaration: &crate::persist::TeamDeclaration,
) -> Result<(), ToolError> {
    if let Err(e) = crate::persist::write_team_declaration(team_dir, declaration).await {
        tracing::error!(team_id = %team_id, error = %e, "failed to write team.json — aborting team creation");
        return Err(ToolError::Execution(anyhow::anyhow!(
            "failed to create team '{name}': could not write team.json: {e}"
        )));
    }
    append_event(
        team_dir,
        team_id,
        "team_created",
        crate::persist::TeamEvent::TeamCreated {
            team_id: team_id.to_string(),
        },
    )
    .await;
    Ok(())
}
