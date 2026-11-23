//! Model providers as `settings.json` describes them — the `providers` list,
//! each entry's vendor, protocol, models, tiers and quota — the contract a
//! session's code routes its calls through ([`TaskRouting`]), and where
//! quota usage is kept ([`QuotaLedger`]).
//!
//! Lives in `base` so `Settings` can carry these types directly. The router
//! that falls back along `model.order` is `model::routing`; building a live
//! client from a [`ProviderConfig`] needs a concrete protocol and is the
//! assembly's job (`assembly::model_router`).

use serde::{Deserialize, Serialize};

use crate::interface::model::Effort;

mod quota;
mod routing;

pub use quota::{PeriodUsage, QuotaLedger, QuotaStatus};
pub use routing::{ProviderSwitch, RouteObserver, TaskRouting};

/// The wire protocol a provider speaks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ApiType {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai_chat")]
    OpenAIChat,
    #[serde(rename = "openai_responses")]
    OpenAIResponses,
}

/// Who runs the endpoint. Decides what the protocol leaves open: how thinking
/// is switched on, whether reasoning goes back, how effort is spelled.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Vendor {
    Anthropic,
    Openai,
    Deepseek,
    Qwen,
    Vllm,
    Sglang,
    #[default]
    Generic,
}

/// The request fields an OpenAI-style endpoint switches thinking with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingToggle {
    /// `"thinking": {"type": "enabled" | "disabled"}`.
    Deepseek,
    /// `"enable_thinking": true | false`.
    EnableThinking,
    /// `"chat_template_kwargs": {"enable_thinking": true | false}`.
    ChatTemplateKwargs,
}

impl Vendor {
    pub fn thinking_toggle(self) -> Option<ThinkingToggle> {
        match self {
            Self::Deepseek => Some(ThinkingToggle::Deepseek),
            Self::Qwen => Some(ThinkingToggle::EnableThinking),
            Self::Vllm | Self::Sglang => Some(ThinkingToggle::ChatTemplateKwargs),
            Self::Anthropic | Self::Openai | Self::Generic => None,
        }
    }

    /// Whether the model's `reasoning_content` has to be echoed on the
    /// assistant messages of the current tool loop.
    pub fn passes_reasoning_back(self) -> bool {
        self == Self::Deepseek
    }

    /// `effort` as this vendor's endpoint names it.
    pub fn effort(self, effort: Effort) -> Effort {
        match (self, effort) {
            (Self::Deepseek, Effort::Medium | Effort::Xhigh) => Effort::High,
            (_, effort) => effort,
        }
    }
}

/// A model tier. Tasks and sessions ask for a tier; each provider says which
/// of its models serves it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Strong,
    Balanced,
    Fast,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::Strong, Tier::Balanced, Tier::Fast];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strong => "strong",
            Self::Balanced => "balanced",
            Self::Fast => "fast",
        }
    }

    pub fn from_name(name: &str) -> Option<Tier> {
        Self::ALL.into_iter().find(|tier| tier.as_str() == name)
    }

    /// The tier a Claude Code model alias stands for.
    pub fn from_claude_alias(name: &str) -> Option<Tier> {
        match name {
            "opus" => Some(Self::Strong),
            "sonnet" => Some(Self::Balanced),
            "haiku" => Some(Self::Fast),
            _ => None,
        }
    }
}

/// Which of a provider's models serves each tier, as written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Tiers {
    /// Required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strong: Option<String>,
    /// Unset, `strong`'s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balanced: Option<String>,
    /// Unset, `balanced`'s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast: Option<String>,
}

/// [`Tiers`] with the gaps filled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTiers {
    pub strong: String,
    pub balanced: String,
    pub fast: String,
}

impl ResolvedTiers {
    pub fn all(model: &str) -> Self {
        Self {
            strong: model.to_string(),
            balanced: model.to_string(),
            fast: model.to_string(),
        }
    }

    pub fn get(&self, tier: Tier) -> &str {
        match tier {
            Tier::Strong => &self.strong,
            Tier::Balanced => &self.balanced,
            Tier::Fast => &self.fast,
        }
    }

    /// The tier `model` serves here: `prefer` when it is one of them, else
    /// the first of strong, balanced, fast.
    pub fn tier_of(&self, model: &str, prefer: Tier) -> Option<Tier> {
        if self.get(prefer) == model {
            return Some(prefer);
        }
        Tier::ALL.into_iter().find(|tier| self.get(*tier) == model)
    }

    pub fn contains(&self, model: &str) -> bool {
        Tier::ALL.into_iter().any(|tier| self.get(tier) == model)
    }
}

/// Token limits per period, counted as uncached input plus cache writes plus
/// output; cache reads do not count. A period left out has no limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Quota {
    /// From 00:00 local time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day: Option<u64>,
    /// From Monday 00:00 local time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub week: Option<u64>,
    /// From the 1st, 00:00 local time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub month: Option<u64>,
}

impl Quota {
    pub fn is_unlimited(&self) -> bool {
        self.day.is_none() && self.week.is_none() && self.month.is_none()
    }
}

