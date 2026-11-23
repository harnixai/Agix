//! The settings keys the modules own, registered once: every load of settings
//! passes [`all`], so the keys are known to the loader, the trust gate, the
//! unknown-key report and the published schema alike.

use base::interface::script::ScriptBinding;
use settings::section::{Section, Sections};
use settings::trust::ProjectKey;

/// Scripts bound to extension points — see [`ScriptBinding`]. Empty by
/// default, and inert in a build without a script engine.
#[derive(
    Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
#[schemars(inline)]
pub struct ScriptBindings(pub Vec<ScriptBinding>);

impl Section for ScriptBindings {
    const KEY: &'static str = "scripts";
    const PROJECT_TIER: &'static [(&'static str, ProjectKey)] = &[("", ProjectKey::Withheld)];
    const SCOPE: settings::scope::Scope = settings::scope::Scope::Pair;
}

impl ScriptBindings {
    pub fn of(settings: &settings::Settings) -> Vec<ScriptBinding> {
        settings.section::<Self>().0
    }
}

pub fn all() -> Sections {
    let sections = Sections::none()
        .with::<crate::plugins::PluginsConfig>()
        .with::<runtime::checkpoint_settings::CheckpointSettings>()
        .with::<ScriptBindings>();
    #[cfg(feature = "agents")]
    let sections = sections.with::<agents::TeamSettings>();
    #[cfg(not(feature = "agents"))]
    let sections = sections.with_spec(settings::section::SectionSpec::inert("team"));
    sections
}
