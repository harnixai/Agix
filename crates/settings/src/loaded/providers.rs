//! A layer's `providers`: added to those of the layers before it, an id
//! they already define ignored; and the provider the engine supplies when
//! no layer defines one.

use base::interface::config_source::Tier;
use base::provider::ProviderConfig;

use super::{LayerFold, LoadedSettings, ProviderOrigin};
use crate::Settings;

pub(super) const PROVIDERS: &str = "providers";

/// What `model.tasks.main` is at the first provider of `model.order`; `None`
/// when the providers do not route.
fn main_model(values: &Settings) -> Option<String> {
    let routing = values.routing().ok()?;
    let (_, tiers) = routing.first()?;
    Some(tiers.get(values.model.tasks.main).to_string())
}

/// The keys a `providers` entry may hold.
const PROVIDER_KEYS: &[&str] = &[
    "id",
    "vendor",
    "api",
    "base_url",
    "api_key_env",
    "models",
    "tiers",
    "quota",
];

impl LayerFold<'_> {
    /// A layer's `providers` added to those of the layers before it; an id
    /// they already define is ignored.
    pub(super) fn take_providers(
        &mut self,
        origin: &str,
        tier: Tier,
        machine_local: bool,
        value: serde_json::Value,
    ) {
        let entries = match value {
            serde_json::Value::Array(entries) => entries,
            serde_json::Value::Object(_) => {
                let error = "`providers` is a list of entries, each with its `id`, not an \
                             object keyed by id"
                    .to_string();
                return self.ignore(tier, origin, PROVIDERS.into(), error);
            }
            _ => {
                let error = "`providers` is a list of entries".to_string();
                return self.ignore(tier, origin, PROVIDERS.into(), error);
            }
        };
        for (index, entry) in entries.into_iter().enumerate() {
            let key = format!("{PROVIDERS}[{index}]");
            let unknown: Vec<String> = entry
                .as_object()
                .into_iter()
                .flat_map(|fields| fields.keys())
                .filter(|field| !PROVIDER_KEYS.contains(&field.as_str()))
                .cloned()
                .collect();
            if !unknown.is_empty() {
                for field in unknown {
                    let key = format!("{key}.{field}");
                    let error = crate::renamed::explain(
                        &key,
                        "not a provider setting; the entry is ignored".to_string(),
                    );
                    self.ignore(tier, origin, key, error);
                }
                continue;
            }
            let provider = match serde_json::from_value::<ProviderConfig>(entry) {
                Ok(provider) if provider.id.is_empty() => {
                    self.ignore(tier, origin, key, "a provider needs an `id`".into());
                    continue;
                }
                Ok(provider) => provider,
                Err(e) => {
                    self.ignore(tier, origin, key, e.to_string());
                    continue;
                }
            };
            if let Some(first) = self.provider_origins.get(&provider.id) {
                let error = format!(
                    "provider `{}` is already defined in {}; this one is ignored",
                    provider.id, first.origin
                );
                self.ignore(tier, origin, key, error);
                continue;
            }
            self.provider_origins.insert(
                provider.id.clone(),
                ProviderOrigin {
                    tier,
                    origin: origin.to_string(),
                    machine_local,
                },
            );
            self.providers.push(provider);
        }
        self.merged_json[PROVIDERS] =
            serde_json::to_value(&self.providers).unwrap_or(serde_json::Value::Null);
    }
}

impl LoadedSettings {
    /// The built-in provider in place when no provider is defined, and the
    /// conversation's model what `model.tasks.main` is at the first provider
    /// of `model.order`. Values that went through JSON lose both; this puts
    /// them back.
    pub fn settle_providers(&mut self) {
        let values = &mut self.values;
        values.providers.retain(|p| !p.builtin);
        if values.providers.is_empty() {
            values
                .providers
                .push(ProviderConfig::builtin(&self.default_model));
        }
        values.model.model_name = main_model(values).unwrap_or_else(|| self.default_model.clone());
    }
}
