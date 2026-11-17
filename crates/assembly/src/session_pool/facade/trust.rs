//! 项目信任：项目层生效前的信任判定、状态查询与写入。

use crate::session_pool::*;

impl SessionPool {
    /// Refuses a write to `project_root`'s settings that would not land in a
    /// project's own tier: a `.agix` that resolves to the global or scene
    /// directory (a link from a repository, or a project rooted at `$HOME`)
    /// would put it in every project, and one linked into another project
    /// would change that project's, so both are `INVALID_PARAMS`. A project
    /// that is not trusted is refused with `PROJECT_UNTRUSTED`: `what` would
    /// not take effect, and `instead` is where it would.
    pub(in crate::session_pool) async fn trusted_project_tier(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        what: &str,
        instead: &str,
    ) -> Result<(), PoolError> {
        self.own_project_tier(project_root, scene_id, what, instead)
            .await?;
        let target = project_root.unwrap_or(self.cwd());
        let decided = settings::trust::ProjectTrustSettings::read_global(
            &self.bindings.paths().global_root(),
        )
        .decide(target);
        if decided.trust.is_untrusted() {
            return Err(PoolError::ProjectUntrusted(format!(
                "{} is not trusted, so {what}; trust it first (project.trust) or {instead}",
                target.display()
            )));
        }
        Ok(())
    }

    /// The `INVALID_PARAMS` half of [`Self::trusted_project_tier`].
    pub(in crate::session_pool) async fn own_project_tier(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        what: &str,
        instead: &str,
    ) -> Result<(), PoolError> {
        let startup_scene = self.bindings.startup_scene().await;
        let written_to = self.bindings.settings_path_of(project_root);
        let dir = written_to.parent().unwrap_or(&written_to);
        let refusal = settings::trust::project_tier_refusal(
            dir,
            &self.bindings.paths().global_root(),
            &self.bindings.scene_root(scene_id, &startup_scene),
        );
        let layout = self.bindings.paths().layout();
        let (root, name) = (
            project_root.unwrap_or(self.cwd()).display(),
            layout.project_dir_name(),
        );
        match refusal {
            None => {}
            Some(settings::trust::NotItsOwnTier::SharedTier) => {
                return Err(PoolError::InvalidRequest(format!(
                    "{root} has no settings tier of its own (its `{name}` is the global or scene directory), so {what}; {instead}"
                )))
            }
            Some(settings::trust::NotItsOwnTier::Elsewhere(to)) => {
                return Err(PoolError::InvalidRequest(format!(
                    "{root}'s `{name}` resolves outside the project, to {}: a write to its settings would change that directory's",
                    to.map_or_else(|| "nowhere".to_string(), |d| d.display().to_string())
                )))
            }
        }
        Ok(())
    }

    /// `project.status` — whether `project_root` (this daemon's own project
    /// when `None`) is trusted, and what of the project's own configuration
    /// takes effect only when it is.
    pub async fn project_status(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
    ) -> api::ProjectStatus {
        let root = project_root.unwrap_or(self.cwd());
        let settings = self.settings_for_project(Some(root), scene_id).await;
        project_trust_report(root, &settings, &self.plugins.agent_types())
    }

    /// `project.trust` — add `project_root` to, or take it off, the
    /// `project_trust.trusted` list in the global tier's
    /// `settings.local.json`, and rebuild what that project's sessions were
    /// built from. Taking it off removes only that exact entry: a glob or
    /// `project_trust.default` may still trust the project, and the status
    /// returned says so.
    ///
    /// The entry is the directory the path resolves to (`..` and symlinks
    /// followed), with glob metacharacters escaped — it names that directory
    /// and no other. A relative path is refused: it would be read against
    /// the daemon's working directory, not the caller's.
    pub async fn set_project_trust(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        trusted: bool,
    ) -> Result<api::ProjectTrustSet, PoolError> {
        use crate::watch_hub::Tier;
        use base::paths::SETTINGS_LOCAL_FILE;
        let root = project_root.unwrap_or(self.cwd()).to_path_buf();
        if !root.is_absolute() {
            return Err(PoolError::InvalidRequest(format!(
                "project_root must be an absolute path: {}",
                root.display()
            )));
        }
        let resolved = match root.canonicalize() {
            Ok(resolved) => resolved,
            Err(_) if !trusted => root.clone(),
            Err(e) => {
                return Err(PoolError::InvalidRequest(format!(
                    "project_root {}: {e}",
                    root.display()
                )))
            }
        };
        let entry = settings::trust::exact_pattern(&resolved);
        let path = self
            .bindings
            .paths()
            .global_root()
            .join(SETTINGS_LOCAL_FILE);
        edit_settings_dir(&path, move |tier| {
            let mut local = tier.read(SETTINGS_LOCAL_FILE).map_err(|e| e.to_string())?;
            let section = local
                .entry("project_trust")
                .or_insert_with(|| serde_json::json!({}));
            if !section.is_object() {
                *section = serde_json::json!({});
            }
            let list = section
                .as_object_mut()
                .expect("made an object above")
                .entry("trusted")
                .or_insert_with(|| serde_json::json!([]));
            if !list.is_array() {
                *list = serde_json::json!([]);
            }
            let list = list.as_array_mut().expect("made an array above");
            let names_root = |v: &serde_json::Value| {
                v.as_str().is_some_and(|p| {
                    p == entry
                        || (!p.contains(['*', '?', '['])
                            && settings::trust::pattern_matches(p, &resolved))
                })
            };
            if trusted {
                if !list.iter().any(names_root) {
                    list.push(serde_json::Value::String(entry.clone()));
                }
            } else {
                list.retain(|v| !names_root(v));
            }
            tier.write(SETTINGS_LOCAL_FILE, &local)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.into_pool(PoolError::Internal))?;

        let tier = Tier::Project(root.clone());
        self.forget_mcp_built_from(&tier, None).await;
        let startup_scene = self.bindings.startup_scene().await;
        if root == self.cwd() {
            self.reload_settings_from(None, &startup_scene, Some(&tier))
                .await;
        }
        if root != self.cwd() || scene_id != startup_scene {
            self.reload_settings_from(Some(&root), scene_id, Some(&tier))
                .await;
        }

        let status = self.project_status(Some(&root), scene_id).await;
        self.emit_event(api::DaemonEvent::ProjectTrustChanged {
            project_root: root.clone(),
            trust: status.trust.as_str().to_string(),
        });
        Ok(api::ProjectTrustSet {
            status,
            written_to: path.display().to_string(),
        })
    }
}
