//! `session.rewind` 与 `session.checkpoints` 的领域类型：回滚的请求与回执、回滚菜单。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RewindScope {
    Files,
    Conversation,
    Both,
}

impl From<RewindScope> for history::checkpoint::RewindScope {
    fn from(scope: RewindScope) -> Self {
        match scope {
            RewindScope::Files => Self::Files,
            RewindScope::Conversation => Self::Conversation,
            RewindScope::Both => Self::Both,
        }
    }
}

/// Where a rewind goes back to: exactly one of the three.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RewindCut {
    /// Back to just after this turn ended.
    #[serde(default)]
    pub to_turn: Option<String>,
    /// Back to just before this tool call — or a program's calls under it —
    /// wrote anything. Files only.
    #[serde(default)]
    pub to_tool_use: Option<String>,
    /// Back to before the first turn.
    #[serde(default)]
    pub to_start: bool,
}

impl RewindCut {
    pub fn target(&self) -> Result<history::checkpoint::RewindTarget, String> {
        use history::checkpoint::RewindTarget;
        match (&self.to_turn, &self.to_tool_use, self.to_start) {
            (Some(turn), None, false) => Ok(RewindTarget::ToTurn(turn.clone())),
            (None, Some(id), false) => Ok(RewindTarget::ToToolUse(id.clone())),
            (None, None, true) => Ok(RewindTarget::ToStart(true)),
            _ => Err("give exactly one of to_turn, to_tool_use and to_start: true".into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RewindRequest {
    pub session_id: String,
    pub cut: RewindCut,
    pub scope: RewindScope,
    pub force: bool,
    pub allow_partial: bool,
    pub dry_run: bool,
    /// Fork the whole session first, so the rewound part survives there.
    pub fork_first: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct RewindOutcome {
    pub session_id: String,
    pub rewind_id: String,
    pub dry_run: bool,
    pub scope: RewindScope,
    pub cut: RewindCut,
    /// `null` when the scope leaves the conversation alone.
    pub conversation: Option<runtime::agent::RewoundConversation>,
    /// `null` when the scope leaves the files alone.
    pub files: Option<runtime::agent::RewoundFiles>,
    pub fork_session_id: Option<String>,
}

/// The rewind menu: the turns a host can go back to and what each wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CheckpointList {
    pub session_id: String,
    pub turns: Vec<CheckpointTurn>,
    /// Turns before it cannot have their files restored; `null` while every
    /// one can.
    pub pruned_before_turn: Option<String>,
    /// A rewind that began and never finished.
    pub interrupted_rewind: Option<InterruptedRewind>,
    /// Whether this session keeps file checkpoints at all.
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CheckpointTurn {
    pub turn_id: String,
    pub started_at: String,
    /// The start of the message the turn began with.
    pub prompt: String,
    pub files: Vec<CheckpointFile>,
    pub restorable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CheckpointFile {
    pub path: String,
    pub tool: String,
    /// The delegate session that wrote it, when one did.
    pub agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct InterruptedRewind {
    pub rewind_id: String,
    pub ts: String,
}

/// The `rewound` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rewound {
    pub rewind_id: String,
    pub scope: RewindScope,
    pub cut: RewindCut,
    /// How many messages the session has now, as `session.history`'s
    /// `total` counts them.
    pub seq: usize,
    pub files_restored: usize,
    pub files_skipped: usize,
}
