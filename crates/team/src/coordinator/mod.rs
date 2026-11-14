//! Team coordinator: runs a team's stages one after another, each stage's
//! members side by side as delegates of the lead's session, and keeps the
//! team's record on disk (`team.json`, `state.json`, `events.jsonl`).
//!
//! `run` walks the stages, `stage` delegates one stage's members under the
//! concurrency limit, `judge` aggregates their outputs and `record` shapes
//! what goes to disk.

mod judge;
mod record;
mod run;
mod stage;

use async_trait::async_trait;
use base::context::EngineConfig;
use base::error::ToolError;
use base::interface::delegate::{Delegator, TeamSeat};
use base::interface::model::Model;
use base::tool::ToolContext;
use base::tool::ToolResult;
use std::sync::Arc;

use crate::tool::TeamStage;

pub use judge::aggregate;

/// The lead session's event channel, where a team's `TeamProgress` goes.
pub type TeamEventSender = tokio::sync::mpsc::UnboundedSender<base::event::AgentEvent>;

#[async_trait]
pub trait Coordinator: Send + Sync {
    async fn orchestrate(&self, request: OrchestrateRequest) -> Result<ToolResult, ToolError>;

    /// Clean up a team's resources. Default implementation is a no-op;
    /// override for actual resource cleanup (task lists, scratchpad, etc.).
    async fn cleanup_team(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }
}

pub struct OrchestrateRequest {
    pub model: Arc<dyn Model>,
    pub config: Arc<EngineConfig>,
    pub stages: Vec<TeamStage>,
    pub name: String,
    pub scratchpad: Option<String>,
    pub ctx: ToolContext,
    /// The permission grant every agent in this call runs under — "one
    /// team, one authorization" (see `TeamCreateInput::permission_mode`'s
    /// doc comment). `None` falls back to `PermissionMode::Plan` (read-only
    /// tools only) at the spawn site — a safe default that doesn't require
    /// a caller to think about permissions to get *something* better than
    /// unrestricted access, but is never silently more permissive than
    /// that without an explicit choice.
    pub permission_mode: Option<base::permission::PermissionMode>,
}

/// Lifecycle states for team members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeammateLifecycle {
    /// Agent registered but not yet started.
    Idle,
    /// Agent actively executing.
    Active,
    /// Agent completed execution (success or failure).
    Completed,
    /// Agent shut down (cleanup complete).
    Shutdown,
    /// Agent stopped before it finished.
    Cancelled,
}

/// How one stage member's run ended, as the team counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberEnd {
    Done,
    Failed,
    Cancelled,
}

const DEFAULT_STAGE_CONCURRENCY: usize = 6;

/// Default coordinator implementation: its members are delegated through a
/// [`Delegator`] from `base`, which `agents` implements — `team` sits below
/// it and knows only the trait.
pub struct DefaultCoordinator {
    delegator: Option<Arc<dyn Delegator>>,
    registry: Option<Arc<crate::registry::TeamRegistry>>,
    events: Option<TeamEventSender>,
    telemetry: Option<telemetry::TelemetryHandle>,
    /// Stamped into `team.json`; `"unknown"` when not given. `team` does not
    /// depend on the `scene` crate, so whoever builds the coordinator says.
    scene_id: Option<String>,
    stage_concurrency: usize,
}

impl DefaultCoordinator {
    /// A coordinator that cannot start members: every stage reports them
    /// failed.
    pub fn new() -> Self {
        Self {
            delegator: None,
            registry: None,
            events: None,
            telemetry: None,
            scene_id: None,
            stage_concurrency: DEFAULT_STAGE_CONCURRENCY,
        }
    }

    pub fn with_stage_concurrency(mut self, stage_concurrency: usize) -> Self {
        self.stage_concurrency = stage_concurrency;
        self
    }

    pub fn with_delegator(delegator: Arc<dyn Delegator>) -> Self {
        Self {
            delegator: Some(delegator),
            ..Self::new()
        }
    }

    pub fn with_scene_id(mut self, scene_id: String) -> Self {
        self.scene_id = Some(scene_id);
        self
    }

    /// Attach the shared team registry so `orchestrate()` records what it
    /// created and `cleanup_team()` can find it again — see
    /// `crate::registry`'s module doc for why this can't just live inside
    /// `DefaultCoordinator` itself (`TeamCreate`/`TeamList`/`TeamDelete` each
    /// wrap their own separate coordinator instance).
    pub fn with_registry(mut self, registry: Arc<crate::registry::TeamRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Where `TeamProgress` goes as stages start and finish, so a team run
    /// is not one long silence followed by an enormous tool result.
    pub fn with_events(mut self, events: TeamEventSender) -> Self {
        self.events = Some(events);
        self
    }

    /// Where the team's stage and resume telemetry goes. Its members' own
    /// `agent_spawned`/`agent_completed` are recorded by the delegator.
    pub fn with_telemetry(mut self, handle: telemetry::TelemetryHandle) -> Self {
        self.telemetry = Some(handle);
        self
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_progress(
        &self,
        team: &str,
        team_id: &str,
        stage: &str,
        stage_index: usize,
        stage_count: usize,
        status: base::event::TeamStageStatus,
        members: Vec<String>,
        failed: Vec<String>,
    ) {
        let Some(tx) = &self.events else {
            return;
        };
        let _ = tx.send(base::event::AgentEvent::TeamProgress {
            team: team.to_string(),
            team_id: team_id.to_string(),
            stage: stage.to_string(),
            stage_index,
            stage_count,
            status,
            members,
            failed,
        });
    }
}

impl Default for DefaultCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Coordinator for DefaultCoordinator {
    async fn orchestrate(&self, req: OrchestrateRequest) -> Result<ToolResult, ToolError> {
        run::orchestrate(self, req).await
    }

    /// Removes the team from the registry and disk and stops its members; no
    /// registry wired is a no-op success, an unknown name is an error.
    async fn cleanup_team(&self, name: &str) -> Result<(), String> {
        let Some(registry) = &self.registry else {
            return Ok(());
        };
        let Some(info) = registry.remove(name) else {
            return Err(format!(
                "no such team: '{name}' — nothing was created under this name \
                 (or it was already deleted). Use TeamList to see active teams."
            ));
        };
        if let Some(delegator) = &self.delegator {
            let mut members: Vec<String> = info.members.iter().map(|m| m.label.clone()).collect();
            members.extend(
                delegator
                    .live_members()
                    .into_iter()
                    .filter(|m| m.seat.team == name)
                    .map(|m| m.seat.member),
            );
            members.sort();
            members.dedup();
            for member in members {
                delegator
                    .stop_member(&TeamSeat {
                        team: name.to_string(),
                        member,
                    })
                    .await;
            }
        }
        if !info.team_dir.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::remove_dir_all(&info.team_dir).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        team = %name,
                        path = %info.team_dir.display(),
                        error = %e,
                        "team cleanup: failed to remove team directory"
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
