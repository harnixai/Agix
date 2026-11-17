//! MCP 的管理：`mcp.addServer` / `updateServer` / `removeServer` / `setEnabled` 经按层写设置的
//! 核心写任一层，写完只对那一个 server 对账，只有 `addServer` 会连起一个没连过的对；
//! `mcp.reconnect` 只重连一个；`mcp.status` 报任一对 `(项目, 场景)` 的状态，没连过的对只读设置、
//! 不启动任何东西。

use super::*;
use crate::session_pool::parts::bindings::tier_covers;
use crate::session_pool::*;
use settings::edit::{EditOp, KeyPath, TierFiles};

impl SessionPool {
    /// `mcp.addServer` — write a server's configuration into `tier` — its
    /// `headers` and `env` into the uncommitted `settings.local.json`, the
    /// rest into settings.json — and connect it.
    pub async fn add_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        tier: api::SettingsTier,
        name: &str,
        config: serde_json::Value,
    ) -> Result<api::McpServerWritten, PoolError> {
        self.put_mcp_server(project_root, scene_id, tier, name, config, false)
            .await
    }

    /// `mcp.updateServer` — replace the configuration of a server `tier`
    /// already declares (a rotated token, a new header) and reconnect it.
    pub async fn update_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        tier: api::SettingsTier,
        name: &str,
        config: serde_json::Value,
    ) -> Result<api::McpServerWritten, PoolError> {
        self.put_mcp_server(project_root, scene_id, tier, name, config, true)
            .await
    }

    /// Refused before anything is written when the MCP policy denies it:
    /// a settings entry the pool would only ever deny is no use to anyone.
    /// A project's own server is held to what a project may declare.
    async fn put_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        tier: api::SettingsTier,
        name: &str,
        config: serde_json::Value,
        must_exist: bool,
    ) -> Result<api::McpServerWritten, PoolError> {
        let invalid = |message: String| PoolError::InvalidRequest(message);
        check_server_name(name)?;
        if !config.is_object() {
            return Err(invalid("config must be a JSON object".into()));
        }
        let mut cfg: mcp::config::McpServerConfig = serde_json::from_value(config.clone())
            .map_err(|e| invalid(format!("invalid mcp server config: {e}")))?;
        if tier == api::SettingsTier::Project {
            self.trusted_project_tier(
                project_root,
                scene_id,
                "a server declared in its settings would not start",
                "declare the server in the global tier",
            )
            .await?;
            cfg = cfg.confined_to_own_env().map_err(invalid)?;
        }
        self.admit_mcp(name, &cfg)
            .map_err(PoolError::McpServerDenied)?;
        let precondition = must_exist.then(|| declared_in_tier(name, false));
        let ops = vec![EditOp::Set(server_key(name), config)];
        let effect = McpEffect::Server {
            name: name.to_string(),
            connect_pair: !must_exist,
        };
        self.write_mcp_settings(project_root, scene_id, tier, effect, ops, precondition)
            .await
    }

    /// `mcp.removeServer` — take a server out of `tier`, with whatever
    /// `settings.local.json` held for it and the tier's `mcp_enabled` entry.
    /// Where another tier still declares it, it reconnects as that one says.
    pub async fn remove_mcp_server(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        tier: api::SettingsTier,
        name: &str,
    ) -> Result<api::McpServerWritten, PoolError> {
        check_server_name(name)?;
        let from_plugin = self.plugins.mcp_servers().contains_key(name);
        let ops = vec![
            EditOp::Remove(server_key(name)),
            EditOp::Remove(enabled_key(name)),
        ];
        let precondition = Some(declared_in_tier(name, from_plugin));
        self.write_mcp_settings(
            project_root,
            scene_id,
            tier,
            wait_for_a_session(name),
            ops,
            precondition,
        )
        .await
    }

    /// `mcp.setEnabled` — turn a server off or back on in `tier`, wherever it
    /// is declared, plugins included.
    pub async fn set_mcp_enabled(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        tier: api::SettingsTier,
        name: &str,
        enabled: bool,
    ) -> Result<api::McpServerWritten, PoolError> {
        check_server_name(name)?;
        self.check_pair(project_root, scene_id)?;
        let root = project_root.unwrap_or(self.cwd());
        let settings = self.settings_for_project(Some(root), scene_id).await;
        let known = self
            .configured_mcp_servers(&settings.values)
            .contains_key(name)
            || settings.project_mcp_servers().contains_key(name);
        let precondition = (!known).then(|| {
            let server = name.to_string();
            Box::new(move |files: &TierFiles, path: &Path| {
                if files.declares(&server_key(&server)) || files.declares(&enabled_key(&server)) {
                    return Ok(());
                }
                Err(format!(
                    "no MCP server `{server}` is configured for this project and scene, nor named in {}",
                    path.display()
                ))
            }) as Precondition
        });
        let ops = vec![EditOp::Set(
            enabled_key(name),
            serde_json::Value::Bool(enabled),
        )];
        self.write_mcp_settings(
            project_root,
            scene_id,
            tier,
            wait_for_a_session(name),
            ops,
            precondition,
        )
        .await
    }

    async fn write_mcp_settings(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        tier: api::SettingsTier,
        mcp: McpEffect,
        ops: Vec<EditOp>,
        precondition: Option<Precondition>,
    ) -> Result<api::McpServerWritten, PoolError> {
        let edit = TierEdit {
            precondition,
            mcp,
            ..TierEdit::new(tier, ops)
        };
        let written = self
            .write_settings_tier(project_root, scene_id, edit)
            .await?;
        let root = project_root.unwrap_or(self.cwd());
        Ok(api::McpServerWritten {
            written_to: written.written_to.display().to_string(),
            secrets_written_to: written.secrets_written_to.map(|p| p.display().to_string()),
            servers: self.mcp_status(Some(root), scene_id).await?.servers,
            warnings: written.warnings,
        })
    }

    /// `mcp.reconnect` — connect one server of the pair again, as the pair's
    /// settings now configure it; every other server keeps its connection.
    pub async fn mcp_reconnect(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
    ) -> Result<api::McpServerReconnected, PoolError> {
        self.check_pair(project_root, scene_id)?;
        let root = project_root.unwrap_or(self.cwd());
        let key = (Some(root.to_path_buf()), scene_id.to_string());
        if self.mcp.cache.current(&key).is_none() {
            return Err(PoolError::InvalidRequest(
                "the MCP servers of this project and scene have not been connected yet; they \
                 connect with its first session"
                    .into(),
            ));
        }
        let settings = self.settings_for_project(Some(root), scene_id).await;
        match self.admit_mcp_server(&settings, name) {
            Some(Admission::Admitted(_)) => {}
            Some(Admission::Disabled) => {
                return Err(PoolError::InvalidRequest(format!(
                    "`{name}` is turned off; mcp.setEnabled turns it back on"
                )))
            }
            Some(Admission::Denied(denied)) => {
                return Err(PoolError::McpServerDenied(denied.reason))
            }
            Some(Admission::Invalid(failed)) => {
                return Err(PoolError::InvalidRequest(failed.error))
            }
            None if is_untrusted_own(&settings, name) => {
                return Err(PoolError::ProjectUntrusted(format!(
                    "{} is not trusted, so its own server `{name}` does not start; trust it first \
                     (project.trust)",
                    root.display()
                )))
            }
            None => {
                return Err(PoolError::InvalidRequest(format!(
                    "no MCP server `{name}` is configured for this project and scene"
                )))
            }
        }
        let server = self
            .reconcile_mcp_server(&key, name, true)
            .await
            .ok_or_else(|| PoolError::Internal(format!("`{name}` was not reconnected")))?;
        Ok(api::McpServerReconnected { server })
    }

    /// `mcp.status` for the pair. One whose servers have not been connected
    /// is answered from its settings alone: nothing is connected, no binding
    /// is built and no process starts — for a project that is not trusted
    /// that is the point.
    pub async fn mcp_status(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> Result<api::McpStatusList, PoolError> {
        self.check_pair(project_root, scene_id)?;
        let root = project_root.unwrap_or(self.cwd());
        let key = (Some(root.to_path_buf()), scene_id.to_string());
        let own = self.bindings.is_own_pair(Some(root), scene_id).await;
        if let Some(manager) = self.mcp.cache.current(&key) {
            let settings = if own {
                self.bindings.pool_settings().await
            } else {
                self.settings_for_project(Some(root), scene_id).await
            };
            let mut servers = mcp_status_entries(&manager);
            servers.extend(untrusted_mcp_entries(&settings));
            servers.extend(self.disabled_mcp_entries(&settings, &servers));
            return Ok(api::McpStatusList {
                loaded: true,
                servers,
            });
        }
        let settings = if own {
            self.bindings.pool_settings().await
        } else {
            Arc::new(self.bindings.load_pair(Some(root), scene_id).await)
        };
        Ok(api::McpStatusList {
            loaded: false,
            servers: self.unstarted_mcp_entries(&settings),
        })
    }

    fn check_pair(&self, project_root: Option<&Path>, scene_id: &str) -> Result<(), PoolError> {
        if self.scenes.registry().resolve(scene_id).is_none() {
            return Err(PoolError::SceneNotFound {
                scene: scene_id.to_string(),
            });
        }
        match project_root {
            Some(root) if !root.is_dir() => Err(PoolError::ProjectNotFound {
                path: root.to_path_buf(),
            }),
            _ => Ok(()),
        }
    }

    /// The servers `settings` turn off that `listed` does not already name.
    fn disabled_mcp_entries(
        &self,
        settings: &LoadedSettings,
        listed: &[api::McpServerStatus],
    ) -> Vec<api::McpServerStatus> {
        self.configured_mcp_servers(&settings.values)
            .iter()
            .filter(|(name, _)| settings.values.mcp_enabled.get(*name) == Some(&false))
            .filter(|(name, _)| !listed.iter().any(|s| &s.name == *name))
            .map(|(name, value)| disabled_entry(name, value))
            .collect()
    }

    /// Each server `settings` configure, as far as can be told without
    /// connecting it.
    fn unstarted_mcp_entries(&self, settings: &LoadedSettings) -> Vec<api::McpServerStatus> {
        use api::McpServerState as State;
        let mut servers: Vec<api::McpServerStatus> = self
            .configured_mcp_servers(&settings.values)
            .iter()
            .filter_map(|(name, value)| {
                Some(match self.admit_mcp_server(settings, name)? {
                    Admission::Admitted(cfg) => {
                        let auth = self
                            .mcp
                            .auth
                            .status(name, &cfg)
                            .map(api::McpAuthStatus::from)
                            .unwrap_or_default();
                        idle_mcp_entry(name, cfg.transport_kind(), State::NotStarted, None, auth)
                    }
                    Admission::Disabled => disabled_entry(name, value),
                    Admission::Invalid(f) => idle_mcp_entry(
                        name,
                        f.transport,
                        State::Failed,
                        Some(f.error),
                        Default::default(),
                    ),
                    Admission::Denied(d) => idle_mcp_entry(
                        name,
                        d.transport,
                        State::Denied,
                        Some(d.reason),
                        Default::default(),
                    ),
                })
            })
            .collect();
        servers.extend(untrusted_mcp_entries(settings));
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        servers
    }

    /// Look at `name` again in the pair's connections, under the pair's
    /// update lock, as the pair's settings now say: connect it when they admit
    /// it (again with `force`, or when its configuration changed or it is not
    /// connected), take it out when they turn it off, refuse it or no longer
    /// configure it. Every other server keeps its connection. `None` when the
    /// pair has no connections, or the server is gone from them.
    pub(in crate::session_pool) async fn reconcile_mcp_server(
        &self,
        key: &ProjectSceneKey,
        name: &str,
        force: bool,
    ) -> Option<api::McpServerStatus> {
        let (status, event) = self.reconcile_unannounced(key, name, force).await?;
        if let Some(event) = event {
            self.emit_event(event);
        }
        status
    }

    /// [`Self::reconcile_mcp_server`] handing back the event that announces
    /// what came of it instead of sending it; `None` when the pair has no
    /// connections.
    pub(in crate::session_pool) async fn reconcile_unannounced(
        &self,
        key: &ProjectSceneKey,
        name: &str,
        force: bool,
    ) -> Option<(Option<api::McpServerStatus>, Option<api::DaemonEvent>)> {
        let slot = self.mcp.cache.existing(key)?;
        let settings = self.settings_for_project(key.0.as_deref(), &key.1).await;
        let admission = self.admit_mcp_server(&settings, name);
        let (entries, event) = {
            let update = slot.update().await;
            let current = update.current()?;
            let mut updated = current
                .share()
                .with_subprocess_env(settings.values.sandbox.subprocess_env.clone())
                .with_auth(Some(self.mcp.auth.clone()));
            updated.refresh_tools().await;
            let event = apply_admission(&mut updated, name, admission, force).await;
            let entries = mcp_status_entries(&updated);
            update.set(Arc::new(updated));
            (entries, event)
        };
        self.invalidate_mcp_keys(std::slice::from_ref(key)).await;
        let status = entries
            .into_iter()
            .find(|s| s.name == name)
            .or_else(|| {
                self.disabled_mcp_entries(&settings, &[])
                    .into_iter()
                    .find(|s| s.name == name)
            })
            .or_else(|| {
                untrusted_mcp_entries(&settings)
                    .into_iter()
                    .find(|s| s.name == name)
            });
        Some((status, event))
    }

    /// What a write that touched only `name` does to connections: the pair it
    /// targeted reconciled now — connected whole first if it never was and
    /// `connect_pair` says so, else left for its first session — and every
    /// other connected pair under `changed` in the background. The pair's
    /// `(connected, failed, connecting)` names, for the routing report.
    pub(in crate::session_pool) async fn reconcile_written_mcp_server(
        &self,
        changed: Option<&crate::watch_hub::Tier>,
        key: &ProjectSceneKey,
        name: &str,
        connect_pair: bool,
    ) -> (Vec<String>, Vec<String>, Vec<String>) {
        let status = if self.mcp.cache.current(key).is_some() {
            self.reconcile_mcp_server(key, name, true).await
        } else if connect_pair {
            self.connect_pair_for(key, name).await
        } else {
            None
        };
        if let Some(tier) = changed {
            self.reconcile_elsewhere_in_background(tier, key, name);
        }
        use api::McpServerState as State;
        let mut report: (Vec<String>, Vec<String>, Vec<String>) = Default::default();
        match status.map(|s| s.state) {
            Some(State::Connected | State::Disconnected) => report.0.push(name.to_string()),
            Some(State::Failed | State::Denied | State::AuthRequired) => {
                report.1.push(name.to_string())
            }
            Some(State::Connecting) => report.2.push(name.to_string()),
            _ => {}
        }
        report
    }

    /// The pair's first connect, made now because `name` was written for it:
    /// its other servers as a session would connect them, `name` once, as
    /// [`Self::reconcile_mcp_server`] connects it.
    async fn connect_pair_for(
        &self,
        key: &ProjectSceneKey,
        name: &str,
    ) -> Option<api::McpServerStatus> {
        let settings = self.settings_for_project(key.0.as_deref(), &key.1).await;
        self.first_mcp_connect_without(key.clone(), &settings, Some(name))
            .await;
        self.reconcile_mcp_server(key, name, true).await
    }

    fn reconcile_elsewhere_in_background(
        &self,
        tier: &crate::watch_hub::Tier,
        written_for: &ProjectSceneKey,
        name: &str,
    ) {
        let others: Vec<ProjectSceneKey> = self
            .mcp
            .cache
            .slots()
            .into_iter()
            .filter(|(key, slot)| {
                key != written_for && tier_covers(tier, key) && slot.current().is_some()
            })
            .map(|(key, _)| key)
            .collect();
        let Some(pool) = self.this.upgrade().filter(|_| !others.is_empty()) else {
            return;
        };
        let name = name.to_string();
        tokio::spawn(async move {
            for key in others {
                pool.reconcile_mcp_server(&key, &name, false).await;
            }
        });
    }
}

