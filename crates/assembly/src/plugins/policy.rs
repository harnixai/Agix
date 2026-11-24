//! `plugins` in settings.json: whether packages may load at all, and which.

/// Runtime control over the plugin subsystem, for a build that has it
/// compiled in.
///
/// The compile-time feature decides whether plugins *can* exist; this
/// decides whether they may. A deployment that wants none should prefer the
/// feature, since that leaves nothing to switch back on — this is for the
/// case where the same binary serves several deployments.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[non_exhaustive]
pub struct PluginsConfig {
    /// Load installed plugins at all.
    pub enabled: bool,
    /// When non-empty, only these plugin names load. An allow-list, so a
    /// name that is not on it is refused rather than merely unmentioned.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Whether `plugin.install` turns a package on once it is installed.
    /// Off by default: the host reads the disclosure the install returns and
    /// calls `plugin.enable` itself. `true` enables it as part of the install.
    pub enable_on_install: bool,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow: Vec::new(),
            enable_on_install: false,
        }
    }
}

impl PluginsConfig {
    /// May `name` load under this configuration?
    pub fn permits(&self, name: &str) -> bool {
        self.enabled && (self.allow.is_empty() || self.allow.iter().any(|a| a == name))
    }
}

impl settings::section::Section for PluginsConfig {
    const KEY: &'static str = "plugins";
    const KEYS: &'static [settings::keys::Key] = {
        use settings::keys::Order::{FalseTighter, SubsetEmptyIsAll};
        use settings::keys::{Key, Scope};
        &[
            Key::at("").scope(Scope::Process),
            Key::at("allow")
                .scope(Scope::Session)
                .tighten(SubsetEmptyIsAll),
            Key::at("enable_on_install").tighten(FalseTighter),
            Key::at("enabled")
                .scope(Scope::Session)
                .tighten(FalseTighter),
        ]
    };
}

#[cfg(test)]
mod tests {
    use super::PluginsConfig;

    /// A build with plugins compiled in serves them unless told otherwise —
    /// the compile-time feature is where "never" belongs.
    #[test]
    fn the_default_permits_everything() {
        let c = PluginsConfig::default();
        assert!(c.enabled);
        assert!(c.permits("anything"));
    }

    #[test]
    fn disabling_refuses_every_plugin() {
        let c = PluginsConfig {
            enabled: false,
            allow: vec!["github-tools".into()],
            ..Default::default()
        };
        assert!(
            !c.permits("github-tools"),
            "an allow-list does not override being switched off"
        );
    }

    /// An allow-list is an allow-list: a name that is not on it is refused,
    /// not merely unmentioned.
    #[test]
    fn a_non_empty_allow_list_excludes_everything_else() {
        let c = PluginsConfig {
            enabled: true,
            allow: vec!["github-tools".into()],
            ..Default::default()
        };
        assert!(c.permits("github-tools"));
        assert!(!c.permits("something-else"));
    }
}
