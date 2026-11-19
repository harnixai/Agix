//! The two protocols this crate implements, as registrable factories.
//!
//! They live next to the models they construct, behind
//! [`ModelFactory`](base::interface::model_factory::ModelFactory), so "which
//! protocols exist" is decided by the crate that implements them, and a host
//! can add a third — or replace one of these — without touching the kernel.

use std::sync::Arc;

use base::interface::credentials::CredentialSource;
use base::interface::exec::Network;
use base::interface::model::Model;
use base::interface::model_factory::{ModelFactory, ModelFactoryRegistry};
use base::provider::ProviderConfig;

use crate::adapter::AnthropicModel;
use crate::client::{AnthropicClient, AuthMode, HttpAnthropicClient};

/// Anthropic's Messages API. Also what an unset `api` means.
pub struct AnthropicFactory;

impl ModelFactory for AnthropicFactory {
    fn api_type(&self) -> &str {
        "anthropic"
    }

    fn build(
        &self,
        provider_id: &str,
        cfg: &ProviderConfig,
        credentials: &dyn CredentialSource,
        network: Option<&Arc<dyn Network>>,
    ) -> Result<Arc<dyn Model>, String> {
        let api_key = credentials
            .api_key(provider_id, cfg)
            .map_err(|e| format!("provider '{provider_id}': {e}"))?;
        let auth = AuthMode::ApiKey(api_key.expose().to_string());
        let client = match cfg.base_url.as_deref() {
            Some(url) if !url.is_empty() => {
                let base = url::Url::parse(url).map_err(|e| {
                    format!("provider '{provider_id}': invalid base_url '{url}': {e}")
                })?;
                HttpAnthropicClient::with_base(auth, base)
            }
            _ => HttpAnthropicClient::new(auth),
        }
        .map_err(|e| format!("provider '{provider_id}': {e}"))?;
        let client: Arc<dyn AnthropicClient> = Arc::new(match network {
            Some(net) => client.with_network(net.clone()),
            None => client,
        });
        Ok(Arc::new(
            AnthropicModel::new(client).with_provenance(provider_id),
        ))
    }
}

/// OpenAI Chat Completions, and the gateways that only speak its shape.
pub struct OpenAIChatFactory;

impl ModelFactory for OpenAIChatFactory {
    fn api_type(&self) -> &str {
        "openai_chat"
    }

    fn build(
        &self,
        provider_id: &str,
        cfg: &ProviderConfig,
        credentials: &dyn CredentialSource,
        network: Option<&Arc<dyn Network>>,
    ) -> Result<Arc<dyn Model>, String> {
        // Required here, unlike the Anthropic factory: there is no default
        // OpenAI-compatible host to fall back to, and guessing one produces a
        // confusing auth failure at first use instead of a clear config error
        // at startup.
        let base_url = cfg
            .base_url
            .clone()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                format!("provider '{provider_id}': api 'openai_chat' requires base_url")
            })?;
        let api_key = credentials
            .api_key(provider_id, cfg)
            .map_err(|e| format!("provider '{provider_id}': {e}"))?;
        let default_model = cfg.tiers.strong.clone().unwrap_or_default();
        let model = crate::OpenAICompatibleModel::new(&base_url, api_key.expose(), default_model)
            .map_err(|e| format!("provider '{provider_id}': {e}"))?
            .with_provenance(provider_id)
            .with_vendor(cfg.vendor);
        Ok(Arc::new(match network {
            Some(net) => model.with_network(net.clone()),
            None => model,
        }))
    }
}

/// OpenAI's Responses API.
pub struct OpenAIResponsesFactory;

impl ModelFactory for OpenAIResponsesFactory {
    fn api_type(&self) -> &str {
        "openai_responses"
    }

    fn build(
        &self,
        provider_id: &str,
        cfg: &ProviderConfig,
        credentials: &dyn CredentialSource,
        network: Option<&Arc<dyn Network>>,
    ) -> Result<Arc<dyn Model>, String> {
        let base_url = cfg
            .base_url
            .clone()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "https://api.openai.com".to_string());
        let api_key = credentials
            .api_key(provider_id, cfg)
            .map_err(|e| format!("provider '{provider_id}': {e}"))?;
        let default_model = cfg.tiers.strong.clone().unwrap_or_default();
        let model = crate::OpenAIResponsesModel::new(&base_url, api_key.expose(), default_model)
            .map_err(|e| format!("provider '{provider_id}': {e}"))?
            .with_provenance(provider_id);
        Ok(Arc::new(match network {
            Some(net) => model.with_network(net.clone()),
            None => model,
        }))
    }
}

/// The protocols the engine ships with. A host adds to this rather than
/// building its own from scratch, so registering a third protocol does not
/// mean re-registering the first two.
pub fn builtin_registry() -> ModelFactoryRegistry {
    let mut registry = ModelFactoryRegistry::new();
    registry.register(Arc::new(AnthropicFactory));
    registry.register(Arc::new(OpenAIChatFactory));
    registry.register(Arc::new(OpenAIResponsesFactory));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtins_are_the_two_protocols_this_crate_implements() {
        assert_eq!(
            builtin_registry().api_types(),
            ["anthropic", "openai_chat", "openai_responses"]
        );
    }

    /// Same message the hand-written match produced, still produced at the
    /// point config is read.
    #[test]
    fn openai_chat_without_a_base_url_fails_with_the_reason() {
        let Err(err) = builtin_registry().build(
            "relay",
            &ProviderConfig {
                api: Some("openai_chat".into()),
                api_key_env: Some("PATH".into()),
                ..Default::default()
            },
        ) else {
            panic!("a config with no base_url must not build");
        };
        assert_eq!(err, "provider 'relay': api 'openai_chat' requires base_url");
    }

    #[test]
    fn a_provider_with_no_api_key_says_which_provider() {
        let Err(err) = builtin_registry().build("main", &ProviderConfig::default()) else {
            panic!("a config with no api_key must not build");
        };
        assert_eq!(err, "provider 'main': missing api_key: no `api_key_env`");
    }

    #[test]
    fn built_clients_know_which_provider_they_are() {
        let registry = builtin_registry();
        for (id, api) in [("claude", "anthropic"), ("deepseek", "openai_chat")] {
            let model = registry
                .build(
                    id,
                    &ProviderConfig {
                        api: Some(api.into()),
                        base_url: Some("https://example.invalid".into()),
                        api_key_env: Some("PATH".into()),
                        vendor: base::provider::Vendor::Deepseek,
                        ..Default::default()
                    },
                )
                .unwrap_or_else(|e| panic!("{api}: {e}"));
            assert_eq!(model.provenance().as_deref(), Some(id));
        }
    }
}
