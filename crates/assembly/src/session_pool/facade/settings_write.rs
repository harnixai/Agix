//! 按层写设置的唯一实现 `write_settings_tier`：找到那一层、在锁下读改判写、认领自己
//! 写下的内容让监视器不再重载一遍、然后走"这一层变了"的那一条路。`config.set`、
//! `config.setProvider`、`mcp.*` 的写入都架在它上面。

use crate::session_pool::*;
use crate::watch_hub::Tier;

impl SessionPool {
    /// The project a write to `tier` of the pair is judged in, and the tier
    /// as the watcher names it. The scene must be one this daemon has; a
    /// project tier must be a directory with a tier of its own.
    pub(in crate::session_pool) async fn tier_target(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        tier: api::SettingsTier,
    ) -> Result<(PathBuf, Tier), PoolError> {
        if self.scenes.registry().resolve(scene_id).is_none() {
            return Err(PoolError::SceneNotFound {
                scene: scene_id.to_string(),
            });
        }
        let target = project_root.unwrap_or(self.cwd()).to_path_buf();
        let watched = match tier {
            api::SettingsTier::Global => Tier::Global,
            api::SettingsTier::Scene => {
                let startup_scene = self.bindings.startup_scene().await;
                let dir = self.bindings.scene_root(scene_id, &startup_scene);
                if settings::trust::same_dir(&dir, &self.bindings.paths().global_root()) {
                    Tier::Global
                } else {
                    Tier::Scene(scene_id.to_string())
                }
            }
            api::SettingsTier::Project => {
                if !target.is_dir() {
                    return Err(PoolError::ProjectNotFound { path: target });
                }
                self.own_project_tier(
                    Some(&target),
                    scene_id,
                    "settings written there would reach every project",
                    "write the global tier instead",
                )
                .await?;
                Tier::Project(target.clone())
            }
        };
        Ok((target, watched))
    }

    /// The one way the pool writes a settings tier: `edit.tier` of the pair
    /// `(project_root, scene_id)`. Nothing is written when a changed key is
    /// refused (`SettingsRejected`); with `dry_run` nothing is written at all,
    /// no directory made and no lock taken, and the refusals come back in the
    /// result. After a write, the watcher is told the pool's own content is
    /// not news, and the change goes the way any change to that tier goes,
    /// MCP as `edit.mcp` says.
    pub(in crate::session_pool) async fn write_settings_tier(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        edit: TierEdit,
    ) -> Result<TierWritten, PoolError> {
        let (target, watched) = self.tier_target(project_root, scene_id, edit.tier).await?;
        let reader = self.bindings.pair_reader(&target, scene_id).await;
        let paths = TierPaths::of(&reader, edit.tier)?;
        let judge_trust = edit.tier == api::SettingsTier::Project;
        let TierEdit {
            ops,
            if_revision,
            dry_run,
            precondition,
            mcp,
            ..
        } = edit;
        let (shared, local) = (paths.shared.clone(), paths.local.clone());
        let run = move |held: Option<&settings::file::SettingsDir>| {
            edit_tier(
                &paths,
                held,
                &ops,
                if_revision.as_deref(),
                precondition,
                &reader,
                judge_trust,
            )
        };
        let edited = if dry_run {
            off_the_runtime(move || run(None)).await
        } else {
            edit_settings_dir(&shared, move |held| Ok(run(Some(held))))
                .await
                .map_err(|e| e.into_pool(PoolError::InvalidRequest))?
        };
        let edited = edited.map_err(|e| match e {
            EditFailure::Conflict(revision) => PoolError::SettingsConflict { revision },
            EditFailure::Failed(message) => PoolError::InvalidRequest(message),
        })?;
        if !dry_run && !edited.rejected.is_empty() {
            return Err(refused(edited.rejected));
        }
        let (loaded, routing) = if dry_run {
            self.routed_dry_run(&target, scene_id, edited.loaded).await
        } else {
            self.bindings
                .adopt_written(&[shared.clone(), local.clone()]);
            self.settings_tier_changed(Some(&watched), (&target, scene_id), mcp)
                .await
        };
        Ok(TierWritten {
            written_to: shared,
            secrets_written_to: edited.local_changed.then_some(local),
            rejected: edited.rejected,
            revision: edited.revision,
            loaded,
            routing,
            warnings: edited.warnings,
        })
    }

    /// A dry run's settings with the host's and managed layers on top, and
    /// the routing they would build; no MCP server is touched.
    async fn routed_dry_run(
        &self,
        target: &Path,
        scene_id: &str,
        loaded: LoadedSettings,
    ) -> (Arc<LoadedSettings>, api::RoutingStatus) {
        let (loaded, routing) = self.bindings.routed_as_if(target, scene_id, loaded).await;
        let status = api::RoutingStatus {
            ok: routing.error.is_none(),
            warnings: routing.warnings,
            error: routing.error,
            router_rebuilt: routing.router_rebuilt,
            router_error: routing.router_error,
            mcp_connected: Vec::new(),
            mcp_failed: Vec::new(),
            mcp_connecting: Vec::new(),
        };
        (Arc::new(loaded), status)
    }
}
