//! `mcp.*` 的 params 与线格式。不依赖 mcp crate，没编进 mcp 模块的构建里 schema
//! 也一样。

use super::ConfigTarget;
use crate::session_pool::api::SettingsTier;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpPutServerParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    pub name: String,
    /// An `McpServerConfig`, tagged by `type`; the pool validates it.
    pub config: serde_json::Value,
    #[serde(default)]
    pub tier: SettingsTier,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpRemoveServerParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    pub name: String,
    #[serde(default)]
    pub tier: SettingsTier,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpSetEnabledParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    pub name: String,
    #[serde(default)]
    pub tier: SettingsTier,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpReconnectParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpRedirectKind {
    #[default]
    Loopback,
    External,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpAuthStartParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    pub server: String,
    #[serde(default)]
    pub redirect: McpRedirectKind,
    /// Only for `"redirect": "external"`, and required there.
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpAuthCompleteParams {
    pub flow_id: String,
    pub callback_url: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpAuthCancelParams {
    pub flow_id: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpAuthLogoutParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    pub server: String,
}

/// `scene` / `project_root` name a pair as for `mcp.status`; `session_id`
/// names a session instead, never both.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct McpToolsParams {
    /// Only this server, which must be configured.
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub pair: McpPairParams,
}

/// A pair, as `mcp.status` names one: omitted is this daemon's own scene and
/// project.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct McpPairParams {
    #[serde(default)]
    pub scene: Option<String>,
    /// A project the daemon serves.
    #[serde(default, deserialize_with = "a_project")]
    #[schemars(with = "String")]
    pub project_root: Option<PathBuf>,
}

fn a_project<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<PathBuf>, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(path) => Ok(Some(PathBuf::from(path))),
        _ => Err(serde::de::Error::custom(
            "project_root is a path; leave it out for this daemon's own project",
        )),
    }
}

/// One tool of a server of a pair or of a session, as for `mcp.tools`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpCallToolParams {
    /// The server's key in settings, as `mcp.status` names it.
    pub server: String,
    /// The server's own name for the tool, not `mcp__…`.
    pub tool: String,
    /// Absent and `null` both mean `{}`.
    #[serde(default)]
    pub arguments: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub pair: McpPairParams,
    /// Default 120000, at most 600000.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// What `mcp.cancel` names this call by, on this connection.
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpCancelParams {
    pub run_id: String,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct McpCancelled {
    /// `false`: no call of this connection is running under `run_id`.
    pub cancelled: bool,
}

/// Exactly one of the two.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpSetIdentityParams {
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}
