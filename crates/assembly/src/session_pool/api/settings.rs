//! 设置、provider、项目信任与配置导入的领域结果类型。设置文件的内容本身
//! （`ConfigTier.settings`、`managed`）是不透明的 JSON，保持 `Value`。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which settings layer `config.get` reads.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ConfigTierName {
    Global,
    Scene,
    Project,
    /// The settings given on the command line (`agixd --set`) or by the
    /// embedder (`EmbedConfig::settings`).
    Cli,
    #[default]
    Effective,
}

/// Which settings layer a write goes to.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SettingsTier {
    /// The state root's own, under every scene and project.
    Global,
    /// The scene's own directory, under every project of that scene.
    Scene,
    #[default]
    Project,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConfigTier {
    pub scene: String,
    pub project_root: Option<String>,
    pub tier: ConfigTierName,
    /// The tier's settings file as it sits on disk (`null` when it has none),
    /// the `cli` layer (`null` without one), or the merged result for
    /// `effective`.
    pub settings: serde_json::Value,
    /// The tier's `settings.local.json` (`null` when it has none); absent for
    /// `cli` and `effective`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<serde_json::Value>,
    /// The tier's two files as they are now, for `config.set`'s
    /// `if_revision`; absent for `cli` and `effective`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// The deployment's managed layer; only `effective` has one.
    pub managed: serde_json::Value,
}

/// What `config.set` is asked to write.
#[derive(Debug, Clone)]
pub struct SettingsChange {
    pub tier: SettingsTier,
    /// Dotted key to its new value; `null` removes the key from the tier.
    pub changes: serde_json::Map<String, serde_json::Value>,
    pub dry_run: bool,
    pub if_revision: Option<String>,
}

/// Why one key of a settings write is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RejectKind {
    /// Not a dotted key, or overlapping another key of the same write.
    BadPath,
    UnknownKey,
    InvalidValue,
    /// A project that is not trusted may only add and tighten.
    RequiresTrust,
    /// Read from the global tier only.
    NotAtTier,
    /// Written by another method, named in `method`.
    UseMethod,
    /// Not written over RPC at all.
    NotWritable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SettingRejected {
    pub key: String,
    pub kind: RejectKind,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

impl SettingRejected {
    pub fn new(key: impl Into<String>, kind: RejectKind, reason: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            kind,
            reason: reason.into(),
            method: None,
        }
    }
}

pub(super) fn rejected_summary(rejected: &[SettingRejected]) -> String {
    rejected
        .iter()
        .map(|r| format!("`{}`: {}", r.key, r.reason))
        .collect::<Vec<_>>()
        .join("; ")
}

/// What `config.set` wrote, or with `dry_run` would write.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SettingsWritten {
    pub tier: SettingsTier,
    pub dry_run: bool,
    pub written_to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets_written_to: Option<String>,
    /// The keys written; empty when something was refused.
    pub changed: Vec<String>,
    /// Only a `dry_run` reports refusals here; a write refuses with an error.
    pub rejected: Vec<SettingRejected>,
    /// Each key's value in the pair's merged settings afterwards, credentials
    /// redacted.
    pub effective: serde_json::Map<String, serde_json::Value>,
    /// The keys written whose value in the pair's merged settings is not what
    /// was written: a higher tier, the host or the managed layer sets them.
    pub overridden: Vec<String>,
    /// The tier's revision after the write (now, for `dry_run`).
    pub revision: String,
    /// The keys written whose value in `settings.json` carries a credential,
    /// each said without it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(flatten)]
    pub reloaded: SettingsReloaded,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderView {
    pub id: String,
    pub vendor: base::provider::Vendor,
    /// `anthropic` when the entry leaves it unset.
    pub api: String,
    /// Its credentials and query scrubbed unless secrets were asked for.
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub models: Vec<base::provider::ModelEntry>,
    /// The tiers as written.
    pub tiers: base::provider::Tiers,
    pub quota: base::provider::Quota,
    /// Tokens used in the day, week and month the call falls in, each with
    /// its limit and first day; only for a provider with a `quota`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<base::provider::QuotaStatus>,
    /// Whether the credential source the engine builds this provider's client
    /// with yields a non-empty key. Never the key itself, whatever
    /// `include_secrets` says.
    pub credential_present: bool,
    /// The provider the engine supplies when the settings define none.
    pub builtin: bool,
    /// The settings file the entry is in; `None` for the built-in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defined_in: Option<String>,
}

/// What a provider's endpoint lists, as `config.listModels` answers.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelList {
    pub models: Vec<base::provider::ListedModel>,
    /// The endpoint had more pages than are read; `models` is the first of them.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderConfigView {
    /// Every defined provider in load order, or the one asked for.
    pub providers: Vec<ProviderView>,
    /// `model.order` as it routes: the first defined provider when unset.
    pub order: Vec<String>,
    pub tasks: base::provider::TaskTiers,
}

/// What reading the settings back and resolving provider routing found.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RoutingStatus {
    /// The providers route: every tier is in its provider's models and every
    /// id in `model.order` is defined.
    pub ok: bool,
    pub warnings: Vec<String>,
    pub error: Option<String>,
    /// A usable router was actually built; `ok` can hold without it.
    pub router_rebuilt: bool,
    pub router_error: Option<String>,
    pub mcp_connected: Vec<String>,
    pub mcp_failed: Vec<String>,
    pub mcp_connecting: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SettingsReloaded {
    pub providers: Vec<String>,
    pub order: Vec<String>,
    pub routing: RoutingStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderSaved {
    pub tier: SettingsTier,
    /// Nothing was written: `written_to` and the rest are what the write
    /// would have done.
    pub dry_run: bool,
    pub written_to: String,
    /// The provider settings written whose value in `settings.json` carries
    /// a credential, each said without it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(flatten)]
    pub reloaded: SettingsReloaded,
}

/// What `set_provider` is asked to write.
#[derive(Debug, Clone)]
pub struct ProviderPatch {
    /// Where a provider not yet defined is written; one already defined is
    /// written where it is.
    pub tier: SettingsTier,
    pub dry_run: bool,
    pub provider_id: String,
    pub delete: bool,
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TrustState {
    /// No project tier of its own.
    None,
    Trusted,
    Untrusted,
}

impl From<base::permission::ProjectTrust> for TrustState {
    fn from(trust: base::permission::ProjectTrust) -> Self {
        use base::permission::ProjectTrust;
        match trust {
            ProjectTrust::NoProject => Self::None,
            ProjectTrust::Trusted => Self::Trusted,
            ProjectTrust::Untrusted => Self::Untrusted,
        }
    }
}

impl TrustState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectStatus {
    pub project_root: PathBuf,
    pub trust: TrustState,
    /// The `app.trusted_projects` entry that matched; `null` when the
    /// default decided.
    pub matched: Option<String>,
    /// What decided `trust`: the default, a `trusted` pattern in the global
    /// `settings.json` (`user`), or one `project.trust` recorded (`host`);
    /// `null` with no project tier.
    pub trust_source: Option<settings::trust::TrustSource>,
    pub default: settings::trust::TrustLevel,
    /// What the project sets that takes effect only while it is trusted.
    pub requires_trust: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectTrustSet {
    #[serde(flatten)]
    pub status: ProjectStatus,
    pub written_to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImportRun {
    pub source: String,
    pub actions: Vec<String>,
}
