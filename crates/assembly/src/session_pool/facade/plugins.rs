//! 插件：加载、安装卸载、启停、宿主自带包的登记，以及变更后的刷新。

use crate::session_pool::*;

impl SessionPool {
    /// Take up the scenes installed packages own.
    ///
    /// **Every embedder that constructs a `SessionPool` must call this
    /// before serving**: `new` is synchronous and cannot, and a session
    /// created before it runs cannot enter a package's scene.
    pub async fn load_plugins(&self) {
        self.adopt_plugin_scenes().await;
    }

    /// Rebuild the scene registry from the built-ins plus whatever the
    /// current plugin set owns, and make those scenes servable.
    ///
    /// Plugin scenes are activated rather than merely registered: installing
    /// and enabling a plugin that owns a scene *is* the user's consent to
    /// serve it, and requiring a second `scene.activate` would be a step
    /// with no safety it buys — entering the scene is still an explicit
    /// `session.create`.
    ///
    /// Withdrawing one removes it from both maps, which stops new sessions
    /// from entering it. Sessions already inside keep the `Arc` they were
    /// built with and run to completion — the alternative would be pulling a
    /// session's system prompt out from under it mid-turn.
    pub(in crate::session_pool) async fn adopt_plugin_scenes(&self) {
        self.scenes.adopt_plugin_scenes(self.plugins.scenes());
    }

    /// Whether this binary has the plugin subsystem, and whether it is on —
    /// see `crate::plugins::PluginStatus`. `daemon.doctor` reports it, and
    /// the `plugin.*` RPCs refuse with `PLUGINS_DISABLED` when it is not
    /// `Enabled`.
    pub fn plugin_status(&self) -> crate::plugins::PluginStatus {
        self.plugins.status()
    }

    /// Every installed plugin with its current enable state — for the
    /// `plugin.list` RPC. Includes disabled ones, since a management UI needs
    /// to show and re-enable them. Which of a package's skills are shadowed
    /// is answered for the daemon's own project and scene, like the rest of
    /// `plugin.*`.
    pub async fn list_plugins(&self) -> Result<Vec<api::PluginInfo>, PoolError> {
        self.require_plugins()?;
        Ok(self.plugin_infos().await)
    }

    async fn plugin_infos(&self) -> Vec<api::PluginInfo> {
        let mut listed = self.plugins.list();
        let binding = self.default_binding().await;
        let mut conflicts = binding.modules.skill_conflicts();
        let mut command_conflicts = self.command_conflicts(&binding.commands);
        for entry in &mut listed {
            entry.skill_conflicts = conflicts.remove(&entry.name).unwrap_or_default();
            entry.command_conflicts = command_conflicts.remove(&entry.name).unwrap_or_default();
        }
        listed
    }

    /// Package commands whose name another holds, by package: an earlier
    /// package's command of the name, or a skill that takes its `/name`.
    fn command_conflicts(
        &self,
        commands: &runtime::commands::CommandRegistry,
    ) -> std::collections::HashMap<String, Vec<api::CommandConflict>> {
        let mut out: std::collections::HashMap<String, Vec<api::CommandConflict>> =
            Default::default();
        for (command, kept, refused) in self.plugins.command_clashes() {
            out.entry(refused).or_default().push(api::CommandConflict {
                command,
                shadowed_by: api::CommandShadow::Plugin,
                plugin: Some(kept),
                path: None,
            });
        }
        for command in self.plugins.commands() {
            if let Some(runtime::commands::Command::Prompt { entry }) =
                commands.resolve(&command.name)
            {
                out.entry(command.plugin)
                    .or_default()
                    .push(api::CommandConflict {
                        command: command.name,
                        shadowed_by: api::CommandShadow::Skill,
                        plugin: None,
                        path: Some(entry.path.display().to_string()),
                    });
            }
        }
        out
    }

    /// The plugins the session of `spec` draws on, over the pool's active
    /// set as it is now.
    pub(in crate::session_pool) fn session_plugins(
        &self,
        spec: &SessionSpec,
    ) -> crate::plugins::SessionPlugins {
        self.plugins.session_plugins(spec.settings_layer()).0
    }

    /// The plugins live `session_id`'s engine was built to draw on; `None`
    /// for a session not in memory.
    pub(in crate::session_pool) async fn live_session_plugins(
        &self,
        session_id: &str,
    ) -> Option<Arc<crate::plugins::SessionPlugins>> {
        self.live
            .with_live(session_id, |live| live.engine.plugins.clone())
            .await
    }

    /// The plugins `session_id` draws on: its engine's when it is in memory,
    /// else what its spec asks of the pool's active set now.
    pub(in crate::session_pool) async fn plugins_of_session(
        &self,
        session_id: &str,
    ) -> Arc<crate::plugins::SessionPlugins> {
        if let Some(live) = self.live_session_plugins(session_id).await {
            return live;
        }
        let spec = match self.specs.get(session_id) {
            Some(spec) => spec,
            None => self.specs.recorded(session_id).await,
        };
        Arc::new(self.session_plugins(&spec))
    }

