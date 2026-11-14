//! Hook events.

use serde::Serialize;

/// One run of an event's hooks: how long the model's step waited on them,
/// and whether they stopped something.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HookRunPayload {
    /// The hook event's name, e.g. `PreToolUse`.
    pub event: String,
    pub hook_count: usize,
    pub duration_ms: u64,
    pub blocked: bool,
    /// Hooks that failed, timed out or were stopped.
    pub errors: usize,
    /// The tool call the run was about, for the tool events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
}
