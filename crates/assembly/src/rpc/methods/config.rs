//! `config.*` 的 params，以及配置类方法共用的定位参数 [`ConfigTarget`]。

use crate::session_pool::api::{ConfigTierName, SettingsTier};
use serde::{Deserialize, Deserializer};
use std::path::PathBuf;

/// The `(scene, project_root)` pair a configuration method acts on. Omitted
/// means this daemon's own; a named `project_root` reaches another project
/// the daemon serves. `null` is refused: these methods act on the *project*
/// tier, and a session with no project has none.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct ConfigTarget {
    #[serde(default)]
    pub scene: Option<String>,
    #[serde(default, deserialize_with = "project_path")]
    #[schemars(with = "Option<String>")]
    pub project_root: Option<PathBuf>,
}

fn project_path<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PathBuf>, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(path) => Ok(Some(PathBuf::from(path))),
        _ => Err(serde::de::Error::custom(
            "project_root must be a path: a session with no project has no project tier",
        )),
    }
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ConfigGetParams {
    pub scene: String,
    /// Absent and `null` both mean the scene with no project.
    #[serde(default)]
    pub project_root: Option<PathBuf>,
    #[serde(default)]
    pub tier: ConfigTierName,
    #[serde(default)]
    pub include_secrets: bool,
}

/// A key, or a block, of a `(scene, project_root)` pair or of a session.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ConfigExplainParams {
    /// Dotted; a block (`turn`) explains each key in it.
    pub key: String,
    /// The pair is the session's: `scene` and `project_root` are left out.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub target: ConfigTarget,
    #[serde(default)]
    pub include_secrets: bool,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ConfigSetParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    pub tier: SettingsTier,
    /// Dotted settings key to the value it takes in `tier`; `null` removes
    /// the key from the tier.
    pub changes: serde_json::Map<String, serde_json::Value>,
    /// Work out what the write would do, and write nothing.
    #[serde(default)]
    pub dry_run: bool,
    /// The tier's `revision` as `config.get` read it; the write is refused
    /// when the tier has changed since.
    #[serde(default)]
    pub if_revision: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ConfigGetProviderParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    /// Only this provider; every one when absent.
    #[serde(default)]
    pub provider_id: Option<String>,
    /// `base_url` as written, credentials and query included.
    #[serde(default)]
    pub include_secrets: bool,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ConfigListModelsParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    pub provider: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ConfigSetProviderParams {
    #[serde(flatten)]
    pub target: ConfigTarget,
    /// Where a provider not defined yet is written. One already defined is
    /// written in the file that defines it.
    #[serde(default)]
    pub tier: SettingsTier,
    /// Work out what the write would do, and write nothing.
    #[serde(default)]
    pub dry_run: bool,
    pub provider_id: String,
    /// A partial patch merged key by key onto the `providers` entry with this
    /// id.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    /// Remove the provider instead of merging.
    #[serde(default)]
    pub delete: bool,
}
