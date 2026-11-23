//! The settings sections the runner's engine reads: the same keys the
//! assembly registers, for the modules the runner wires itself.

use base::interface::script::ScriptBinding;
use settings::section::{Section, Sections};
use settings::tier_gate::ProjectKey;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
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
    Sections::none()
        .with::<ScriptBindings>()
        .with::<agents::TeamSettings>()
}
