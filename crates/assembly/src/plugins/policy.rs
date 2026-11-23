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
    /// With `false` the host reads the disclosure the install returns and
    /// calls `plugin.enable` itself. Defaults to `true`, which a later 0.3.x
    /// release is planned to change to `false`.
    pub enable_on_install: bool,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow: Vec::new(),
            enable_on_install: true,
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
    const PROJECT_TIER: &'static [(&'static str, settings::trust::ProjectKey)] = {
        use settings::trust::Order::{FalseTighter, SubsetEmptyIsAll};
        use settings::trust::ProjectKey::Tighten;
        &[
            ("allow", Tighten(SubsetEmptyIsAll)),
            ("enable_on_install", Tighten(FalseTighter)),
            ("enabled", Tighten(FalseTighter)),
        ]
    };
    const SCOPE: settings::scope::Scope = settings::scope::Scope::Process;
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
