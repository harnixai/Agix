//! `CredentialSource` — where the engine's API keys come from.
//!
//! The default source is the environment: the variable a provider's
//! `api_key_env` names, and `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` for
//! the provider the engine supplies when the settings define none. That is fine on a laptop and disqualifying for a host that
//! is not allowed to have a credential on disk at all — a service pulling
//! from a vault, an operator injecting per-request identity, anything with
//! rotation. This trait is the seam such a host replaces.
//!
//! # The value never comes back as a `String`
//!
//! Credentials leak through the boring paths — a `{:?}` on a config struct, a
//! telemetry payload built by serializing something that happened to hold one.
//! [`Secret`] exists so that cannot happen by accident: it does not implement
//! `Display`, it does not implement `Serialize`, and its `Debug` prints
//! nothing. Reading the value takes [`Secret::expose`], which is verbose on
//! purpose — a call to it is a place to look during review.

use std::collections::HashMap;

use crate::provider::ProviderConfig;

/// A credential.
///
/// Deliberately awkward to print, serialize or otherwise let out: the only
/// way to the value is [`expose`](Self::expose).
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The credential itself. Every call is a place a secret can escape;
    /// there should be few, and each should be handing it to the thing that
    /// authenticates with it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Where a provider's credential comes from.
///
/// Synchronous, and that is a real constraint: a source that has to reach a
/// network vault must resolve eagerly and cache, because this is called while
/// provider configuration is being turned into live clients, at startup. The
/// dynamic case — a token that expires mid-session and must be refreshed
/// per-request — is not this trait's job; that is what a bearer-token provider
/// is for, and one can be handed out here inside whatever the factory builds.
pub trait CredentialSource: Send + Sync {
    /// The API key for `provider_id`, or a reason there is none.
    ///
    /// The reason is phrased without naming the provider — the caller knows
    /// which provider it asked about and prefixes it, so the message reads the
    /// same wherever it surfaces.
    fn api_key(&self, provider_id: &str, config: &ProviderConfig) -> Result<Secret, String>;
}

/// What the provider entry in `settings.json` says: the environment variable
/// its `api_key_env` names — `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`
/// for the built-in provider. The default source.
pub struct ConfigCredentials;

impl CredentialSource for ConfigCredentials {
    fn api_key(&self, provider_id: &str, config: &ProviderConfig) -> Result<Secret, String> {
        match config.api_key_env.as_deref().filter(|n| !n.is_empty()) {
            Some(name) => EnvCredentials::new([name]).api_key(provider_id, config),
            None if config.builtin => EnvCredentials::anthropic().api_key(provider_id, config),
            None => Err("missing api_key: no `api_key_env`".to_string()),
        }
    }
}

/// The provider entry's own answer ([`ConfigCredentials`]) first, then a
/// host-supplied source for whatever the entry leaves unset.
pub struct ConfigThen(pub std::sync::Arc<dyn CredentialSource>);

impl CredentialSource for ConfigThen {
    fn api_key(&self, provider_id: &str, config: &ProviderConfig) -> Result<Secret, String> {
        ConfigCredentials
            .api_key(provider_id, config)
            .or_else(|own| {
                self.0
                    .api_key(provider_id, config)
                    .map_err(|host| format!("{own}; host: {host}"))
            })
    }
}

/// Reads the key from the environment, trying each name in turn.
///
/// The other half of what the engine already did — the daemon's own client is
/// built this way — and the smallest example of a source that keeps nothing on
/// disk.
pub struct EnvCredentials {
    names: Vec<String>,
}

impl EnvCredentials {
    /// Also holds every name back from the environment child processes
    /// inherit ([`hold_back_credential`](crate::interface::exec::env::hold_back_credential)).
    pub fn new(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let names: Vec<String> = names.into_iter().map(Into::into).collect();
        for name in &names {
            crate::interface::exec::env::hold_back_credential(name);
        }
        Self { names }
    }

    /// The names the engine has always looked at, in the order it looked.
    pub fn anthropic() -> Self {
        Self::new(["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"])
    }
}

impl CredentialSource for EnvCredentials {
    #[expect(
        clippy::disallowed_methods,
        reason = "reading the key from the process environment is what this credential source is"
    )]
    fn api_key(&self, _provider_id: &str, _config: &ProviderConfig) -> Result<Secret, String> {
        for name in &self.names {
            if let Ok(v) = std::env::var(name) {
                if !v.is_empty() {
                    return Ok(Secret::new(v));
                }
            }
        }
        Err(format!(
            "missing api_key: none of {} is set",
            self.names.join(", ")
        ))
    }
}

