//! 会话池的 MCP 部分在没编进 `mcp` 模块时的桩：签名与 `live.rs` 相同，签名里不出现
//! mcp crate 的类型。server 照常调这些方法；`mcp.*` 在到达之前已经按方法表的
//! `compiled` 答了 `METHOD_NOT_FOUND`，所以对外的方法只回一个内部错误兜底。

use crate::session_pool::api;
use crate::session_pool::api::PoolError;
use crate::session_pool::{ProjectSceneKey, SessionPool};
use std::path::Path;
use std::sync::Arc;

/// Whether this build has the `mcp` module.
pub const COMPILED: bool = false;

/// What a host decides about MCP. Nothing to decide in a build without it.
#[derive(Default)]
pub struct McpOptions {}

pub(crate) struct McpSetup;

impl McpOptions {
    pub(crate) fn resolve(self, _config: &crate::config::DaemonConfig) -> anyhow::Result<McpSetup> {
        Ok(McpSetup)
    }
}

pub(in crate::session_pool) struct McpHub;

impl McpHub {
    pub(in crate::session_pool) fn new(
        _setup: Option<McpSetup>,
        _paths: &base::paths::PathSettings,
    ) -> Self {
        Self
    }

    pub(in crate::session_pool) fn health_check(
        &self,
    ) -> Option<Arc<dyn base::interface::health::HealthCheck>> {
        None
    }
}

fn not_compiled() -> PoolError {
    PoolError::Internal("this build has no MCP support".to_string())
}

impl SessionPool {
    pub async fn mcp_status(
        &self,
        _project_root: Option<&Path>,
        _scene_id: &str,
    ) -> Result<api::McpStatusList, PoolError> {
        Err(not_compiled())
    }

    pub fn connect_mcp_servers_in_background(self: &Arc<Self>) {}

    pub async fn add_mcp_server(
        &self,
        _project_root: Option<&Path>,
        _scene_id: &str,
        _tier: api::SettingsTier,
        _name: &str,
        _config: serde_json::Value,
    ) -> Result<api::McpServerWritten, PoolError> {
        Err(not_compiled())
    }

    pub async fn update_mcp_server(
        &self,
        _project_root: Option<&Path>,
        _scene_id: &str,
        _tier: api::SettingsTier,
        _name: &str,
        _config: serde_json::Value,
    ) -> Result<api::McpServerWritten, PoolError> {
        Err(not_compiled())
    }

    pub async fn remove_mcp_server(
        &self,
        _project_root: Option<&Path>,
        _scene_id: &str,
        _tier: api::SettingsTier,
        _name: &str,
    ) -> Result<api::McpServerWritten, PoolError> {
        Err(not_compiled())
    }

    pub async fn set_mcp_enabled(
        &self,
        _project_root: Option<&Path>,
        _scene_id: &str,
        _tier: api::SettingsTier,
        _name: &str,
        _enabled: bool,
    ) -> Result<api::McpServerWritten, PoolError> {
        Err(not_compiled())
    }

    pub async fn mcp_reconnect(
        &self,
        _project_root: Option<&Path>,
        _scene_id: &str,
        _name: &str,
    ) -> Result<api::McpServerReconnected, PoolError> {
        Err(not_compiled())
    }

    pub async fn mcp_auth_start(
        self: &Arc<Self>,
        _project_root: Option<&Path>,
        _scene_id: &str,
        _name: &str,
        _redirect: api::McpRedirect,
        _scopes: Option<Vec<String>>,
    ) -> Result<api::McpAuthStarted, PoolError> {
        Err(not_compiled())
    }

    pub async fn mcp_auth_complete(
        &self,
        _flow_id: &str,
        _callback_url: &str,
    ) -> Result<api::McpAuthDone, PoolError> {
        Err(not_compiled())
    }

    pub async fn mcp_auth_cancel(&self, _flow_id: &str) -> Result<api::McpServerRef, PoolError> {
        Err(not_compiled())
    }

    pub async fn mcp_auth_logout(
        &self,
        _project_root: Option<&Path>,
        _scene_id: &str,
        _name: &str,
    ) -> Result<api::McpServerRef, PoolError> {
        Err(not_compiled())
    }

    pub async fn mcp_auth_set_identity(
        &self,
        _identity: api::McpIdentity,
    ) -> Result<api::McpReconnected, PoolError> {
        Err(not_compiled())
    }

    pub(in crate::session_pool) async fn forget_mcp_built_from(
        &self,
        _tier: &crate::watch_hub::Tier,
        _keep: Option<&ProjectSceneKey>,
    ) {
    }

    pub(in crate::session_pool) async fn reconcile_mcp_servers(
        &self,
        _project_root: Option<&Path>,
        _scene_id: &str,
        _settings: &settings::LoadedSettings,
        _connect_pair: bool,
    ) -> (Vec<String>, Vec<String>, Vec<String>) {
        (Vec::new(), Vec::new(), Vec::new())
    }

    pub(in crate::session_pool) async fn reconcile_written_mcp_server(
        &self,
        _changed: Option<&crate::watch_hub::Tier>,
        _key: &ProjectSceneKey,
        _name: &str,
        _connect_pair: bool,
    ) -> (Vec<String>, Vec<String>, Vec<String>) {
        (Vec::new(), Vec::new(), Vec::new())
    }

    pub(in crate::session_pool) async fn drop_unconfigured_mcp_servers(&self) {}

    pub(in crate::session_pool) async fn mcp_session_module(
        &self,
        _project_root: Option<&Path>,
        _scene_id: &str,
        _plugins: &crate::plugins::SessionPlugins,
    ) -> (usize, Option<Arc<dyn runtime::module::Module>>) {
        (0, None)
    }
}
