//! Builds the [`TaskRouter`] a pair's settings describe: one live client per
//! `providers` entry, the order of `model.order`, the tiers of `model.tasks`,
//! and the quota ledger under the state root.
//!
//! Which protocols can be built is not decided here. This assembles a
//! router from whatever is in a
//! [`ModelFactoryRegistry`](base::interface::model_factory::ModelFactoryRegistry);
//! `model::factory::builtin_registry()` is the default contents, and a host
//! that wants a third protocol adds to it rather than editing this file.

use base::interface::credentials::CredentialSource;
use base::interface::model::Model;
use base::interface::model_factory::ModelFactoryRegistry;
use base::provider::{RoutedProvider, TaskRouter};
use model::adapter::AnthropicModel;
use model::client::{AnthropicClient, NoCredentialsClient};
use settings::quota::FileQuotaLedger;
use settings::Settings;
use std::sync::Arc;

/// Build the router `settings` describe. The provider the engine supplies
/// when the settings define none talks through `builtin`; the rest are built
/// by the protocols this crate ships, with keys from `credentials`. Usage
/// against quotas is kept under `quota_dir`.
///
/// A provider whose key cannot be found is not an error here: its calls fail
/// with `no_credentials`, which hands them to the next provider in the order.
/// Anything else wrong with a provider — a tier outside its models, an `api`
/// nothing implements, a `base_url` that is not one — fails the build.
pub fn build_task_router(
    settings: &Settings,
    builtin: &Arc<dyn AnthropicClient>,
    credentials: &Arc<dyn CredentialSource>,
    layout: &base::paths::Layout,
    quota_dir: std::path::PathBuf,
) -> Result<TaskRouter, String> {
    let factories = model::factory::builtin_registry()
        .with_credentials(credentials.clone())
        .with_network(Arc::new(
            platform::LocalNetwork::default().with_user_agent(layout.user_agent()),
        ));
    build_task_router_with(settings, builtin, &factories, quota_dir)
}

/// [`build_task_router`] against a registry the caller chose — the entry
/// point for a host that registered a protocol of its own — keeping usage
/// against quotas under `quota_dir` the same way.
pub fn build_task_router_with(
    settings: &Settings,
    builtin: &Arc<dyn AnthropicClient>,
    factories: &ModelFactoryRegistry,
    quota_dir: std::path::PathBuf,
) -> Result<TaskRouter, String> {
    Ok(routed(settings, builtin, factories)?.with_quota(Arc::new(FileQuotaLedger::new(quota_dir))))
}

fn routed(
    settings: &Settings,
    builtin: &Arc<dyn AnthropicClient>,
    factories: &ModelFactoryRegistry,
) -> Result<TaskRouter, String> {
    let order = settings.routing()?;
    let mut providers = Vec::with_capacity(settings.providers.len());
    for config in &settings.providers {
        let tiers = config.resolved_tiers()?;
        let client: Arc<dyn Model> = if config.builtin {
            Arc::new(AnthropicModel::new(builtin.clone()))
        } else if let Some(missing) = factories.missing_credential(&config.id, config) {
            factories
                .clone()
                .with_credentials(Arc::new(Placeholder))
                .build(&config.id, config)?;
            Arc::new(
                AnthropicModel::new(Arc::new(NoCredentialsClient::new(format!(
                    "provider `{}` has no API key ({missing}): set its `api_key_env`, or have \
                     the host supply one through its CredentialSource; the request was not sent",
                    config.id
                ))))
                .with_provenance(&config.id),
            )
        } else {
            factories.build(&config.id, config)?
        };
        providers.push(RoutedProvider {
            config: config.clone(),
            tiers,
            client,
        });
    }
    TaskRouter::new(
        providers,
        order.iter().map(|(p, _)| p.id.clone()).collect(),
        settings.model.tasks.clone(),
    )
}

/// A key for checking that a provider builds; the client built with it is
/// thrown away.
struct Placeholder;

impl CredentialSource for Placeholder {
    fn api_key(
        &self,
        _provider_id: &str,
        _config: &base::provider::ProviderConfig,
    ) -> Result<base::interface::credentials::Secret, String> {
        Ok(base::interface::credentials::Secret::new("placeholder"))
    }
}

#[cfg(test)]
mod tests;