/// Answers from a map given up front.
///
/// For tests, and for a host that resolved its credentials somewhere else
/// entirely — a vault client, a secrets mount, an operator's own prompt — and
/// just needs to hand the answers over.
pub struct StaticCredentials {
    by_provider: HashMap<String, Secret>,
}

impl StaticCredentials {
    pub fn new(entries: impl IntoIterator<Item = (String, Secret)>) -> Self {
        Self {
            by_provider: entries.into_iter().collect(),
        }
    }
}

impl CredentialSource for StaticCredentials {
    fn api_key(&self, provider_id: &str, _config: &ProviderConfig) -> Result<Secret, String> {
        self.by_provider
            .get(provider_id)
            .cloned()
            .ok_or_else(|| "missing api_key".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANARY: &str = "sk-do-not-print-me";

    #[test]
    fn a_secret_prints_as_nothing() {
        let s = Secret::new(CANARY);
        assert!(!format!("{s:?}").contains(CANARY));
        assert!(!format!("{:?}", Some(s.clone())).contains(CANARY));
        assert!(!format!("{:?}", vec![s.clone()]).contains(CANARY));
        assert_eq!(s.expose(), CANARY);
    }

    #[test]
    fn the_config_source_reports_a_missing_key() {
        let err = ConfigCredentials
            .api_key("main", &ProviderConfig::default())
            .unwrap_err();
        assert!(err.starts_with("missing api_key"), "{err}");
    }

    #[test]
    fn a_source_that_keeps_nothing_on_disk_can_answer_instead() {
        let source = StaticCredentials::new([("vault-backed".to_string(), Secret::new(CANARY))]);
        let got = source
            .api_key("vault-backed", &ProviderConfig::default())
            .expect("the map has this provider");
        assert_eq!(got.expose(), CANARY);
        assert_eq!(
            source
                .api_key("someone-else", &ProviderConfig::default())
                .unwrap_err(),
            "missing api_key"
        );
    }

    /// `PATH` stands in for a key variable: always set, so the test needs no
    /// `set_var` racing other threads.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the expected key is whatever the environment holds"
    )]
    fn a_provider_can_name_the_variable_its_key_is_in() {
        let cfg = ProviderConfig {
            api_key_env: Some("PATH".into()),
            ..Default::default()
        };
        let got = ConfigCredentials.api_key("p", &cfg).unwrap();
        assert_eq!(got.expose(), std::env::var("PATH").unwrap());

        let unset = ProviderConfig {
            api_key_env: Some("AGIX_TEST_CREDENTIAL_THAT_IS_NOT_SET".into()),
            ..Default::default()
        };
        let err = ConfigCredentials.api_key("p", &unset).unwrap_err();
        assert!(
            err.contains("AGIX_TEST_CREDENTIAL_THAT_IS_NOT_SET"),
            "{err}"
        );
    }

    #[test]
    fn the_host_answers_only_what_the_entry_leaves_unset() {
        let host = std::sync::Arc::new(StaticCredentials::new([(
            "vault-backed".to_string(),
            Secret::new(CANARY),
        )]));
        let chain = ConfigThen(host);
        assert_eq!(
            chain
                .api_key("vault-backed", &ProviderConfig::default())
                .unwrap()
                .expose(),
            CANARY
        );
        let named = ProviderConfig {
            api_key_env: Some("PATH".into()),
            ..Default::default()
        };
        assert_ne!(
            chain.api_key("vault-backed", &named).unwrap().expose(),
            CANARY,
            "the variable the entry names wins over the host"
        );
        let err = chain
            .api_key("unknown", &ProviderConfig::default())
            .unwrap_err();
        assert!(err.starts_with("missing api_key"), "{err}");
    }

    #[test]
    fn the_env_source_names_what_it_looked_for() {
        let source = EnvCredentials::new(["AGIX_TEST_CREDENTIAL_THAT_IS_NOT_SET"]);
        let err = source
            .api_key("main", &ProviderConfig::default())
            .unwrap_err();
        assert!(
            err.contains("AGIX_TEST_CREDENTIAL_THAT_IS_NOT_SET"),
            "{err}"
        );
    }
}
