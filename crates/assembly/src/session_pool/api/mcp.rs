//! MCP 服务器状态与登录的领域类型。不依赖 mcp crate，没编进 mcp 模块的构建里
//! 也能用，方法表的 schema 才不随构建变。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpServerState {
    Connected,
    /// Connected once and the connection has since dropped; the next request
    /// reconnects.
    Disconnected,
    Connecting,
    /// The remote server wants an OAuth sign-in (`mcp.auth.start`).
    AuthRequired,
    Failed,
    /// The MCP policy refuses it, or the project declares it with something a
    /// project's own server may not use (a daemon secret, a local key, the
    /// enterprise identity).
    Denied,
    /// A server the project declares, in a project that is not trusted.
    Untrusted,
    /// Turned off by `mcp_enabled` (`mcp.setEnabled`).
    Disabled,
    /// Configured for a project and scene whose servers have not been
    /// connected yet; they connect with its first session.
    NotStarted,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpAuthState {
    #[default]
    None,
    Required,
    Authorizing,
    Authorized,
    Failed,
}

/// A server's authorization. Never carries a token.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpAuthStatus {
    pub state: McpAuthState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpServerStatus {
    pub name: String,
    pub transport: String,
    pub tool_count: usize,
    pub state: McpServerState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What the server's tool list was cut to keep to its limits
    /// (`max_tools`, `max_description_chars`, `max_schema_bytes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limited: Option<String>,
    pub auth: McpAuthStatus,
}

/// The MCP servers of one project and scene, as `mcp.status` reports them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpStatusList {
    /// The pair's servers have been connected; `false` lists what its settings
    /// configure, and nothing was started to find out.
    pub loaded: bool,
    pub servers: Vec<McpServerStatus>,
}

/// The one server `mcp.reconnect` connected again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpServerReconnected {
    pub server: McpServerStatus,
}

/// What `mcp.addServer` / `mcp.updateServer` / `mcp.removeServer` /
/// `mcp.setEnabled` wrote and what is connected now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpServerWritten {
    pub written_to: String,
    /// Set when `headers` or `env` went to the `settings.local.json` beside
    /// `written_to`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets_written_to: Option<String>,
    pub servers: Vec<McpServerStatus>,
    /// Set when the configuration written to `settings.json` carries a
    /// credential outside `headers` and `env`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Where a sign-in's browser comes back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpRedirect {
    /// A port on 127.0.0.1 the daemon listens on.
    Loopback,
    /// A URI the host owns; it hands the callback URL to `mcp.auth.complete`.
    External(String),
}

/// The user's enterprise identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpIdentity {
    IdToken(String),
    RefreshToken(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpAuthStarted {
    pub flow_id: String,
    pub authorize_url: String,
    pub redirect_uri: String,
    pub expires_in: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpAuthDone {
    pub server: String,
    pub scopes: Vec<String>,
}

/// A sign-in flow that was cancelled, or a server that was signed out of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpServerRef {
    pub server: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpReconnected {
    pub reconnected: Vec<String>,
}
