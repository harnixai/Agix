//! 设置的读取与热替换：`settings`、`reload_settings`，以及重载后向宿主报告。

use crate::session_pool::*;

impl SessionPool {
    /// The currently-active settings snapshot (for config-inspection RPCs).
    pub async fn settings(&self) -> Arc<LoadedSettings> {
        self.bindings.pool_settings().await
    }

    /// Re-read whatever is currently on disk across all three settings.json
    /// tiers and apply it — the `config.reload` RPC's backing implementation,
    /// used when a human (or another process) hand-edited a settings.json
    /// file directly instead of going through `config.setProvider`. Shares
    /// `apply_reloaded_settings()` with `set_provider()`, so both entry
    /// points converge on identical semantics (routing re-resolved,
    /// `task_router` rebuilt, bindings marked stale so already-running
    /// sessions recreate themselves lazily — see `run_turn`).
    pub async fn reload_settings(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> api::SettingsReloaded {
        self.reload_settings_from(project_root, scene_id, None)
            .await
    }

    pub(in crate::session_pool) async fn reload_settings_from(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        changed: Option<&crate::watch_hub::Tier>,
    ) -> api::SettingsReloaded {
        let (reloaded, report) = self
            .apply_reloaded_settings(project_root, scene_id, changed)
            .await;
        settings_reloaded(&reloaded.values, report)
    }

    /// Re-read the three-tier merged settings.json from disk, re-resolve
    /// `providers`/`task_models`, rebuild the pair's router from the result,
    /// swap this daemon's own settings when it is its own pair, and invalidate
    /// the bindings built from `changed` (every binding when `None`: an
    /// explicit reload does not know which tier moved) so their running
    /// sessions recreate themselves before their next turn (see `run_turn`).
    /// Shared by `set_provider()` (called after writing a patch) and
    /// `reload_settings()` (called directly, no patch — just "whatever's on
    /// disk right now").
    ///
    /// Never fails outright — misconfiguration degrades to "keep the
    /// previous router, report why in the returned status";
    /// `resolve_task_models` is advisory.
    pub(in crate::session_pool) async fn apply_reloaded_settings(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        changed: Option<&crate::watch_hub::Tier>,
    ) -> (Arc<LoadedSettings>, api::RoutingStatus) {
        let project_root = project_root.unwrap_or(self.cwd());
        // The daemon's own pair keeps a separate, hot-swappable copy —
        // everything with no session to ask about reads it. Every other pair
        // lives only in its binding, which the invalidation below rebuilds
        // from disk.
        if self
            .bindings
            .is_own_pair(Some(project_root), scene_id)
            .await
        {
            self.bindings.reload_own(self.scenes.default_id()).await;
        }

        // The cached bindings hold the settings that were just replaced, and
        // the skill/agent catalogs and router built from them. Dropping them
        // is what makes a reload reach the next session.
        self.invalidate_bindings(changed).await;

        let binding = self.binding_for(Some(project_root), scene_id).await;
        let routing = &binding.routing;
        let (mcp_connected, mcp_failed, mcp_connecting) = self
            .reconcile_mcp_servers(Some(project_root), scene_id, &binding.settings)
            .await;
        let status = api::RoutingStatus {
            ok: routing.error.is_none(),
            warnings: routing.warnings.clone(),
            error: routing.error.clone(),
            router_rebuilt: routing.router_rebuilt,
            router_error: routing.router_error.clone(),
            mcp_connected,
            mcp_failed,
            mcp_connecting,
        };

        (binding.settings.clone(), status)
    }
}

pub(in crate::session_pool) fn settings_reloaded(
    settings: &Settings,
    routing: api::RoutingStatus,
) -> api::SettingsReloaded {
    api::SettingsReloaded {
        providers: settings.providers.keys().cloned().collect(),
        default_provider: settings.default_provider.clone(),
        task_models: settings.task_models.keys().cloned().collect(),
        routing,
    }
}
