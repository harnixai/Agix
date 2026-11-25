//! 宿主调用插件函数（`[[function]]`）的请求与结局：调哪个函数、在哪个会话或目录下、
//! 让不让写，以及返回值或失败原因、函数记的日志；宿主不经会话的调用的来源标记。

use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

/// Where a call runs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginCallTarget {
    /// In that session's environment, beside any turn it is running.
    Session(String),
    /// In this directory, with the scene's tools and rules, in no session.
    Project(PathBuf),
}

#[derive(Debug, Clone)]
pub struct PluginCall {
    /// The call name, `<plugin>.<name>`.
    pub function: String,
    pub input: serde_json::Value,
    pub target: PluginCallTarget,
    /// Whether the function may write inside its `writes`; otherwise it
    /// writes nothing.
    pub writes: bool,
    /// Held after the session's or the scene's rules, for this call only.
    pub permission_rules: Vec<base::permission::PermissionRule>,
    /// At most a plugin command's limit, which `None` is.
    pub timeout: Option<Duration>,
    /// What the caller names the call by; a call in no session carries it on
    /// the hook calls it puts to host connections.
    pub run_id: Option<String>,
}

impl PluginCall {
    /// A read-only call with the scene's or session's rules alone and the
    /// longest time.
    pub fn new(
        function: impl Into<String>,
        input: serde_json::Value,
        target: PluginCallTarget,
    ) -> Self {
        Self {
            function: function.into(),
            input,
            target,
            writes: false,
            permission_rules: Vec::new(),
            timeout: None,
            run_id: None,
        }
    }
}

/// The call in no session a hook call comes from: what its frame carries
/// as `plugin_call`, its `session_id` being the call's own engine's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct PluginCallOrigin {
    pub function: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// The host's call in no session a hook call comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostCallOrigin {
    /// `plugin.call`; the frame carries it as `plugin_call`.
    Plugin(PluginCallOrigin),
    /// `mcp.callTool`; the frame carries it as `mcp_call`.
    Mcp(super::McpCallOrigin),
}

#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct PluginCallResult {
    pub ok: bool,
    /// What the function returned, when `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginCallError>,
    /// What the function logged, oldest first; the start is dropped past the
    /// log's limit.
    pub logs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct PluginCallError {
    /// `invalid_input`, `refused`, `answered_by_middleware`, `threw`,
    /// `timeout`, `cancelled`, `stopped` or `not_started`.
    pub code: String,
    pub message: String,
}

/// A function the host can call.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct PluginFunctionInfo {
    /// The call name, `<plugin>.<name>`.
    pub name: String,
    pub plugin: String,
    pub plugin_version: String,
    pub description: String,
    /// Globs relative to the project that a writing call may write.
    pub writes: Vec<String>,
    pub input_schema: Option<serde_json::Value>,
}
