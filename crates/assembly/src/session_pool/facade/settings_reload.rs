//! 设置的读取与热替换：`settings`、`reload_settings`，以及"一层设置变了"之后的全部动作
//! ——自家那一对重读、失效从那一层合出来的绑定、按需重连 MCP、向宿主报路由。监视器、
//! `config.reload` 和写设置的核心都走这里。

use crate::session_pool::parts::bindings::tier_covers;
use crate::session_pool::*;
use crate::watch_hub::Tier;

impl SessionPool {
    /// The currently-active settings snapshot (for config-inspection RPCs).
    pub async fn settings(&self) -> Arc<LoadedSettings> {
        self.bindings.pool_settings().await
    }

    /// Re-read whatever is currently on disk across all three settings.json
    /// tiers and apply it — the `config.reload` RPC's backing implementation,
    /// used when a human (or another process) hand-edited a settings.json
    /// file directly. Routing is re-resolved, the pair's MCP servers
    /// reconnected, and every binding marked stale so already-running
    /// sessions recreate themselves lazily — see `run_turn`.
    pub async fn reload_settings(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> api::SettingsReloaded {
        self.reload_settings_from(project_root, scene_id, None)
            .await
    }

    /// [`Self::settings_tier_changed`] of `changed` (every tier when `None`)
    /// for the pair, every MCP server reconnected.
    pub(in crate::session_pool) async fn reload_settings_from(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        changed: Option<&Tier>,
    ) -> api::SettingsReloaded {
        let project_root = project_root.unwrap_or(self.cwd());
        let (reloaded, report) = self
            .settings_tier_changed(
                changed,
                (project_root, scene_id),
                McpEffect::Full { connect_pair: true },
            )
            .await;
        settings_reloaded(&reloaded.values, report)
    }

    /// Everything a change to `changed` (every tier when `None`: an explicit
    /// reload does not know which one moved) means, then the pair's settings
    /// and routing as they are now. The pair's MCP connections follow `mcp`.
    ///
    /// Never fails outright — misconfiguration degrades to "keep the
    /// previous router, report why in the returned status".
    pub(in crate::session_pool) async fn settings_tier_changed(
        &self,
        changed: Option<&Tier>,
        (project_root, scene_id): (&Path, &str),
        mcp: McpEffect,
    ) -> (Arc<LoadedSettings>, api::RoutingStatus) {
        let key = (Some(project_root.to_path_buf()), scene_id.to_string());
        self.tier_changed(changed, Some(&key), matches!(mcp, McpEffect::Full { .. }))
            .await;
        let binding = self.binding_for(Some(project_root), scene_id).await;
        let routing = &binding.routing;
        let (mcp_connected, mcp_failed, mcp_connecting) = match &mcp {
            McpEffect::Full { connect_pair } => {
                self.reconcile_mcp_servers(
                    Some(project_root),
                    scene_id,
                    &binding.settings,
                    *connect_pair,
                )
                .await
            }
            McpEffect::Server { name, connect_pair } => {
                self.reconcile_written_mcp_server(changed, &key, name, *connect_pair)
                    .await
            }
            McpEffect::None => Default::default(),
        };
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

    /// The pool's side of `changed` having changed: this daemon's own pair
    /// re-read when the tier is under it — it keeps a separate, hot-swappable
    /// copy that everything with no session to ask about reads — and the
    /// bindings merged from it dropped, so their running sessions recreate
    /// themselves before their next turn. With `forget_mcp`, the MCP
    /// connections built from it go too, except `keep`'s, which the caller
    /// reconnects itself.
    pub(in crate::session_pool) async fn tier_changed(
        &self,
        changed: Option<&Tier>,
        keep: Option<&ProjectSceneKey>,
        forget_mcp: bool,
    ) {
        let own = (
            Some(self.cwd().to_path_buf()),
            self.bindings.startup_scene().await,
        );
        if changed.is_none_or(|tier| tier_covers(tier, &own)) {
            self.bindings.reload_own(self.scenes.default_id()).await;
        }
        self.invalidate_bindings(changed).await;
        if let (true, Some(tier)) = (forget_mcp, changed) {
            self.forget_mcp_built_from(tier, keep).await;
        }
    }
}

pub(in crate::session_pool) fn settings_reloaded(
    settings: &Settings,
    routing: api::RoutingStatus,
) -> api::SettingsReloaded {
    api::SettingsReloaded {
        providers: settings.providers.iter().map(|p| p.id.clone()).collect(),
        order: settings
            .routing()
            .map(|order| order.iter().map(|(p, _)| p.id.clone()).collect())
            .unwrap_or_else(|_| settings.model.order.clone()),
        routing,
    }
}
