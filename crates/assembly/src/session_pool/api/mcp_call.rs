//! 宿主不经会话直接调一个 MCP 工具（`mcp.callTool`）与列出能调的工具（`mcp.tools`）：
//! 请求、结局、工具的描述，审计事件里的结局，以及这种调用的 hook 调用带的 `mcp_call` 标记。

use super::McpServerState;
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

/// Whose MCP connections a call or a listing uses.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpToolCallTarget {
    /// That session's: its servers, rules, trust and hooks.
    Session(String),
    /// The pair's, in no session; `project_root` `None` is this daemon's own
    /// project.
    Pair {
        project_root: Option<PathBuf>,
        scene: String,
    },
}

#[derive(Debug, Clone)]
pub struct McpToolCall {
    /// The server's key in settings, as `mcp.status` names it.
    pub server: String,
    /// The server's own name for the tool.
    pub tool: String,
    pub arguments: serde_json::Map<String, serde_json::Value>,
    pub target: McpToolCallTarget,
    /// At most [`MCP_CALL_TIME_LIMIT`]; `None` is [`MCP_CALL_DEFAULT_TIME`].
    pub timeout: Option<Duration>,
    /// What the caller names the call by; a call in no session carries it on
    /// the hook calls it puts to host connections.
    pub run_id: Option<String>,
}

impl McpToolCall {
    /// A call with no arguments and the default time.
    pub fn new(
        server: impl Into<String>,
        tool: impl Into<String>,
        target: McpToolCallTarget,
    ) -> Self {
        Self {
            server: server.into(),
            tool: tool.into(),
            arguments: serde_json::Map::new(),
            target,
            timeout: None,
            run_id: None,
        }
    }
}

pub const MCP_CALL_DEFAULT_TIME: Duration = Duration::from_millis(120_000);
pub const MCP_CALL_TIME_LIMIT: Duration = Duration::from_millis(600_000);

/// The call in no session a hook call comes from: what its frame carries
/// as `mcp_call`, its `session_id` being the call's own engine's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct McpCallOrigin {
    pub server: String,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// What a host's call of a tool came to. `ok` with `is_error` is the tool
/// itself reporting a failure.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct McpToolCallResult {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// The result's blocks: `text` and `image` as in a message, any other
    /// kind as the server sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<serde_json::Value>>,
    /// The text blocks joined with `\n`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The server's `structuredContent`, as it sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<McpToolCallError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct McpToolCallError {
    /// `refused`, `timeout`, `cancelled` or `failed`; more may come.
    pub code: String,
    pub message: String,
}

impl McpToolCallResult {
    /// How the call ended, as the audit event says it.
    pub fn outcome(&self) -> McpToolCallOutcome {
        match (&self.error, self.is_error) {
            (None, Some(true)) => McpToolCallOutcome::ToolError,
            (None, _) => McpToolCallOutcome::Ok,
            (Some(error), _) => McpToolCallOutcome::from_code(&error.code),
        }
    }
}

/// How a host's call of an MCP tool ended, for `mcp_tool_called`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpToolCallOutcome {
    Ok,
    ToolError,
    Refused,
    Timeout,
    Cancelled,
    Failed,
}

impl McpToolCallOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::ToolError => "tool_error",
            Self::Refused => "refused",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    fn from_code(code: &str) -> Self {
        match code {
            "refused" => Self::Refused,
            "timeout" => Self::Timeout,
            "cancelled" => Self::Cancelled,
            _ => Self::Failed,
        }
    }
}

/// The servers of a pair or a session and the tools each offers.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct McpToolList {
    pub servers: Vec<McpServerTools>,
}

#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct McpServerTools {
    pub name: String,
    pub state: McpServerState,
    /// Empty unless the server is connected (or was, and reconnects).
    pub tools: Vec<McpToolInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct McpToolInfo {
    /// The server's own name, what `mcp.callTool` takes as `tool`.
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    pub annotations: McpToolAnnotations,
    /// `mcp__<server>__<tool>`: the name permission rules and hooks match.
    pub tool_name: String,
}

/// The server's hints about a tool; one it says nothing about is absent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct McpToolAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world: Option<bool>,
}
