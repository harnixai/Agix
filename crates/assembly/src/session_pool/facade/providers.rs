//! provider 配置的读写：`get_providers`、`set_provider`（写项目层或全局层，或只试算不写），
//! provider 里密钥的拆出，以及向 provider 端点问它有哪些模型的 `list_models`。

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
                        api_key_env: cfg.api_key_env.clone(),
                        default_model: cfg.default_model.clone(),
                        models: cfg.models.clone(),
                        reasoning_passback: cfg.reasoning_passback,
                        thinking_toggle: cfg.thinking_toggle,
                        credential_present: self.bindings.credential_present(id, cfg),
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

    /// `config.listModels`: the models `provider_id`'s endpoint lists, asked
    /// with the credential its client is built with. Without one nothing is
    /// sent.
    pub async fn list_models(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        provider_id: &str,
    ) -> Result<api::ModelList, PoolError> {
        use base::event::TurnFailure;
        use model::catalog::ListModelsError;
        let settings = self
            .binding_for(Some(project_root.unwrap_or(self.cwd())), scene_id)
            .await
            .settings
            .clone();
        let Some(cfg) = settings.values.providers.get(provider_id) else {
            return Err(PoolError::InvalidRequest(format!(
                "provider `{provider_id}` is not configured"
            )));
        };
        let key = match self.bindings.api_key(provider_id, cfg) {
            Ok(key) if !key.is_empty() => key,
            Ok(_) => return Err(no_key(provider_id, "its key is empty")),
            Err(reason) => return Err(no_key(provider_id, &reason)),
        };
        let api_type = cfg.api_type.as_deref().unwrap_or("anthropic");
        let network = platform::LocalNetwork::default();
        match model::catalog::list_models(api_type, cfg.base_url.as_deref(), &key, &network).await {
            Ok(listing) => Ok(api::ModelList {
                models: listing.models,
                truncated: listing.truncated,
            }),
            Err(ListModelsError::Unsupported(why)) => Err(PoolError::ModelsUnsupported(format!(
                "provider `{provider_id}` does not list its models: {why}"
            ))),
            Err(ListModelsError::Failed(e)) => {
                let mut failure = TurnFailure::model(&e, false);
                failure.message = format!("provider `{provider_id}`: {}", failure.message);
                Err(PoolError::Engine(failure))
            }
        }
    }

    /// Merge a provider config patch (and/or `default_provider` /
    /// `task_models` patches) into `patch.tier`'s settings.json — its
    /// credential fields into the uncommitted `settings.local.json` beside
    /// it — then reload the full effective `Settings` (global → scene →
    /// project) so validation and any session created after this call see
    /// the change — see `crate::doctor` module docs for the same tri-tier
    /// layout. `dry_run` reads the same files, merges what the write would
    /// leave in them, and writes nothing.
    ///
    /// `config_patch`/`task_models_patch` are raw partial JSON objects
    /// merged key-by-key onto the existing tier file (see
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
    /// would not be used. The global tier is the user's own and needs no trust.
    pub async fn set_provider(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        patch: api::ProviderPatch,
    ) -> Result<api::ProviderSaved, PoolError> {
        use base::paths::{SETTINGS_FILE, SETTINGS_LOCAL_FILE};
        check_provider_patch(&patch)?;
        let settings_path = match patch.tier {
            api::ProviderTier::Project => {
                self.trusted_project_tier(
                    project_root,
                    scene_id,
                    "a provider configured in its settings would not be used",
                    "configure the provider in the global tier",
                )
                .await?;
                self.bindings.settings_path_of(project_root)
            }
            api::ProviderTier::Global => self.bindings.paths().global_root().join(SETTINGS_FILE),
        };
        if patch.dry_run {
            return self
                .provider_dry_run(project_root, scene_id, settings_path, patch)
                .await;
        }

        let local_path = settings_path.with_file_name(SETTINGS_LOCAL_FILE);
        let tier_name = patch.tier;
        let local_changed = edit_settings_dir(&settings_path, move |tier| {
            let mut shared = tier.read(SETTINGS_FILE).map_err(|e| e.to_string())?;
            let mut local = tier.read(SETTINGS_LOCAL_FILE).map_err(|e| e.to_string())?;
            let local_changed = apply_provider_patch(patch, &mut shared, &mut local);
            // The key first: a failure after it leaves a key nothing uses yet,
            // where the other order would leave a provider without its key.
            if local_changed {
                tier.write(SETTINGS_LOCAL_FILE, &local)
                    .map_err(|e| e.to_string())?;
            }
            tier.write(SETTINGS_FILE, &shared)
                .map_err(|e| e.to_string())?;
            Ok(local_changed)
        })
        .await?;

        let target = project_root.unwrap_or(self.cwd());
        if tier_name == api::ProviderTier::Global
            && !self.bindings.is_own_pair(Some(target), scene_id).await
        {
            self.bindings.reload_own(self.scenes.default_id()).await;
        }
        let (reloaded, report) = self
            .apply_reloaded_settings(project_root, scene_id, None)
            .await;
        Ok(api::ProviderSaved {
            tier: tier_name,
            dry_run: false,
            written_to: settings_path.display().to_string(),
            secrets_written_to: local_changed.then(|| local_path.display().to_string()),
            reloaded: settings_reloaded(&reloaded.values, report),
        })
    }

    /// `set_provider` with `dry_run`: the pair's settings merged with the two
    /// files as the write would leave them, held in memory, and the router
    /// built from them. A configuration that would not route is refused, so a
    /// result means the write would work.
    async fn provider_dry_run(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        settings_path: PathBuf,
        patch: api::ProviderPatch,
    ) -> Result<api::ProviderSaved, PoolError> {
        use base::paths::SETTINGS_LOCAL_FILE;
        let tier = patch.tier;
        let local_path = settings_path.with_file_name(SETTINGS_LOCAL_FILE);
        let (shared_path, secrets_path) = (settings_path.clone(), local_path.clone());
        let (in_memory, local_changed) = off_the_runtime(move || {
            let mut shared = settings::file::read_object(&shared_path)?;
            let mut local = settings::file::read_object(&secrets_path)?;
            let local_changed = apply_provider_patch(patch, &mut shared, &mut local);
            let mut in_memory = vec![(shared_path, serde_json::Value::Object(shared))];
            if local_changed {
                in_memory.push((secrets_path, serde_json::Value::Object(local)));
            }
            Ok::<_, settings::file::SettingsFileError>((in_memory, local_changed))
        })
        .await
        .map_err(|e| PoolError::InvalidRequest(e.to_string()))?;

        let target = project_root.unwrap_or(self.cwd());
        let (settings, routing) = self.bindings.as_if(target, scene_id, in_memory).await;
        if let Some(problem) = routing.error.or(routing.router_error) {
            return Err(PoolError::InvalidRequest(format!(
                "the provider configuration this would write does not route: {problem}"
            )));
        }
        let status = api::RoutingStatus {
            ok: true,
            warnings: routing.warnings,
            error: None,
            router_rebuilt: routing.router_rebuilt,
            router_error: None,
            mcp_connected: Vec::new(),
            mcp_failed: Vec::new(),
            mcp_connecting: Vec::new(),
        };
        Ok(api::ProviderSaved {
            tier,
            dry_run: true,
            written_to: settings_path.display().to_string(),
            secrets_written_to: local_changed.then(|| local_path.display().to_string()),
            reloaded: settings_reloaded(&settings.values, status),
        })
    }
}