/// Bring `name` in `manager` to what `admission` says; the event announcing
/// what came of it, if anything did.
async fn apply_admission(
    manager: &mut McpManager,
    name: &str,
    admission: Option<Admission>,
    force: bool,
) -> Option<api::DaemonEvent> {
    match admission {
        Some(Admission::Admitted(cfg)) => {
            let connected = manager.server_statuses().iter().any(|s| s.name == name);
            let same = manager.config_of(name).is_some_and(|held| {
                serde_json::to_value(held).ok() == serde_json::to_value(&cfg).ok()
            });
            if !force && connected && same {
                return None;
            }
            let denied = manager
                .denied()
                .iter()
                .filter(|d| d.name != name)
                .cloned()
                .collect();
            let invalid = manager
                .invalid()
                .iter()
                .filter(|f| f.name != name)
                .cloned()
                .collect();
            *manager = std::mem::replace(manager, McpManager::empty())
                .with_denied(denied)
                .with_invalid(invalid);
            manager.add_server(name, &cfg).await;
            Some(
                match manager.server_statuses().iter().find(|s| s.name == name) {
                    Some(status) => mcp_connected_event(status),
                    None => mcp_connect_failed_event(manager, name),
                },
            )
        }
        Some(Admission::Invalid(failed)) => {
            manager.remove_server(name);
            let error = failed.error.clone();
            let mut invalid = manager.invalid().to_vec();
            invalid.push(failed);
            *manager = std::mem::replace(manager, McpManager::empty()).with_invalid(invalid);
            Some(api::DaemonEvent::McpConnectFailed {
                server: name.to_string(),
                error,
            })
        }
        Some(Admission::Denied(denied)) => {
            manager.remove_server(name);
            let reason = denied.reason.clone();
            let mut all = manager.denied().to_vec();
            all.push(denied);
            *manager = std::mem::replace(manager, McpManager::empty()).with_denied(all);
            Some(api::DaemonEvent::McpConnectFailed {
                server: name.to_string(),
                error: reason,
            })
        }
        Some(Admission::Disabled) | None => {
            manager.remove_server(name);
            None
        }
    }
}