/// One entry of `providers`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Unique, of ASCII letters, digits, `-` and `_`. When the tiers define
    /// the same id twice, the first in load order (global, scene, project;
    /// each tier's `settings.local.json` after its `settings.json`) is used
    /// and the rest ignored.
    #[schemars(pattern(r"^[a-z0-9_-]+$"))]
    pub id: String,
    /// `anthropic`, `openai`, `deepseek`, `qwen`, `vllm`, `sglang` or
    /// `generic` (default). Decides how thinking is switched, whether
    /// reasoning is sent back and how effort is spelled.
    #[serde(default)]
    pub vendor: Vendor,
    /// `anthropic` (default), `openai_chat` or `openai_responses`, or a
    /// protocol the host registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Required for `openai_chat`; `anthropic` without one uses Anthropic's
    /// own API, `openai_responses` OpenAI's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The environment variable holding the key. Without one the host's
    /// credential source is asked. No child process inherits it once a
    /// settings file names it, unless `sandbox.env.allow` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// The models this provider serves, each an id or an object with what
    /// the engine should know about it. Empty accepts any id. Not fetched
    /// from the provider — `config.listModels` does that, on request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelEntry>,
    /// Which model serves `strong`, `balanced` and `fast`. `strong` is
    /// required; `balanced` defaults to `strong`'s, `fast` to `balanced`'s.
    /// Each must be in `models` when `models` lists any.
    #[serde(default)]
    pub tiers: Tiers,
    /// Token limits; past one, the provider is skipped until the period
    /// turns over.
    #[serde(default, skip_serializing_if = "Quota::is_unlimited")]
    pub quota: Quota,
    /// The provider the engine supplies when the settings define none.
    #[serde(skip)]
    #[schemars(skip)]
    pub builtin: bool,
}

/// One entry of a provider's `models`.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
#[non_exhaustive]
pub enum ModelEntry {
    Id(String),
    Spec(ModelSpec),
}

/// A model with what the engine should know about it beyond its id.
///
/// `context_window` and `max_output` are applied to the model's calls;
/// `reasoning_levels` and `input_types` are kept and reported; nothing in
/// the engine decides by them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// The most `max_tokens` a call to this model is sent with, and the most
    /// a reply cut off at its limit is retried with. Unset, the model's
    /// documented limit, looked up by its id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_levels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_types: Option<Vec<String>>,
}

impl ModelEntry {
    pub fn id(&self) -> &str {
        match self {
            Self::Id(id) => id,
            Self::Spec(spec) => &spec.id,
        }
    }

    pub fn spec(&self) -> Option<&ModelSpec> {
        match self {
            Self::Id(_) => None,
            Self::Spec(spec) => Some(spec),
        }
    }
}

impl From<&str> for ModelEntry {
    fn from(id: &str) -> Self {
        Self::Id(id.to_string())
    }
}

impl From<String> for ModelEntry {
    fn from(id: String) -> Self {
        Self::Id(id)
    }
}

/// By hand rather than `untagged`, so a malformed entry says what is wrong
/// with it instead of that it matched no variant.
impl<'de> Deserialize<'de> for ModelEntry {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::String(id) => Ok(Self::Id(id)),
            object @ serde_json::Value::Object(_) => serde_json::from_value(object)
                .map(Self::Spec)
                .map_err(|e| D::Error::custom(format!("a model entry: {e}"))),
            _ => Err(D::Error::custom(
                "a model entry is an id string or an object with `id`",
            )),
        }
    }
}

impl ProviderConfig {
    /// The id of the provider the engine supplies when the settings define
    /// none.
    pub const BUILTIN_ID: &'static str = "anthropic";

    /// The provider used when the settings define none: Anthropic's API,
    /// keyed and addressed from the environment, every tier `model`.
    pub fn builtin(model: &str) -> Self {
        Self {
            id: Self::BUILTIN_ID.to_string(),
            vendor: Vendor::Anthropic,
            api: Some("anthropic".to_string()),
            tiers: Tiers {
                strong: Some(model.to_string()),
                balanced: None,
                fast: None,
            },
            builtin: true,
            ..Default::default()
        }
    }

    /// Why `id` cannot name a provider, if it cannot. The id is the name of
    /// the provider's quota usage file as it stands, so it holds nothing a
    /// file name would have to change — and no capital, since `Ab` and `ab`
    /// are one file on a case-insensitive file system.
    pub fn id_problem(id: &str) -> Option<String> {
        if id.is_empty() {
            return Some("a provider needs an `id`".to_string());
        }
        let fits = id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        (!fits).then(|| {
            format!("provider id `{id}` may hold only lowercase ASCII letters, digits, `-` and `_`")
        })
    }

    /// `api`, defaulted.
    pub fn api(&self) -> &str {
        self.api.as_deref().unwrap_or("anthropic")
    }

    /// Whether `model` may be sent to this provider: listed, or nothing is.
    pub fn accepts_model(&self, model: &str) -> bool {
        self.models.is_empty() || self.models.iter().any(|m| m.id() == model)
    }

    /// Whether `models` lists `model`.
    pub fn lists_model(&self, model: &str) -> bool {
        self.models.iter().any(|m| m.id() == model)
    }