/// The refusal `list_models` gives a provider with no credential, the kind a
/// turn on it would fail with.
fn no_key(provider_id: &str, reason: &str) -> PoolError {
    PoolError::Engine(base::event::TurnFailure::new(
        base::event::FailureKind::NoCredentials,
        format!(
            "provider `{provider_id}` has no API key ({reason}): set its `api_key` or \
             `api_key_env`, or have the host supply one through its CredentialSource; \
             the request was not sent"
        ),
    ))
}

fn check_provider_patch(patch: &api::ProviderPatch) -> Result<(), PoolError> {
    let invalid = |message: String| PoolError::InvalidRequest(message);
    if patch.provider_id.trim().is_empty() {
        return Err(invalid("provider_id must not be empty".into()));
    }
    if let Some(cfg) = &patch.config {
        if !cfg.is_object() {
            return Err(invalid("config must be a JSON object".into()));
        }
        serde_json::from_value::<base::provider::ProviderConfig>(cfg.clone())
            .map_err(|e| invalid(format!("invalid provider config: {e}")))?;
    }
    if let Some(tm) = &patch.task_models {
        let obj = tm
            .as_object()
            .ok_or_else(|| invalid("task_models must be a JSON object".into()))?;
        for (task, ov) in obj {
            serde_json::from_value::<base::provider::TaskModelOverride>(ov.clone())
                .map_err(|e| invalid(format!("invalid task_models.{task}: {e}")))?;
        }
    }
    Ok(())
}

/// `patch` applied to a tier's `settings.json` (`shared`) and the
/// `settings.local.json` beside it (`local`); whether `local` changed.
fn apply_provider_patch(
    patch: api::ProviderPatch,
    shared: &mut serde_json::Map<String, serde_json::Value>,
    local: &mut serde_json::Map<String, serde_json::Value>,
) -> bool {
    let local_before = local.clone();
    let provider_id = patch.provider_id.as_str();
    if patch.delete {
        for file in [&mut *shared, &mut *local] {
            if let Some(providers) = file.get_mut("providers").and_then(|v| v.as_object_mut()) {
                providers.remove(provider_id);
            }
        }
    } else if let Some(mut cfg) = patch.config {
        let secrets = take_provider_secrets(&mut cfg);
        let existing = provider_entry(shared, provider_id);
        settings::merge_json_values(existing, cfg);
        let mut moved = take_provider_secrets(existing);
        moved.extend(secrets);
        if !moved.is_empty() {
            let local_entry = provider_entry(local, provider_id);
            for (key, value) in moved {
                local_entry[key] = value;
            }
        }
    }

    if let Some(dp) = patch.default_provider {
        shared.insert(
            "default_provider".to_string(),
            serde_json::Value::String(dp),
        );
    }

    if let Some(tm) = patch.task_models {
        let task_models = shared
            .entry("task_models")
            .or_insert_with(|| serde_json::json!({}));
        if !task_models.is_object() {
            *task_models = serde_json::json!({});
        }
        settings::merge_json_values(task_models, tm);
    }
    *local != local_before
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
