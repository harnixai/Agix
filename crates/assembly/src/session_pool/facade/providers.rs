//! provider 配置的读写：`get_providers`（含配额用量）、`set_provider`（按 id 改 `providers`
//! 列表项，写回定义它的那个文件，新的写到指定层，或只试算不写，经按层写设置的核心），以及
//! 向 provider 端点问它有哪些模型的 `list_models`。

use crate::session_pool::*;
use settings::edit::{EditOp, KeyPath};

impl SessionPool {
    /// The pair's providers as configured — every one, or only `provider_id`
    /// — with each one's usage against its quota; and the order and tiers
    /// calls go by. A `base_url`'s credentials and query are scrubbed unless
    /// `include_secrets` is `true`; no key is ever in the answer.
    pub async fn get_providers(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        provider_id: Option<&str>,
        include_secrets: bool,
    ) -> Result<api::ProviderConfigView, PoolError> {
        let binding = self
            .binding_for(Some(project_root.unwrap_or(self.cwd())), scene_id)
            .await;
        let settings = binding.settings.clone();
        if let Some(id) = provider_id {
            if settings.values.provider(id).is_none() {
                return Err(PoolError::InvalidRequest(format!(
                    "provider `{id}` is not configured"
                )));
            }
        }
        let ledger = settings::quota::FileQuotaLedger::new(
            settings
                .paths
                .global_data_dir
                .join(base::paths::entry::QUOTA),
        );
        let providers = settings
            .values
            .providers
            .iter()
            .filter(|cfg| provider_id.is_none_or(|id| cfg.id == id))
            .map(|cfg| api::ProviderView {
                id: cfg.id.clone(),
                vendor: cfg.vendor,
                api: cfg.api().to_string(),
                base_url: cfg.base_url.as_deref().map(|url| match include_secrets {
                    true => url.to_string(),
                    false => scrub_url(url),
                }),
                api_key_env: cfg.api_key_env.clone(),
                models: cfg.models.clone(),
                tiers: cfg.tiers.clone(),
                quota: cfg.quota.clone(),
                usage: (!cfg.quota.is_unlimited())
                    .then(|| base::provider::QuotaLedger::status(&ledger, &cfg.id, &cfg.quota)),
                credential_present: self.bindings.credential_present(&cfg.id, cfg),
                builtin: cfg.builtin,
                defined_in: settings
                    .provider_origins
                    .get(&cfg.id)
                    .map(|origin| origin.origin.clone()),
            })
            .collect();
        Ok(api::ProviderConfigView {
            providers,
            order: binding.task_router.order().to_vec(),
            tasks: settings.values.model.tasks.clone(),
        })
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
        let Some(cfg) = settings.values.provider(provider_id) else {
            return Err(PoolError::InvalidRequest(format!(
                "provider `{provider_id}` is not configured"
            )));
        };
        let key = match self.bindings.api_key(provider_id, cfg) {
            Ok(key) if !key.is_empty() => key,
            Ok(_) => return Err(no_key(provider_id, "its key is empty")),
            Err(reason) => return Err(no_key(provider_id, &reason)),
        };
        let network = platform::LocalNetwork::default().with_user_agent(self.layout().user_agent());
        match model::catalog::list_models(cfg.api(), cfg.base_url.as_deref(), &key, &network).await
        {
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

    /// Merge `patch.config` into the `providers` entry `patch.provider_id`
    /// in the settings file that defines it — a new one goes to `patch.tier`'s
    /// `settings.json` — or with `delete` remove it from that tier, then
    /// reload the pair's merged settings so validation and any session
    /// created after this call see the change. `dry_run` reads the same
    /// files, merges what the write would leave in them, and writes nothing;
    /// a configuration that would not route is refused, so a result means the
    /// write would work.
    ///
    /// `config` is a raw partial JSON object merged key by key onto the entry
    /// — deliberately not a typed `ProviderConfig`, whose defaults would
    /// clobber every field the caller did not mention.
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
        let defined = self
            .binding_for(Some(project_root.unwrap_or(self.cwd())), scene_id)
            .await
            .settings
            .provider_origins
            .get(&patch.provider_id)
            .map(|origin| origin.tier);
        let patch = api::ProviderPatch {
            tier: match defined {
                Some(base::interface::config_source::Tier::Global) => api::SettingsTier::Global,
                Some(base::interface::config_source::Tier::Scene) => api::SettingsTier::Scene,
                Some(base::interface::config_source::Tier::Project) => api::SettingsTier::Project,
                _ => patch.tier,
            },
            ..patch
        };
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
            warnings: written.warnings,
            reloaded: self
                .settings_reloaded(&written.loaded.values, written.routing)
                .await,
        })
    }
}

/// `patch` as changes to its tier.
fn provider_ops(patch: api::ProviderPatch) -> Vec<EditOp> {
    let list = KeyPath::new(["providers"]);
    let id = patch.provider_id;
    if patch.delete {
        return vec![EditOp::RemoveEntry(list, id)];
    }
    let config = patch
        .config
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
    vec![EditOp::MergeEntry(list, id, config)]
}

/// The refusal `list_models` gives a provider with no credential, the kind a
/// turn on it would fail with.
fn no_key(provider_id: &str, reason: &str) -> PoolError {
    PoolError::Engine(base::event::TurnFailure::new(
        base::event::FailureKind::NoCredentials,
        format!(
            "provider `{provider_id}` has no API key ({reason}): set its `api_key_env`, or \
             have the host supply one through its CredentialSource; the request was not \
             sent"
        ),
    ))
}

fn check_provider_patch(patch: &api::ProviderPatch) -> Result<(), PoolError> {
    let invalid = |message: String| PoolError::InvalidRequest(message);
    if patch.provider_id.trim().is_empty() {
        return Err(invalid("provider_id must not be empty".into()));
    }
    if let Some(cfg) = &patch.config {
        let Some(fields) = cfg.as_object() else {
            return Err(invalid("config must be a JSON object".into()));
        };
        if let Some(id) = fields
            .get("id")
            .filter(|id| id.as_str() != Some(&patch.provider_id))
        {
            return Err(invalid(format!(
                "config.id {id} is not provider_id `{}`; a provider's id is not changed this way",
                patch.provider_id
            )));
        }
        let mut entry = cfg.clone();
        entry["id"] = serde_json::Value::String(patch.provider_id.clone());
        if let Err(e) = serde_json::from_value::<base::provider::ProviderConfig>(entry) {
            let message = match fields.keys().find_map(|key| {
                settings::renamed::renamed_to(&format!("providers[].{key}")).map(|new| (key, new))
            }) {
                Some((key, new)) => {
                    format!("invalid provider config: `{key}` was renamed to `{new}`")
                }
                None => format!("invalid provider config: {e}"),
            };
            return Err(invalid(message));
        }
    }
    Ok(())
}
