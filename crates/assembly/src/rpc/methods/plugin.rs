//! `plugin.*` 的 params 与线格式。

use crate::session_pool::api::{PluginFunctionInfo, PluginInfo, PluginScope};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginList {
    pub plugins: Vec<PluginInfo>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct PluginInstallParams {
    /// A cache directory name: 1 to 128 of ASCII letters, digits, `.`, `_`,
    /// `-`, `+`, not starting with `.`.
    pub name: String,
    pub version: String,
    /// `file://` or `https://`; the daemon fetches it.
    pub download_url: String,
    /// SHA-256 of the archive, hex. Required for `http(s)://`.
    #[serde(default)]
    pub checksum: Option<String>,
    #[serde(default)]
    pub scope: PluginScope,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct PluginUninstallParams {
    pub name: String,
    /// All versions when omitted.
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub scope: PluginScope,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct PluginToggleParams {
    pub name: String,
    #[serde(default)]
    pub scope: PluginScope,
}

/// Either names a place that must exist; neither lists them all the same.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct PluginFunctionsParams {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PluginFunctionList {
    pub functions: Vec<PluginFunctionInfo>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct PluginCallParams {
    /// The call name, `<plugin>.<name>`.
    pub function: String,
    #[serde(default)]
    pub input: serde_json::Value,
    /// Run in this session's environment; without it `cwd` is required.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The directory a call without a session runs in.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Let the function write inside its `writes`.
    #[serde(default)]
    pub writes: bool,
    /// Held after the session's or the scene's rules, for this call only.
    #[serde(default)]
    pub permission_rules: Vec<base::permission::PermissionRule>,
    /// At most 120000.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// What `plugin.cancel` names this call by, on this connection.
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct PluginCancelParams {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PluginCancelled {
    /// `false`: no call of this connection is running under `run_id`.
    pub cancelled: bool,
}
