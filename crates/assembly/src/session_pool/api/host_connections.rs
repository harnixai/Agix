//! 宿主连接级的工具与 hook：`daemon.setHostTools` / `daemon.setHooks` 登记的条目、
//! 收下与拒绝的结果，以及 `session.respondHook` 的回答。

use serde::{Deserialize, Serialize};

/// One entry of `daemon.setHooks`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostHookOption {
    /// `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `UserPromptSubmit`,
    /// `Stop`.
    pub events: Vec<String>,
    /// Which tool calls the tool events ask about, as tool selector patterns
    /// (`*`, `Prefix*`, a full name, `mcp__<server>`); at most 64. Omitted:
    /// every one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// 1000..=30000.
    #[serde(default = "HostHookOption::default_timeout_ms")]
    pub timeout_ms: u64,
    /// `allow` (default) or `block`: what a call that times out or finds its
    /// connection gone amounts to.
    #[serde(default)]
    pub on_error: HostHookOnError,
}

impl HostHookOption {
    pub const MIN_TIMEOUT_MS: u64 = 1_000;
    pub const MAX_TIMEOUT_MS: u64 = 30_000;
    pub const MAX_TOOLS: usize = 64;

    fn default_timeout_ms() -> u64 {
        10_000
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HostHookOnError {
    #[default]
    Allow,
    Block,
}

/// An entry `daemon.setHostTools` or `daemon.setHooks` did not take, and why;
/// the others are taken all the same.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefusedEntry {
    pub index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostToolsSet {
    /// The names of the tools taken.
    pub host_tools: Vec<String>,
    pub refused: Vec<RefusedEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostHookSet {
    pub index: usize,
    pub events: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HooksSet {
    pub hooks: Vec<HostHookSet>,
    pub refused: Vec<RefusedEntry>,
}

/// What a host connection answers a `hook_call` with: what a hook response
/// may say but standing in for the user, rewriting the input, ending the
/// turn and asking to be run again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookAnswer {
    pub block: bool,
    pub message: Option<String>,
    pub additional_context: Option<String>,
}

impl HookAnswer {
    /// The hook response object the engine reads.
    pub fn response(&self) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        if self.block {
            out.insert("decision".into(), "block".into());
        }
        if let Some(message) = &self.message {
            out.insert("message".into(), message.clone().into());
        }
        if let Some(context) = &self.additional_context {
            out.insert("additional_context".into(), context.clone().into());
        }
        serde_json::Value::Object(out)
    }
}
