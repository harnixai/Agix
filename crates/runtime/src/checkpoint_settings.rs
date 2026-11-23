//! `checkpoints` 设置段：文件快照记不记、单个文件和整个会话的快照各占多少字节。项目层只能
//! 收紧——关掉、调小；调大或放开要用户层。

use serde::{Deserialize, Serialize};
use settings::section::Section;
use settings::tier_gate::Order::{FalseTighter, Lower};
use settings::tier_gate::ProjectKey::{self, Tighten};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct CheckpointSettings {
    /// `false`: the file tools write without recording what they overwrite,
    /// and the session cannot rewind files.
    pub enabled: bool,
    /// A file larger than this is hashed but its earlier contents are not kept.
    pub max_bytes_per_file: u64,
    /// Over this many bytes of kept copies per session, the oldest are
    /// deleted and a `pruned` line is written to the transcript.
    pub max_total_bytes: u64,
}

impl Default for CheckpointSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes_per_file: 16 * 1024 * 1024,
            max_total_bytes: 512 * 1024 * 1024,
        }
    }
}

impl Section for CheckpointSettings {
    const KEY: &'static str = "checkpoints";
    const PROJECT_TIER: &'static [(&'static str, ProjectKey)] = &[
        ("enabled", Tighten(FalseTighter)),
        ("max_bytes_per_file", Tighten(Lower)),
        ("max_total_bytes", Tighten(Lower)),
    ];
    const SCOPE: settings::scope::Scope = settings::scope::Scope::Session;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use settings::section::Sections;

    #[test]
    fn an_untrusted_project_may_shrink_the_quota_or_turn_it_off_but_not_raise_it() {
        let sections = Sections::none().with::<CheckpointSettings>();
        let mut lower = serde_json::to_value(settings::Settings::defaults_for("m")).unwrap();
        lower["checkpoints"] = json!({
            "enabled": true,
            "max_bytes_per_file": 1000,
            "max_total_bytes": 5000
        });
        let mut layer = json!({"checkpoints": {
            "enabled": false,
            "max_bytes_per_file": 500,
            "max_total_bytes": 9000
        }})
        .as_object()
        .unwrap()
        .clone();
        let found = settings::tier_gate::gate_layer(&mut layer, &lower, true, &sections);
        assert_eq!(found, ["checkpoints.max_total_bytes"]);
        assert_eq!(
            layer["checkpoints"],
            json!({"enabled": false, "max_bytes_per_file": 500})
        );
    }
}
