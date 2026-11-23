//! 会话生命周期操作的结果：建、查、删、分叉、恢复、改名、换模型、换提示词。

use super::{SessionKindName, TrustState};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCreated {
    pub session_id: String,
    pub scene: String,
    pub session_kind: SessionKindName,
    pub project_root: Option<String>,
    pub project_trust: TrustState,
    /// What decided `project_trust`; `null` with no project tier.
    pub trust_source: Option<settings::trust::TrustSource>,
    pub created_at: String,
    /// `true`: the `session_id` asked for was already a session, and this is
    /// it; nothing was created.
    pub existing: bool,
    /// The limits in `options` the settings hold lower than asked: the
    /// session runs on `applied`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clamped_options: Vec<super::ClampedOption>,
}

/// What a session already under an id is, for the `SESSION_EXISTS` that
/// refuses to create another under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExistingSession {
    pub scene: Option<String>,
    pub project_root: Option<String>,
    pub session_kind: SessionKindName,
    /// The `options` fields asked for that are not what the session was
    /// created with; empty when the scene, project or kind is what differs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options_differ: Vec<String>,
}

/// `session.get`: the session's `session.list` entry plus what only the live
/// pool knows.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub info: super::super::SessionInfo,
    pub scene_active: bool,
    pub turn_state: TurnState,
    pub current_turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<base::event::ContextUsage>,
    /// What the session has spent since it was created, as of now: its own
    /// calls, its delegates' and those made on their side. Absent for a
    /// session not in memory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<base::interface::budget_policy::Spend>,
    /// Shaped like `daemon.doctor`'s `health`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<serde_json::Value>,
    /// Absent when the session bound no scripts, which is a different answer
    /// from scripts that never ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scripts: Option<ScriptReport>,
    /// The host's skill directories for the session, in order; absent when
    /// it has none. A session out of memory reports only their paths.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_dirs: Option<Vec<SkillDirReport>>,
    /// What the session's next model call carries; absent for a sidechain,
    /// and for a session whose scene is not known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<runtime::agent::ReasoningReport>,
}

/// What one of the host's skill directories gives a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillDirReport {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<SkillDirState>,
    /// The skills this directory has the name of in the session, sorted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// Its skills another source holds in the session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadowed: Option<Vec<ShadowedSkill>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SkillDirState {
    Ok,
    /// Not there; read once it is.
    Missing,
    /// Recorded, but left out when the session was brought back: it no
    /// longer passes the checks `options.skill_dirs` is held to.
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ShadowedSkill {
    pub skill: String,
    pub by: super::SkillOrigin,
    /// The `SKILL.md` that holds the name.
    pub path: String,
}

/// What a session in memory is doing; a session not in memory is idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TurnState {
    Idle,
    Running,
    /// Being rebuilt in place on its next engine.
    Rebuilding,
    /// Held by an operation other than a turn — a look at or a change to
    /// its context.
    Operating,
}

/// What a session's scripts have done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScriptReport {
    pub calls: usize,
    pub applied: usize,
    pub no_change: usize,
    pub failed: usize,
    pub refused: usize,
    /// Non-zero means `recent` is a tail of a longer story.
    pub dropped: usize,
    /// Newest first.
    pub recent: Vec<ScriptCall>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScriptCall {
    pub point: String,
    pub script: String,
    pub entry: String,
    pub turn: u32,
    pub outcome: ScriptCallOutcome,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ScriptCallOutcome {
    Applied,
    NoChange,
    Failed,
    Refused,
}

/// `session.delete`, or with `deleted: false` what a dry run would delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeleteReport {
    pub session_id: String,
    pub deleted: bool,
    pub sidechains_deleted: usize,
    pub sidechain_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForkReport {
    /// The new session.
    pub session_id: String,
    pub parent_session_id: String,
    pub forked_at_message: usize,
    pub message_count: usize,
    /// Log entries copied.
    pub entry_count: usize,
    pub source_message_count: usize,
    /// Always `false`: a fork is written to disk, not brought into memory.
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResumeReport {
    pub session_id: String,
    pub status: ResumeStatus,
    pub active: bool,
    pub message_count: usize,
    pub entry_count: usize,
    pub name: Option<String>,
    pub created_at: String,
    pub last_active: String,
    pub scene: String,
    pub project_trust: TrustState,
    /// What decided `project_trust`; `null` with no project tier.
    pub trust_source: Option<settings::trust::TrustSource>,
    pub permission: PermissionReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResumeStatus {
    AlreadyActive,
    Resumed,
    Created,
}

impl ResumeStatus {
    /// A session brought into memory: from its log, or new under its id.
    pub(crate) fn loaded(had_history: bool) -> Self {
        if had_history {
            Self::Resumed
        } else {
            Self::Created
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionReport {
    pub mode: base::permission::PermissionMode,
    /// `false`: no `prompt_type:"permission"` frame will ever be raised.
    pub prompts: bool,
    pub prompt_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Renamed {
    pub session_id: String,
    pub name: Option<String>,
}

/// When a change to a session's model or prompt takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Applies {
    NextTurn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelChoice {
    pub session_id: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// The session's own; `None` follows the settings.
    pub effort: Option<base::interface::model::Effort>,
    pub thinking: Option<base::interface::model::ThinkingMode>,
    pub applies: Applies,
}

/// A change to a session's own effort and thinking mode: `None` leaves a
/// field as it is, `Some(None)` takes it back to the settings'.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReasoningChange {
    pub effort: Option<Option<base::interface::model::Effort>>,
    pub thinking: Option<Option<base::interface::model::ThinkingMode>>,
}

impl ReasoningChange {
    pub fn is_empty(&self) -> bool {
        self.effort.is_none() && self.thinking.is_none()
    }
}

/// What `session.setPermissionMode` left the session in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionModeChoice {
    pub session_id: String,
    /// The mode the session is in now.
    pub mode: base::permission::PermissionMode,
    pub requested: base::permission::PermissionMode,
    /// The settings held the session to a narrower mode than `requested`.
    pub clamped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PromptChoice {
    pub session_id: String,
    pub append: Option<String>,
    #[serde(rename = "override")]
    pub override_text: Option<String>,
    pub applies: Applies,
}
