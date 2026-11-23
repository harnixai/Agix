//! `team` in settings.json: whether the team tools exist for a session, and
//! how a team and its persistent members are bounded.

use serde::{Deserialize, Serialize};
use settings::keys::Order::{FalseTighter, Lower};
use settings::keys::{Key, Scope};
use settings::section::Section;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[non_exhaustive]
pub struct TeamSettings {
    /// Gates whether `TeamCreate`/`TeamList`/`TeamDelete` are registered at
    /// all for a session, and only in a scene whose `SceneSpec::supports_team`
    /// is true. Default `false`: an always-registered tool puts its
    /// `tool.prompt.md` "When to Use" section in every system prompt and its
    /// `team`/`name` input fields in every `Agent` tool schema, whether or not
    /// the operator ever intends to allow it. Flipping this only affects
    /// sessions built after the change.
    pub enabled: bool,
    /// Maximum number of `TeamCreate` sub-agents dispatched concurrently
    /// within a single stage.
    pub stage_concurrency: usize,
    /// Reclaim policy for persistent team members (`Agent` tool's
    /// `team_name`+`name` mode): total pool size across a session. Past it the
    /// oldest-idle members are stopped (never an `Active` one), just enough to
    /// be back at the cap. `0` turns persistent members off: such a call is
    /// refused; one-shot sub-agents are not affected.
    pub max_members: usize,
    /// Reclaim policy for persistent team members: how long a member may sit
    /// idle (no message since its last turn finished) before it is stopped on
    /// its own, independent of the total-count cap above.
    pub member_idle_secs: u64,
}

impl Default for TeamSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            stage_concurrency: 6,
            max_members: 20,
            member_idle_secs: 1800,
        }
    }
}

impl Section for TeamSettings {
    const KEY: &'static str = "team";
    const KEYS: &'static [Key] = &[
        Key::at("").scope(Scope::Session),
        Key::at("enabled").tighten(FalseTighter),
        Key::at("max_members").tighten(Lower),
        Key::at("member_idle_secs").tighten(Lower),
        Key::at("stage_concurrency").tighten(Lower),
    ];
}
