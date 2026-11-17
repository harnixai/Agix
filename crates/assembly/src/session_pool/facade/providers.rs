//! provider 配置的读写：`get_providers`、`set_provider`（写项目层、场景层或全局层，或只
//! 试算不写，经按层写设置的核心），以及向 provider 端点问它有哪些模型的 `list_models`。

use crate::session_pool::*;
use settings::edit::{EditOp, KeyPath};

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
                        base_url: cfg.base_url.as_deref().map(|url| {
                            if include_secrets {
                                url.to_string()
                            } else {
                                scrub_url(url)
                            }
                        }),
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
        let network = platform::LocalNetwork::default().with_user_agent(self.layout().user_agent());
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
    /// it — then reload the pair's merged settings so validation and any
    /// session created after this call see the change. `dry_run` reads the
    /// same files, merges what the write would leave in them, and writes
    /// nothing; a configuration that would not route is refused, so a result
    /// means the write would work.
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
    /// would not be used. The global and scene tiers are the user's own and
    /// need no trust.
    pub async fn set_provider(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        patch: api::ProviderPatch,
    ) -> Result<api::ProviderSaved, PoolError> {
        check_provider_patch(&patch)?;
        if patch.tier == api::SettingsTier::Project {
            self.trusted_project_tier(
                project_root,
                scene_id,
                "a provider configured in its settings would not be used",
                "configure the provider in the global tier",
            )
            .await?;
        }
        let (tier, dry_run) = (patch.tier, patch.dry_run);
        let edit = TierEdit {
            dry_run,
            mcp: McpEffect::Full {
                connect_pair: false,
            },
            ..TierEdit::new(tier, provider_ops(patch))
        };
        let written = self
            .write_settings_tier(project_root, scene_id, edit)
            .await?;
        if !written.rejected.is_empty() {
            return Err(PoolError::SettingsRejected {
                untrusted_only: false,
                rejected: written.rejected,
            });
        }
        if dry_run {
            let routing = &written.routing;
            if let Some(problem) = routing.error.as_ref().or(routing.router_error.as_ref()) {
                return Err(PoolError::InvalidRequest(format!(
                    "the provider configuration this would write does not route: {problem}"
                )));
            }
        }
        Ok(api::ProviderSaved {
            tier,
            dry_run,
            written_to: written.written_to.display().to_string(),
            secrets_written_to: written.secrets_written_to.map(|p| p.display().to_string()),
            warnings: written.warnings,
            reloaded: settings_reloaded(&written.loaded.values, written.routing),
        })
    }
}

/// `patch` as changes to its tier.
fn provider_ops(patch: api::ProviderPatch) -> Vec<EditOp> {
    let entry = KeyPath::new(["providers", patch.provider_id.as_str()]);
    let mut ops = Vec::new();
    if patch.delete {
        ops.push(EditOp::Remove(entry));
    } else if let Some(config) = patch.config {
        ops.push(EditOp::Merge(entry, config));
    }
    if let Some(provider) = patch.default_provider {
        ops.push(EditOp::Set(
            KeyPath::new(["default_provider"]),
            serde_json::Value::String(provider),
        ));
    }
    if let Some(task_models) = patch.task_models {
        ops.push(EditOp::Merge(KeyPath::new(["task_models"]), task_models));
    }
    ops
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
