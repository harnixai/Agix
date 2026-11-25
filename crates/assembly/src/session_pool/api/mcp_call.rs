//! 宿主不经会话直接调一个 MCP 工具（`mcp.callTool`）与列出能调的工具（`mcp.tools`）：
//! 请求、结局、工具的描述，以及这种调用的 hook 调用带的 `mcp_call` 标记。

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

/// What a host's call of a tool came to: the server's answer, or why there
/// is none.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum McpToolCallResult {
    Answered(McpToolAnswer),
    NotAnswered(McpToolCallFailure),
}

/// The server's `CallToolResult`, as it sent it. `is_error` is the tool
/// itself reporting a failure.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct McpToolAnswer {
    /// Always `true`.
    #[schemars(extend("const" = true))]
    pub ok: bool,
    /// `isError`; `false` when the server left it out.
    pub is_error: bool,
    /// `content` verbatim, in the shape of the MCP spec revision the server
    /// speaks.
    pub content: Vec<serde_json::Value>,
    /// The `text` of the `type: "text"` blocks joined with `\n`; `""` when
    /// there are none.
    pub text: String,
    /// `structuredContent` verbatim; absent when the server gave none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct McpToolCallFailure {
    /// Always `false`.
    #[schemars(extend("const" = false))]
    pub ok: bool,
    pub error: McpToolCallError,
    /// The request was handed to the server, which may have acted on it: a
    /// tool that is not idempotent is not simply called again.
    pub reached_server: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct McpToolCallError {
    /// `refused`, `answered_by_middleware`, `timeout`, `cancelled` or
    /// `failed`; more may come.
    pub code: String,
    pub message: String,
}

impl McpToolCallResult {
    /// How the call ended, as the audit event says it: `ok`, `tool_error`,
    /// or the error's `code`.
    pub fn outcome(&self) -> &str {
        match self {
            Self::Answered(answer) if answer.is_error => "tool_error",
            Self::Answered(_) => "ok",
            Self::NotAnswered(failure) => &failure.error.code,
        }
    }

    pub fn reached_server(&self) -> bool {
        match self {
            Self::Answered(_) => true,
            Self::NotAnswered(failure) => failure.reached_server,
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
    #[schemars(with = "bool")]
    pub read_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "bool")]
    pub destructive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "bool")]
    pub idempotent: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "bool")]
    pub open_world: Option<bool>,
}
