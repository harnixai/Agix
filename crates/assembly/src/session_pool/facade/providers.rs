//! provider 配置的读写：`get_providers`、`set_provider`，以及 provider 里密钥的拆出。

use crate::session_pool::*;

impl SessionPool {
    /// Read back the current effective provider/task_models configuration —
    /// the read-side counterpart to `set_provider`. `api_key` is redacted
    /// (`***<last 4 chars>`) unless `include_secrets` is `true` — default
    /// to redacted so a caller that just wants to know *which* providers
    /// are configured doesn't accidentally end up with plaintext keys in
    /// its own logs/UI. See the trust-boundary note on `config.setProvider`
    /// in `crates/assembly/src/server/` — `include_secrets: true` is still gated
    /// by nothing more than "can reach this socket", same as every other
    /// method here.
    pub async fn get_providers(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        include_secrets: bool,
    ) -> api::ProviderConfigView {
        let settings = self
            .binding_for(Some(project_root.unwrap_or(self.cwd())), scene_id)
            .await
            .settings
            .clone();
        let providers = settings
            .values
            .providers
            .iter()
            .map(|(id, cfg)| {
                let api_key = cfg.api_key.as_ref().map(|k| {
                    if include_secrets {
                        k.clone()
                    } else {
                        redact_secret(k)
                    }
                });
                (
                    id.clone(),
                    api::ProviderView {
                        api_type: cfg.api_type.clone(),
                        base_url: cfg.base_url.clone(),
                        api_key,
                        default_model: cfg.default_model.clone(),
                        models: cfg.models.clone(),
                    },
                )
            })
            .collect();
        api::ProviderConfigView {
            providers,
            default_provider: settings.values.default_provider.clone(),
            task_models: settings.values.task_models.clone().into_iter().collect(),
        }
    }