fn check_server_name(name: &str) -> Result<(), PoolError> {
    if name.trim().is_empty() {
        return Err(PoolError::InvalidRequest("name must not be empty".into()));
    }
    Ok(())
}

fn server_key(name: &str) -> KeyPath {
    KeyPath::new(["mcp_servers", name])
}

fn enabled_key(name: &str) -> KeyPath {
    KeyPath::new(["mcp_enabled", name])
}

/// That the tier's files declare `name`; a server only a plugin declares
/// is turned off with `plugin.disable` or `mcp.setEnabled`.
fn declared_in_tier(name: &str, from_plugin: bool) -> Precondition {
    let server = name.to_string();
    Box::new(move |files: &TierFiles, path: &Path| {
        if files.declares(&server_key(&server)) {
            return Ok(());
        }
        if from_plugin {
            return Err(format!(
                "`{server}` is declared by a plugin package, not in {}; plugin.disable or \
                 mcp.setEnabled turns it off",
                path.display()
            ));
        }
        Err(format!("no mcp server `{server}` in {}", path.display()))
    })
}

/// Whether `name` is the project's own server, held back because the
/// project is not trusted.
fn is_untrusted_own(settings: &LoadedSettings, name: &str) -> bool {
    settings.trust().is_untrusted() && settings.project_mcp_servers().contains_key(name)
}

fn disabled_entry(name: &str, value: &serde_json::Value) -> api::McpServerStatus {
    let transport = serde_json::from_value::<mcp::config::McpServerConfig>(value.clone())
        .map(|c| c.transport_kind())
        .unwrap_or("unknown");
    idle_mcp_entry(
        name,
        transport,
        api::McpServerState::Disabled,
        None,
        Default::default(),
    )
}

fn wait_for_a_session(name: &str) -> McpEffect {
    McpEffect::Server {
        name: name.to_string(),
        connect_pair: false,
    }
}