    fn require_plugins(&self) -> Result<(), PoolError> {
        let status = self.plugins.status();
        if status == crate::plugins::PluginStatus::Enabled {
            return Ok(());
        }
        Err(PoolError::PluginsDisabled {
            status: status.as_str(),
        })
    }

    /// Install a plugin from an explicit source. `scope` picks which tier's
    /// cache it lands in ("global", the default a caller should use unless it
    /// specifically wants a scene-only install, or "scene"). Refreshes the
    /// active set on success so sessions created after this call see it —
    /// already-running sessions are unaffected, same as
    /// `config.setProvider`/`mcp.addServer`.
    /// Re-read the plugin directories and reload what is there.
    ///
    /// Install/uninstall/enable/disable each refresh on their own, so this is
    /// for the case none of them covers: the files changed without going
    /// through an RPC — a plugin dropped into the directory by hand, or a
    /// rebuilt component swapped in during development. Without it the only
    /// way to pick that up is restarting the daemon.
    ///
    /// Returns the installed set afterwards, so a caller learns what the
    /// rescan actually found without a second call.
    pub async fn reload_plugins(&self) -> Result<Vec<api::PluginInfo>, PoolError> {
        self.require_plugins()?;
        self.plugins.refresh().await;
        self.refresh_after_plugin_change().await;
        Ok(self.plugin_infos().await)
    }

    pub async fn install_plugin(
        &self,
        name: &str,
        version: &str,
        download_url: &str,
        checksum: Option<&str>,
        scope: api::PluginScope,
    ) -> Result<api::PluginInstalled, PoolError> {
        self.require_plugins()?;
        let out = self
            .plugins
            .install(name, version, download_url, checksum, scope)
            .await
            .map_err(PoolError::InvalidRequest)?;
        self.refresh_after_plugin_change().await;
        Ok(out)
    }

    /// Uninstall a plugin (all versions, or a specific one) from `scope`'s
    /// tier — see `install_plugin` for the refresh semantics.
    pub async fn uninstall_plugin(
        &self,
        name: &str,
        version: Option<&str>,
        scope: api::PluginScope,
    ) -> Result<api::PluginRemoved, PoolError> {
        self.require_plugins()?;
        let out = self
            .plugins
            .uninstall(name, version, scope)
            .await
            .map_err(PoolError::InvalidRequest)?;
        self.refresh_after_plugin_change().await;
        Ok(out)
    }

    /// Enable or disable a plugin by name in `scope`'s tier — see
    /// `install_plugin` for the refresh semantics.
    pub async fn set_plugin_enabled(
        &self,
        name: &str,
        enabled: bool,
        scope: api::PluginScope,
    ) -> Result<api::PluginToggled, PoolError> {
        self.require_plugins()?;
        let out = self
            .plugins
            .set_enabled(name, enabled, scope)
            .await
            .map_err(PoolError::InvalidRequest)?;
        self.refresh_after_plugin_change().await;
        Ok(out)
    }

    /// Take up the package unpacked at `dir` as one of the host's own: checked
    /// like an install, loaded at once in place of an installed package of
    /// its name, and beyond `plugin.uninstall` and `plugin.disable`. A second
    /// registration of a name replaces the first. Sessions created after
    /// this see it, as after `install_plugin`. From then on its directory is
    /// protected as a host skill directory is: no tool and no sandboxed
    /// command of any session writes under it.
    pub async fn register_host_plugin(
        &self,
        dir: impl AsRef<std::path::Path>,
    ) -> Result<api::PluginInfo, PoolError> {
        self.require_plugins()?;
        let name = self
            .plugins
            .register_host(dir.as_ref())
            .map_err(PoolError::InvalidRequest)?;
        let dir = dir.as_ref();
        let given = std::path::absolute(dir).map(|dir| base::path::normalize_lexically(&dir));
        self.bindings
            .host_dirs()
            .0
            .protected()
            .extend(given.into_iter().chain(dir.canonicalize()));
        self.refresh_after_plugin_change().await;
        self.plugin_infos()
            .await
            .into_iter()
            .find(|p| p.name == name && p.source == api::PluginOrigin::Host)
            .ok_or_else(|| PoolError::Internal(format!("host package `{name}` is not listed")))
    }

    /// Re-merge whatever the plugin set now contributes into the catalogs of
    /// every binding. The subsystem has already re-read itself by this point;
    /// what is left is the state this pool derives from it. The catalogs are
    /// swapped in place, so a running session's next turn sees a package's
    /// skills come or go. A package's MCP servers leave with it; one a
    /// package brings waits for its pair to connect anew.
    pub(in crate::session_pool) async fn refresh_after_plugin_change(&self) {
        self.adopt_plugin_scenes().await;
        self.bindings.refresh_plugins().await;
        self.drop_unconfigured_mcp_servers().await;
    }
}