    pub fn model_ids(&self) -> Vec<&str> {
        self.models.iter().map(ModelEntry::id).collect()
    }

    /// What `models` says about `model` beyond its id, if anything.
    pub fn model_spec(&self, model: &str) -> Option<&ModelSpec> {
        self.models
            .iter()
            .find(|m| m.id() == model)
            .and_then(ModelEntry::spec)
    }

    /// The tiers with the gaps filled, or why this entry is not usable.
    pub fn resolved_tiers(&self) -> Result<ResolvedTiers, String> {
        let id = &self.id;
        let strong = self
            .tiers
            .strong
            .clone()
            .filter(|m| !m.is_empty())
            .ok_or_else(|| format!("provider `{id}` has no `tiers.strong`"))?;
        let balanced = self
            .tiers
            .balanced
            .clone()
            .unwrap_or_else(|| strong.clone());
        let fast = self.tiers.fast.clone().unwrap_or_else(|| balanced.clone());
        let resolved = ResolvedTiers {
            strong,
            balanced,
            fast,
        };
        if !self.models.is_empty() {
            for tier in Tier::ALL {
                let model = resolved.get(tier);
                if !self.lists_model(model) {
                    return Err(format!(
                        "provider `{id}`: `tiers.{}` is `{model}`, which is not in its `models`",
                        tier.as_str()
                    ));
                }
            }
        }
        Ok(resolved)
    }
}

/// One model a provider's endpoint lists, with only what the endpoint said.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListedModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_types: Option<Vec<String>>,
    /// Whether the model reasons, when the endpoint says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
}

/// The tier each kind of model call asks for. The task types: `main` (the
/// conversation), `subagent` (sub-agents started without a model of their
/// own), `compact` (summarizing for compaction), `memory` (extracting
/// memories), `title` (naming a session), `web_search` (`WebSearch`) and
/// `web_fetch` (`WebFetch` reading a page — unset, `WebFetch` uses no model
/// and returns the page text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TaskTiers {
    /// Default `strong`.
    pub main: Tier,
    /// Default `balanced`.
    pub subagent: Tier,
    /// Default `fast`.
    pub compact: Tier,
    /// Default `fast`.
    pub memory: Tier,
    /// Default `fast`.
    pub title: Tier,
    /// Default `fast`.
    pub web_search: Tier,
    /// Default unset: no model reads fetched pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_fetch: Option<Tier>,
}

impl Default for TaskTiers {
    fn default() -> Self {
        Self {
            main: Tier::Strong,
            subagent: Tier::Balanced,
            compact: Tier::Fast,
            memory: Tier::Fast,
            title: Tier::Fast,
            web_search: Tier::Fast,
            web_fetch: None,
        }
    }
}

impl TaskTiers {
    pub const MAIN: &'static str = "main";
    pub const SUBAGENT: &'static str = "subagent";
    pub const COMPACT: &'static str = "compact";
    pub const MEMORY: &'static str = "memory";
    pub const TITLE: &'static str = "title";
    pub const WEB_SEARCH: &'static str = "web_search";
    pub const WEB_FETCH: &'static str = "web_fetch";

    /// The tier `task` asks for; `None` for a task with no model, or one not
    /// in the list.
    pub fn get(&self, task: &str) -> Option<Tier> {
        match task {
            Self::MAIN => Some(self.main),
            Self::SUBAGENT => Some(self.subagent),
            Self::COMPACT => Some(self.compact),
            Self::MEMORY => Some(self.memory),
            Self::TITLE => Some(self.title),
            Self::WEB_SEARCH => Some(self.web_search),
            Self::WEB_FETCH => self.web_fetch,
            _ => None,
        }
    }
}

/// The providers `order` puts calls through, first to last, each with its
/// tiers filled — or what is wrong with the configuration. Every defined
/// provider is checked, ordered or not. An empty `order` is the first
/// defined provider alone.
pub fn routing_order<'a>(
    providers: &'a [ProviderConfig],
    order: &[String],
) -> Result<Vec<(&'a ProviderConfig, ResolvedTiers)>, String> {
    let mut resolved = Vec::with_capacity(providers.len());
    for provider in providers {
        if let Some(problem) = ProviderConfig::id_problem(&provider.id) {
            return Err(problem);
        }
        resolved.push((provider, provider.resolved_tiers()?));
    }
    let Some(first) = providers.first() else {
        return Err("no provider is defined".to_string());
    };
    let order: Vec<&str> = match order.is_empty() {
        true => vec![first.id.as_str()],
        false => order.iter().map(String::as_str).collect(),
    };
    let mut out: Vec<(&ProviderConfig, ResolvedTiers)> = Vec::with_capacity(order.len());
    for id in order {
        if out.iter().any(|(p, _)| p.id == id) {
            continue;
        }
        let Some((provider, tiers)) = resolved.iter().find(|(p, _)| p.id == id) else {
            return Err(format!(
                "`model.order` names provider `{id}`, which no `providers` entry defines"
            ));
        };
        out.push((provider, tiers.clone()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
