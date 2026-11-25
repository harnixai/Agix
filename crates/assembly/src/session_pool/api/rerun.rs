//! `session.rerunCall` 的领域类型：要重发录像里哪一次调用、发之前怎么改，以及答回的新应答与
//! 和录下那次的差异。结果形状是协议契约；`response` 与 `session.call` 的同形。

use super::{CallResponse, CallUsage};
use serde::{Deserialize, Serialize};

/// One recorded call to send again, changed first by `edits`, to `provider`
/// / `model` when given and the recorded call's own otherwise.
#[derive(Debug, Clone)]
pub struct RerunRequest {
    pub session_id: String,
    pub index: usize,
    pub generation: Option<u64>,
    pub edits: RerunEdits,
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// Positions are the recorded request's: every edit is resolved against it
/// before any is applied, so removing one block does not move another
/// edit's target.
#[derive(Debug, Clone, Default)]
pub struct RerunEdits {
    pub system: Vec<SystemEdit>,
    pub drop_messages: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct SystemEdit {
    pub block: BlockSelector,
    /// The block's new text; `None` removes the block.
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlockSelector {
    Index(usize),
    /// The block's name, as `session.call` gives it in `source`.
    Source(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RerunResult {
    pub session_id: String,
    pub index: usize,
    pub generation: u64,
    /// The provider that served the rerun and the model it was sent with;
    /// `None` when no provider took the call.
    pub provider: Option<String>,
    pub model: Option<String>,
    pub response: CallResponse,
    pub diff: RerunDiff,
}

/// The rerun's answer against the recorded one. Tool calls and the stop
/// reason compare exactly; text only as the same or not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RerunDiff {
    /// Text, tool calls and stop reason all as recorded.
    pub same: bool,
    pub text: TextDiff,
    pub tool_uses: ToolUsesDiff,
    pub stop_reason: StopReasonDiff,
    pub usage: UsageDiff,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TextDiff {
    pub same: bool,
    pub recorded: String,
    pub live: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolUsesDiff {
    pub same: bool,
    /// One per position in the longer of the two sequences of tool calls.
    pub steps: Vec<ToolStepDiff>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolStepDiff {
    pub status: ToolStepStatus,
    /// The tool called at this position; `None` on the side with fewer calls.
    pub recorded: Option<String>,
    pub live: Option<String>,
    /// Every argument key of either side; only for `args_differ`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<ArgDiffView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolStepStatus {
    Same,
    ArgsDiffer,
    NameDiffers,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ArgDiffView {
    pub key: String,
    pub same: bool,
    /// Absent on the side whose call has no such key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StopReasonDiff {
    pub same: bool,
    pub recorded: Option<String>,
    pub live: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UsageDiff {
    pub recorded: Option<CallUsage>,
    pub live: Option<CallUsage>,
}
