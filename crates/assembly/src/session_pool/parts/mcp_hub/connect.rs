//! MCP 连接：准入、后台首连与迟到的连接、按 `(project, scene)` 共享的 manager、会话
//! 拿到的那一份，以及配置变化后整对的对账。

use super::*;
use crate::session_pool::parts::bindings::tier_covers;
use crate::session_pool::*;

impl SessionPool {
    /// This pool's MCP authorization state.
    pub fn mcp_auth(&self) -> Arc<mcp::oauth::McpAuth> {
        self.mcp.auth.clone()
    }

    /// Drop the cached connections whose `mcp_servers` came from `tier`, so the
    /// next session or rebuild on those keys connects with the changed config
    /// (a new `Authorization`, say) instead of reusing the old connection.
    /// `keep` is a key the caller reconnects itself.
    pub(in crate::session_pool) async fn forget_mcp_built_from(
        &self,
        tier: &crate::watch_hub::Tier,
        keep: Option<&ProjectSceneKey>,
    ) {
        self.mcp
            .cache
            .retain(|key| !tier_covers(tier, key) || keep == Some(key));
    }

    /// This project's centrally-connected `McpManager` — the MCP
    /// counterpart to `settings_for_project`, same `None`/`Some(root)` key
    /// convention. Connects lazily and caches the result on first use
    /// rather than eagerly for every project this pool has ever seen a
    /// session for: most daemons only ever serve their own default project,
    /// and eagerly connecting every project mentioned in a `session.create`
    /// call would pay for MCP servers nobody's session ever actually needs.
    /// The default project is the one exception — it's still connected
    /// eagerly at startup by `connect_mcp_servers_in_background`, so by the
    /// time this is first called for it the cache is typically already warm.
    ///
    /// Not deduplicated across projects with an identical `mcp_servers`
    /// config — two projects that happen to declare the same server each
    /// get their own connection.
    pub(in crate::session_pool) async fn mcp_for_project(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Arc<McpManager> {
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        if let Some(existing) = self.mcp.cache.current(&key) {
            return existing;
        }
        let settings = self.settings_for_project(project_root, scene_id).await;
        self.first_mcp_connect(key, &settings).await
    }

    /// What a session's engine gets of its pair's MCP servers: its own
    /// `McpManager` built from the pair's connected client handles (cheap
    /// `Arc` clones — not a reconnect), since a manager needs `&mut self` to
    /// refresh its tools, which a shared one cannot give it. `None` when
    /// nothing is connected or connecting, so the common "no MCP servers"
    /// case pays nothing. The count is the pair's connected servers.
    pub(in crate::session_pool) async fn mcp_session_module(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> (usize, Option<Arc<dyn runtime::module::Module>>) {
        let central = self.mcp_for_project(project_root, scene_id).await;
        let count = central.server_count();
        if count == 0 && central.pending().is_empty() {
            return (count, None);
        }
        let mut per_session = central.share();
        per_session.refresh_tools().await;
        (count, Some(Arc::new(mcp::McpModule::new(per_session))))
    }

    /// The MCP servers a `(project, scene)` with these settings connects:
    /// its `mcp_servers` and every active plugin's `[[mcp]]` declarations.
    pub(in crate::session_pool) fn configured_mcp_servers(
        &self,
        settings: &Settings,
    ) -> HashMap<String, serde_json::Value> {
        let mut servers = settings.mcp_servers.clone();
        servers.extend(self.plugins.mcp_servers());
        servers
    }

    /// The one gate every MCP server passes before this pool connects it.
    pub(in crate::session_pool) fn admit_mcp(
        &self,
        name: &str,
        cfg: &mcp::config::McpServerConfig,
    ) -> Result<(), String> {
        self.mcp.policy.admit(name, cfg).inspect_err(|reason| {
            tracing::warn!(server = %name, %reason, "MCP server not connected");
        })
    }

    /// A stdio server starts, and its relative command and paths are found,
    /// in the directory of whoever declared it: a plugin's package root, the
    /// project root for the project's own, the scene's or the global
    /// configuration directory otherwise. Never the daemon's working
    /// directory, which may be some other project.
    pub(in crate::session_pool) fn placed_mcp_server(
        &self,
        settings: &settings::LoadedSettings,
        name: &str,
        cfg: mcp::config::McpServerConfig,
    ) -> mcp::config::McpServerConfig {
        let home = self
            .plugins
            .mcp_server_dirs()
            .remove(name)
            .or_else(|| settings.mcp_server_dirs.get(name).cloned())
            .unwrap_or_else(|| self.bindings.paths().global_root());
        cfg.run_in(&home)
    }

    fn refuse_clashing_plugin_servers(&self, admitted: &mut AdmittedMcp) {
        for (key, kept, refused) in self.plugins.mcp_key_clashes() {
            let error = format!(
                "package `{refused}` declares an MCP server that would also be `{key}`, which \
                 package `{kept}` already has; only `{kept}`'s is started"
            );
            self.emit_event(api::DaemonEvent::McpConnectFailed {
                server: key.clone(),
                error: error.clone(),
            });
            admitted.invalid.push(mcp::manager::FailedServer {
                name: key,
                transport: "unknown",
                error,
            });
        }
    }

    /// Parse the servers `settings` configure and pass each through
    /// `admit_mcp`, a project's own after `confined_to_own_env`; one turned
    /// off (`mcp_enabled`) is left out without a word. Every other entry that
    /// will not be connected gets a `mcp_connect_failed` event here.
    pub(in crate::session_pool) fn admit_mcp_servers(
        &self,
        settings: &settings::LoadedSettings,
    ) -> AdmittedMcp {
        let mut admitted = AdmittedMcp::default();
        self.refuse_clashing_plugin_servers(&mut admitted);
        let judge = McpJudge::of(self, settings);
        for (name, v) in &self.configured_mcp_servers(&settings.values) {
            let refusal = match judge.admit(self, settings, name, v) {
                Admission::Admitted(cfg) => {
                    admitted.servers.insert(name.clone(), *cfg);
                    continue;
                }
                Admission::Disabled => {
                    admitted.disabled.push(name.clone());
                    continue;
                }
                Admission::Invalid(failed) => {
                    let error = failed.error.clone();
                    admitted.invalid.push(failed);
                    error
                }
                Admission::Denied(denied) => {
                    let reason = denied.reason.clone();
                    admitted.denied.push(denied);
                    reason
                }
            };
            self.emit_event(api::DaemonEvent::McpConnectFailed {
                server: name.clone(),
                error: refusal,
            });
            admitted.refused.push(name.clone());
        }
        admitted
    }

    /// What `admit_mcp_servers` decides about the one server `name`; `None`
    /// when `settings` configure no such server. Nothing is announced.
    pub(in crate::session_pool) fn admit_mcp_server(
        &self,
        settings: &settings::LoadedSettings,
        name: &str,
    ) -> Option<Admission> {
        let configured = self.configured_mcp_servers(&settings.values);
        let value = configured.get(name)?;
        Some(McpJudge::of(self, settings).admit(self, settings, name, value))
    }

    /// Connect the default project's MCP servers (`configured_mcp_servers`)
    /// in the background — never blocks the caller. Each outcome is a
    /// `daemon.event`: `mcp_connected`, or `mcp_connect_failed` for a config
    /// that does not parse, a server the policy denies, or one that could
    /// not be reached, with the reason. This is the default project's first
    /// connect (see `first_mcp_connect`), so a session created meanwhile
    /// waits for it rather than connecting again.
    pub fn connect_mcp_servers_in_background(self: &Arc<Self>) {
        self.watch_mcp_auth();
        let pool = self.clone();
        tokio::spawn(async move {
            let settings = pool.bindings.pool_settings().await;
            if pool.configured_mcp_servers(&settings.values).is_empty() {
                return;
            }
            let key = (Some(pool.cwd().to_path_buf()), settings.paths.scope.clone());
            let manager = pool.first_mcp_connect(key, &settings).await;
            for status in manager.server_statuses() {
                pool.emit_event(mcp_connected_event(&status));
            }
            let not_connected = manager.failed().iter().map(|f| &f.name);
            for name in not_connected.chain(manager.pending().iter().map(|p| &p.name)) {
                pool.emit_event(mcp_connect_failed_event(&manager, name));
            }
        });
    }

    /// The pair's MCP manager: the cached one, or a new connect of the
    /// servers `settings` declare, cached. The connect returns after
    /// [`MCP_SESSION_CONNECT_WAIT`]; a server still connecting then is
    /// finished by [`Self::finish_mcp_connects`]. A caller that arrives while
    /// the pair's first connect runs waits for it rather than starting every
    /// server a second time; other pairs do not wait.
    pub(in crate::session_pool) async fn first_mcp_connect(
        &self,
        key: ProjectSceneKey,
        settings: &settings::LoadedSettings,
    ) -> Arc<McpManager> {
        self.first_mcp_connect_without(key, settings, None).await
    }

    /// [`Self::first_mcp_connect`] leaving `except` out, for a caller that
    /// connects that one itself.
    pub(in crate::session_pool) async fn first_mcp_connect_without(
        &self,
        key: ProjectSceneKey,
        settings: &settings::LoadedSettings,
        except: Option<&str>,
    ) -> Arc<McpManager> {
        let slot = self.mcp.cache.slot(&key);
        let update = slot.update().await;
        if let Some(existing) = update.current() {
            return existing;
        }
        let AdmittedMcp {
            servers: mut admitted,
            denied,
            invalid,
            ..
        } = self.admit_mcp_servers(settings);
        if let Some(name) = except {
            admitted.remove(name);
        }
        let pool = self.this.upgrade();
        let wait = pool.as_ref().map(|_| MCP_SESSION_CONNECT_WAIT);
        let (mut manager, late) = McpManager::connect_all_within(
            admitted,
            settings.values.sandbox.subprocess_env.clone(),
            Some(self.mcp.auth.clone()),
            wait,
        )
        .await;
        // A server may have announced a change while this manager was being
        // built, after its list was read.
        manager.refresh_if_changed().await;
        let manager = Arc::new(manager.with_denied(denied).with_invalid(invalid));
        update.set(manager.clone());
        drop(update);
        if let (Some(pool), Some(late)) = (pool, late) {
            tokio::spawn(Self::finish_mcp_connects(Arc::downgrade(&pool), key, late));
        }
        manager
    }

    /// Take each server that finishes after its pair's manager was handed
    /// out into that manager, announce it, and have the pair's sessions
    /// rebuilt on it before their next turn. Stops when the pair's manager
    /// has been replaced or dropped since — a reload connects anew.
    pub(in crate::session_pool) async fn finish_mcp_connects(
        pool: std::sync::Weak<Self>,
        key: ProjectSceneKey,
        mut late: mcp::manager::LateConnects,
    ) {
        while let Some(outcome) = late.next().await {
            let Some(pool) = pool.upgrade() else {
                return;
            };
            let server = outcome.server().to_string();
            let connected = outcome.connected();
            let Some(slot) = pool.mcp.cache.existing(&key) else {
                return;
            };
            let updated = {
                let update = slot.update().await;
                let Some(current) = update.current() else {
                    return;
                };
                let mut updated = current.share();
                if !updated.absorb(outcome) {
                    continue;
                }
                let updated = Arc::new(updated);
                update.set(updated.clone());
                updated
            };
            pool.emit_event(match connected {
                Some(s) => mcp_connected_event(&s),
                None => mcp_connect_failed_event(&updated, &server),
            });
            pool.invalidate_mcp_keys(std::slice::from_ref(&key)).await;
        }
    }

    /// Reconnect MCP servers to match `mcp_servers` (from freshly-reloaded
    /// `Settings`) — the `config.reload`/`config.setProvider` counterpart to
    /// `connect_mcp_servers_in_background`, which runs once at daemon
    /// startup. `create()` sources MCP from the MCP cache, not
    /// from `Settings`, so a reloaded `mcp_servers` reaches sessions only
    /// through this.
    ///
    /// Connects the *complete* new config rather than diffing it against the
    /// old one: a config that removed every server produces an empty
    /// `McpManager`, which disconnects everything. Awaited inline, unlike the
    /// startup path, so the `config.reload` / `config.setProvider` response
    /// reports the connections as they are — but for at most
    /// [`MCP_SESSION_CONNECT_WAIT`]: a server still connecting then is
    /// reported as connecting and finished in the background like a
    /// session's (`finish_mcp_connects`).
    ///
    /// Only replaces the cache entry for `(project_root, scene_id)`;
    /// every other pair's cached connections are left untouched. Without
    /// `connect_pair`, a pair that never connected is left so: its servers
    /// start with its first session, as `mcp.status` and the single-server
    /// writes leave them.
    ///
    /// Returns `(connected, failed, connecting)` server names for
    /// the reload's `RoutingStatus`.
    pub(in crate::session_pool) async fn reconcile_mcp_servers(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        settings: &settings::LoadedSettings,
        connect_pair: bool,
    ) -> (Vec<String>, Vec<String>, Vec<String>) {
        let key = (project_root.map(Path::to_path_buf), scene_id.to_string());
        if !connect_pair && self.mcp.cache.current(&key).is_none() {
            return Default::default();
        }
        let slot = self.mcp.cache.slot(&key);
        let update = slot.update().await;
        let AdmittedMcp {
            servers: admitted,
            denied,
            invalid,
            refused,
            ..
        } = self.admit_mcp_servers(settings);
        let requested: std::collections::HashSet<String> = admitted.keys().cloned().collect();
        let pool = self.this.upgrade();
        let wait = pool.as_ref().map(|_| MCP_SESSION_CONNECT_WAIT);
        let (manager, late) = McpManager::connect_all_within(
            admitted,
            settings.values.sandbox.subprocess_env.clone(),
            Some(self.mcp.auth.clone()),
            wait,
        )
        .await;
        let manager = manager.with_denied(denied).with_invalid(invalid);
        let statuses = manager.server_statuses();
        let connected: Vec<String> = statuses.iter().map(|s| s.name.clone()).collect();
        let mut connecting: Vec<String> = manager
            .connecting()
            .iter()
            .map(|c| c.name.clone())
            .collect();
        connecting.sort();
        let settled: std::collections::HashSet<&String> =
            connected.iter().chain(&connecting).collect();
        let unreachable: Vec<String> = requested
            .iter()
            .filter(|name| !settled.contains(name))
            .cloned()
            .collect();

        for status in &statuses {
            self.emit_event(mcp_connected_event(status));
        }
        for name in &unreachable {
            self.emit_event(mcp_connect_failed_event(&manager, name));
        }
        let mut failed = unreachable;
        failed.extend(refused);

        update.set(Arc::new(manager));
        drop(update);
        if let (Some(pool), Some(late)) = (pool, late) {
            tokio::spawn(Self::finish_mcp_connects(Arc::downgrade(&pool), key, late));
        }
        (connected, failed, connecting)
    }
}

/// What admitting a server depends on besides the server: which names a
/// plugin declares, and which the project's own tier does.
struct McpJudge {
    plugin_servers: std::collections::HashSet<String>,
    projects_own: std::collections::HashSet<String>,
}

impl McpJudge {
    fn of(pool: &SessionPool, settings: &settings::LoadedSettings) -> Self {
        let plugin_servers: std::collections::HashSet<String> =
            pool.plugins.mcp_servers().into_keys().collect();
        let projects_own = project_tier_mcp_servers(settings)
            .into_iter()
            .filter(|name| !plugin_servers.contains(name))
            .collect();
        Self {
            plugin_servers,
            projects_own,
        }
    }

    fn admit(
        &self,
        pool: &SessionPool,
        settings: &settings::LoadedSettings,
        name: &str,
        value: &serde_json::Value,
    ) -> Admission {
        if settings.values.mcp_enabled.get(name) == Some(&false) {
            return Admission::Disabled;
        }
        let cfg = match serde_json::from_value::<mcp::config::McpServerConfig>(value.clone()) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(server = %name, error = %e, "invalid mcp_servers config, skipping");
                return Admission::Invalid(mcp::manager::FailedServer {
                    name: name.to_string(),
                    transport: "unknown",
                    error: format!("invalid config: {e}"),
                });
            }
        };
        let projects_own = self.projects_own.contains(name);
        let admitted = if projects_own {
            cfg.confined_to_own_env().inspect_err(|reason| {
                tracing::warn!(server = %name, %reason, "MCP server not connected");
            })
        } else {
            Ok(cfg.clone())
        }
        .and_then(|confined| pool.admit_mcp(name, &confined).map(|()| confined));
        match admitted {
            Ok(confined) => {
                let tier = if self.plugin_servers.contains(name) {
                    "plugin"
                } else if projects_own {
                    "project"
                } else {
                    "global"
                };
                Admission::Admitted(Box::new(
                    pool.placed_mcp_server(settings, name, confined)
                        .declared_in(tier),
                ))
            }
            Err(reason) => Admission::Denied(mcp::manager::DeniedServer {
                name: name.to_string(),
                transport: cfg.transport_kind(),
                reason,
            }),
        }
    }
}