    /// Merge a provider config patch (and/or `default_provider` /
    /// `task_models` patches) into the project-tier settings.json — its
    /// credential fields into the uncommitted `settings.local.json` — then
    /// reload the full effective `Settings` (global → scene → project) so
    /// validation and any session created after this call see the change —
    /// see `crate::doctor` module docs for the same tri-tier layout.
    ///
    /// `config_patch`/`task_models_patch` are raw partial JSON objects
    /// merged key-by-key onto the existing project-tier file (see
    /// `settings::merge_json_values`) — deliberately not
    /// typed `ProviderConfig` structs, since deserializing a partial patch
    /// into a `#[serde(default)]` struct would silently null out every field
    /// the caller didn't mention, clobbering unrelated existing values.
    /// `delete: true` removes the `providers.<id>` entry outright rather
    /// than merging; any `task_models` entry that referenced it degrades to
    /// `default_provider` automatically the next time routing is resolved
    /// (see `base::provider::resolve_task_models`'s soft-fallback) — no
    /// separate "downgrade" write is needed.
    ///
    /// Refused with `PROJECT_UNTRUSTED`, writing nothing, when the target is
    /// a project's own tier and that project is not trusted: a provider there
    /// would not be used.
    pub async fn set_provider(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        patch: api::ProviderPatch,
    ) -> Result<api::ProviderSaved, PoolError> {
        use base::paths::{SETTINGS_FILE, SETTINGS_LOCAL_FILE};
        let api::ProviderPatch {
            provider_id,
            delete,
            config: config_patch,
            default_provider,
            task_models: task_models_patch,
        } = patch;
        let provider_id = provider_id.as_str();
        let invalid = |message: String| PoolError::InvalidRequest(message);
        if provider_id.trim().is_empty() {
            return Err(invalid("provider_id must not be empty".into()));
        }

        if let Some(ref cfg) = config_patch {
            if !cfg.is_object() {
                return Err(invalid("config must be a JSON object".into()));
            }
            serde_json::from_value::<base::provider::ProviderConfig>(cfg.clone())
                .map_err(|e| invalid(format!("invalid provider config: {e}")))?;
        }
        if let Some(ref tm) = task_models_patch {
            let obj = tm
                .as_object()
                .ok_or_else(|| invalid("task_models must be a JSON object".into()))?;
            for (task, ov) in obj {
                serde_json::from_value::<base::provider::TaskModelOverride>(ov.clone())
                    .map_err(|e| invalid(format!("invalid task_models.{task}: {e}")))?;
            }
        }
        self.trusted_project_tier(
            project_root,
            scene_id,
            "a provider configured in its settings would not be used",
            "configure the provider in the global tier",
        )
        .await?;

        let settings_path = self.bindings.settings_path_of(project_root);
        let local_path = settings_path.with_file_name(SETTINGS_LOCAL_FILE);
        let provider_id = provider_id.to_string();
        let local_changed = edit_settings_dir(&settings_path, move |tier| {
            let mut project_json = tier.read(SETTINGS_FILE).map_err(|e| e.to_string())?;
            let mut local_json = tier.read(SETTINGS_LOCAL_FILE).map_err(|e| e.to_string())?;
            let local_before = local_json.clone();
            let obj = &mut project_json;
            let local = &mut local_json;

            if delete {
                for tier in [&mut *obj, &mut *local] {
                    if let Some(providers) =
                        tier.get_mut("providers").and_then(|v| v.as_object_mut())
                    {
                        providers.remove(&provider_id);
                    }
                }
            } else if let Some(mut cfg) = config_patch {
                let secrets = take_provider_secrets(&mut cfg);
                let existing = provider_entry(obj, &provider_id);
                settings::merge_json_values(existing, cfg);
                let mut moved = take_provider_secrets(existing);
                moved.extend(secrets);
                if !moved.is_empty() {
                    let local_entry = provider_entry(local, &provider_id);
                    for (key, value) in moved {
                        local_entry[key] = value;
                    }
                }
            }

            if let Some(dp) = default_provider {
                obj.insert(
                    "default_provider".to_string(),
                    serde_json::Value::String(dp),
                );
            }

            if let Some(tm) = task_models_patch {
                let task_models = obj
                    .entry("task_models")
                    .or_insert_with(|| serde_json::json!({}));
                if !task_models.is_object() {
                    *task_models = serde_json::json!({});
                }
                settings::merge_json_values(task_models, tm);
            }

            // The key first: a failure after it leaves a key nothing uses yet,
            // where the other order would leave a provider without its key.
            let local_changed = local_json != local_before;
            if local_changed {
                tier.write(SETTINGS_LOCAL_FILE, &local_json)
                    .map_err(|e| e.to_string())?;
            }
            tier.write(SETTINGS_FILE, &project_json)
                .map_err(|e| e.to_string())?;
            Ok(local_changed)
        })
        .await
        .map_err(invalid)?;
        let secrets_written_to = local_changed.then(|| local_path.display().to_string());

        let (reloaded, report) = self
            .apply_reloaded_settings(project_root, scene_id, None)
            .await;
        Ok(api::ProviderSaved {
            written_to: settings_path.display().to_string(),
            secrets_written_to,
            reloaded: settings_reloaded(&reloaded.values, report),
        })
    }
}

/// The provider fields that are credentials, which `config.setProvider` keeps
/// out of the committed `settings.json`.
pub(in crate::session_pool) const PROVIDER_SECRET_KEYS: &[&str] = &["api_key"];

/// Remove the credential fields from one provider's JSON, returning them.
pub(in crate::session_pool) fn take_provider_secrets(
    provider: &mut serde_json::Value,
) -> Vec<(String, serde_json::Value)> {
    let Some(fields) = provider.as_object_mut() else {
        return Vec::new();
    };
    PROVIDER_SECRET_KEYS
        .iter()
        .filter_map(|key| fields.remove(*key).map(|value| (key.to_string(), value)))
        .collect()
}

/// `providers.<id>` of a settings object, created as `{}` when it is missing
/// or not an object.
pub(in crate::session_pool) fn provider_entry<'a>(
    settings: &'a mut serde_json::Map<String, serde_json::Value>,
    provider_id: &str,
) -> &'a mut serde_json::Value {
    let providers = settings
        .entry("providers")
        .or_insert_with(|| serde_json::json!({}));
    if !providers.is_object() {
        *providers = serde_json::json!({});
    }
    let entry = providers
        .as_object_mut()
        .expect("just normalized to an object")
        .entry(provider_id.to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !entry.is_object() {
        *entry = serde_json::json!({});
    }
    entry
}
