//! Multi-provider LLM config: `ApiType` + provider registry + per-task-type
//! model routing, parsed from the `providers`/`default_provider`/
//! `task_models` sections of `settings.json`.
//!
//! Lives in `core` so `Settings` can carry these fields directly, rather
//! than keeping a second representation of the same configuration in sync.
//!
//! This module resolves *which provider/model a task should use* from
//! already-merged config (`resolve_task_models`) and, via [`TaskRouter`],
//! hands back the already-constructed `Arc<dyn Model>` instance for a given
//! task type. `TaskRouter` itself only holds `Arc<dyn Model>` values — it
//! doesn't know how to build one from a [`ProviderConfig`] (that needs a
//! concrete `Model` impl like `model::adapter::AnthropicModel`, and `core`
//! can't depend on `model` without inverting the crate layering). The
//! construction site is `daemon::model_router::build_task_router`; wiring it
//! into `SessionPool`/`AgentTool`'s sub-agent spawn points is
//! `crates/runtime/src/agent/builder/mod.rs::Builder::task_router` /
//! `crates/agents/src/agent_tool/resolve.rs::Delegator::model_for_subagent`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// API protocol type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ApiType {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai_compatible")]
    OpenAICompatible,
    #[serde(rename = "openai_responses")]
    OpenAIResponses,
}

/// A single provider's connection + model config, as written in settings.json.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ProviderConfig {
    /// "anthropic" | "openai_compatible" | "openai_responses". Kept as a plain string (not
    /// `ApiType`) so an unrecognized value degrades to a warning instead of
    /// a hard parse failure of the whole settings.json.
    pub api_type: Option<String>,
    pub base_url: Option<String>,
    /// Stored in plaintext, same as every other settings.json field — which
    /// is why [`Debug`](std::fmt::Debug) is hand-written below rather than
    /// derived. To keep the key off disk, name an environment variable in
    /// `api_key_env` instead, or have the host supply it through
    /// [`CredentialSource`](crate::interface::credentials::CredentialSource).
    pub api_key: Option<String>,
    /// Environment variable holding the key, read when `api_key` is unset.
    /// No child process inherits it once a settings file names it, unless
    /// `sandbox.env.allow` does.
    pub api_key_env: Option<String>,
    /// Model used when a task_models override doesn't name one explicitly,
    /// or when a named model fails the `models` allow-list check below.
    pub default_model: Option<String>,
    /// The models this provider serves, each a bare id or an id with what the
    /// engine should know about it. The ids filter an explicitly requested
    /// model name; empty accepts any name. Not fetched from the provider —
    /// `config.listModels` does that, on request.
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    /// OpenAI-compatible only: echo the model's `reasoning_content` back on
    /// the assistant messages of the current tool loop. DeepSeek's thinking
    /// mode requires it; OpenAI itself and older reasoners reject it, hence
    /// off by default.
    pub reasoning_passback: bool,
    /// OpenAI-compatible only: how this endpoint is told to turn thinking on
    /// or off when `thinking_mode` is not `auto`. Unset sends nothing — Chat
    /// Completions has no standard field for it.
    pub thinking_toggle: Option<ThinkingToggle>,
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
pub struct ModelSpec {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// The most `max_tokens` a call to this model is sent with.
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
    /// Whether `model` may be sent to this provider: listed, or nothing is.
    pub fn accepts_model(&self, model: &str) -> bool {
        self.models.is_empty() || self.models.iter().any(|m| m.id() == model)
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

/// The request fields OpenAI-compatible endpoints use to switch thinking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingToggle {
    /// `"thinking": {"type": "enabled" | "disabled"}` (DeepSeek).
    Deepseek,
    /// `"enable_thinking": true | false` (Qwen / DashScope).
    EnableThinking,
    /// `"chat_template_kwargs": {"enable_thinking": true | false}` (vLLM, SGLang).
    ChatTemplateKwargs,
}

/// A `task_models.<task>` entry. Accepts either the shorthand `"provider_id"`
/// string form or the detailed `{ "provider": ..., "model": ... }` object form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum TaskModelOverride {
    ProviderOnly(String),
    Detailed {
        provider: String,
        #[serde(default)]
        model: Option<String>,
    },
}

impl TaskModelOverride {
    fn provider(&self) -> &str {
        match self {
            TaskModelOverride::ProviderOnly(p) => p,
            TaskModelOverride::Detailed { provider, .. } => provider,
        }
    }

    fn model(&self) -> Option<&str> {
        match self {
            TaskModelOverride::ProviderOnly(_) => None,
            TaskModelOverride::Detailed { model, .. } => model.as_deref(),
        }
    }
}

/// The fully-resolved (provider, model) pair a task type will actually use.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModel {
    pub provider_id: String,
    pub model: String,
}

impl std::fmt::Debug for ProviderConfig {
    /// Everything but the key.
    ///
    /// A credential does not leak because someone logged the credential; it
    /// leaks because someone logged the struct that happened to hold one.
    /// Derived `Debug` put the key one `{:?}` away from any log line, any
    /// error context, any telemetry payload built by formatting.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("api_type", &self.api_type)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("api_key_env", &self.api_key_env)
            .field("default_model", &self.default_model)
            .field("models", &self.models)
            .field("reasoning_passback", &self.reasoning_passback)
            .field("thinking_toggle", &self.thinking_toggle)
            .finish()
    }
}

/// Resolve every `task_models` entry (plus an implicit `default_provider`
/// resolution used by any task with no entry) against the `providers` map.
///
/// Degrades softly wherever possible — this is deliberate: a config typo
/// should produce a startup warning, not a crash. Two tiers of failure:
///
/// - `default_provider` missing, empty, or not present in `providers` →
///   hard error (`Err`); there is no lower fallback left.
/// - A `task_models` entry naming an unknown provider → falls back to
///   `default_provider`, with a warning.
/// - A `task_models` entry naming a model absent from that provider's
///   (non-empty) `models` allow-list → falls back to *that provider's own*
///   `default_model`, with a warning (not to `default_provider` — the
///   provider choice itself was valid, only the model name wasn't).
pub fn resolve_task_models(
    providers: &HashMap<String, ProviderConfig>,
    default_provider: Option<&str>,
    task_models: &HashMap<String, TaskModelOverride>,
) -> Result<(HashMap<String, ResolvedModel>, Vec<String>), String> {
    let mut warnings = Vec::new();

    let default_provider = default_provider
        .filter(|p| !p.is_empty())
        .ok_or_else(|| "default_provider is not set but providers are configured".to_string())?;
    let default_model = providers
        .get(default_provider)
        .ok_or_else(|| {
            format!(
                "default_provider '{default_provider}' is not present in providers \
                 ({:?})",
                providers.keys().collect::<Vec<_>>()
            )
        })?
        .default_model
        .clone()
        .ok_or_else(|| format!("default_provider '{default_provider}' has no default_model"))?;

    let mut resolved = HashMap::new();

    for (task, ov) in task_models {
        let requested_provider = ov.provider();
        let Some(provider_cfg) = providers.get(requested_provider) else {
            warnings.push(format!(
                "task_models.{task} 引用的 provider '{requested_provider}' 不存在，\
                 已降级到 default_provider '{default_provider}'"
            ));
            resolved.insert(
                task.clone(),
                ResolvedModel {
                    provider_id: default_provider.to_string(),
                    model: default_model.clone(),
                },
            );
            continue;
        };

        let model = match ov.model() {
            None => provider_cfg.default_model.clone().unwrap_or_else(|| {
                warnings.push(format!(
                    "task_models.{task} 的 provider '{requested_provider}' 没有配置 \
                     default_model，回退到 default_provider '{default_provider}'"
                ));
                default_model.clone()
            }),
            Some(m) if provider_cfg.accepts_model(m) => m.to_string(),
            Some(m) => {
                let fallback = provider_cfg.default_model.clone().unwrap_or_else(|| {
                    warnings.push(format!(
                        "task_models.{task} 指定的模型 '{m}' 不在 provider \
                         '{requested_provider}' 的 models 列表 {:?} 中，且该 provider 没有 \
                         default_model，回退到 default_provider '{default_provider}'",
                        provider_cfg.model_ids()
                    ));
                    default_model.clone()
                });
                warnings.push(format!(
                    "task_models.{task} 指定的模型 '{m}' 不在 provider '{requested_provider}' \
                     的 models 列表 {:?} 中，已回退到该 provider 的默认模型 '{fallback}'",
                    provider_cfg.model_ids()
                ));
                fallback
            }
        };

        resolved.insert(
            task.clone(),
            ResolvedModel {
                provider_id: requested_provider.to_string(),
                model,
            },
        );
    }

    Ok((resolved, warnings))
}

/// Per-task-type model routing, ready to answer "which `Arc<dyn Model>`
/// should this task use" without re-resolving config on every call.
///
/// Purely a data holder: `providers`/`default` are already-constructed
/// `Arc<dyn Model>` instances (one per configured provider), `resolved`
/// comes straight from [`resolve_task_models`]. Building the instances
/// themselves lives outside `core` — see the module doc comment.
pub struct TaskRouter {
    providers: HashMap<String, Arc<dyn crate::interface::model::Model>>,
    /// task_type → (provider_id, model). Only contains entries for task
    /// types with an explicit `task_models` override (see
    /// `resolve_task_models`'s doc comment) — task types with no entry use
    /// `default`.
    resolved: HashMap<String, ResolvedModel>,
    /// `providers[default_provider]`'s instance — used for any task type
    /// with no `task_models` entry, or whose resolved provider isn't in
    /// `providers` (defensive; `resolve_task_models` only ever resolves to
    /// providers present in the same map `providers` was built from, so
    /// this shouldn't be reachable in practice).
    default: Arc<dyn crate::interface::model::Model>,
    /// What a task with no entry asks `default` for when the conversation is
    /// on another provider's client; `None` leaves it on the conversation's.
    default_model: Option<String>,
}

impl TaskRouter {
    pub fn new(
        providers: HashMap<String, Arc<dyn crate::interface::model::Model>>,
        resolved: HashMap<String, ResolvedModel>,
        default: Arc<dyn crate::interface::model::Model>,
    ) -> Self {
        Self {
            providers,
            resolved,
            default,
            default_model: None,
        }
    }

    pub fn with_default_model(mut self, name: impl Into<String>) -> Self {
        self.default_model = Some(name.into());
        self
    }

    /// The model name `task` goes to [`Self::model_for`]'s client with, for a
    /// conversation asking `conversation` for `conversation_model`: its
    /// entry's; else the conversation's when that is on the default client
    /// too, and the default model when it is not.
    pub fn model_name_on_route(
        &self,
        task: &str,
        conversation: &dyn crate::interface::model::Model,
        conversation_model: &str,
    ) -> String {
        if let Some(r) = self.resolved.get(task) {
            return r.model.clone();
        }
        match &self.default_model {
            Some(name) if self.default.provenance() != conversation.provenance() => name.clone(),
            _ => conversation_model.to_string(),
        }
    }

    /// The model instance `task` should use.
    pub fn model_for(&self, task: &str) -> Arc<dyn crate::interface::model::Model> {
        match self.resolved.get(task) {
            Some(r) => self
                .providers
                .get(&r.provider_id)
                .cloned()
                .unwrap_or_else(|| self.default.clone()),
            None => self.default.clone(),
        }
    }

    /// The client configured for provider `id`, if there is one.
    pub fn provider(&self, id: &str) -> Option<Arc<dyn crate::interface::model::Model>> {
        self.providers.get(id).cloned()
    }

    /// The provider `task` is routed to, if `task_models` names one for it.
    pub fn provider_id_for(&self, task: &str) -> Option<&str> {
        self.resolved.get(task).map(|r| r.provider_id.as_str())
    }

    /// The model name string `task` should use, if `task_models` has an
    /// explicit override for it. `None` means "no override" — callers
    /// combine this with `model_for()` (which already falls back to
    /// `default` in the same case) and their own default model-name
    /// constant, mirroring how `model_for` falls back.
    pub fn model_name_for(&self, task: &str) -> Option<&str> {
        self.resolved.get(task).map(|r| r.model.as_str())
    }

    /// A router with every provider's model replaced by `wrap(provider_id, model)`.
    ///
    /// Exists so a decorator can be applied to *all* routed models at once.
    /// Wrapping only the conversation's model left every routed call — every
    /// sub-agent, every summarizer — running through an undecorated instance,
    /// which is how recording a session with providers configured silently
    /// captured only part of it.
    ///
    /// `default` is one of `providers`' own values, so it is remapped to that
    /// provider's wrapped instance rather than wrapped a second time.
    pub fn map_models(
        &self,
        wrap: impl Fn(
            &str,
            Arc<dyn crate::interface::model::Model>,
        ) -> Arc<dyn crate::interface::model::Model>,
    ) -> Self {
        let providers: HashMap<String, Arc<dyn crate::interface::model::Model>> = self
            .providers
            .iter()
            .map(|(id, model)| (id.clone(), wrap(id, model.clone())))
            .collect();
        let default = self
            .providers
            .iter()
            .find(|(_, model)| Arc::ptr_eq(model, &self.default))
            .and_then(|(id, _)| providers.get(id).cloned())
            .unwrap_or_else(|| wrap("default", self.default.clone()));
        Self {
            providers,
            resolved: self.resolved.clone(),
            default,
            default_model: self.default_model.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(default_model: &str, models: &[&str]) -> ProviderConfig {
        ProviderConfig {
            api_type: Some("openai_compatible".into()),
            base_url: Some("https://example.invalid".into()),
            api_key: Some("key".into()),
            api_key_env: None,
            default_model: Some(default_model.into()),
            models: models.iter().map(|s| (*s).into()).collect(),
            ..Default::default()
        }
    }

    fn providers_fixture() -> HashMap<String, ProviderConfig> {
        let mut p = HashMap::new();
        p.insert("anthropic".to_string(), provider("claude-sonnet-4-6", &[]));
        p.insert(
            "deepseek".to_string(),
            provider("deepseek-pro", &["deepseek-pro", "deepseek-flash"]),
        );
        p
    }

    #[test]
    fn no_override_uses_default_provider_and_its_default_model() {
        let providers = providers_fixture();
        let task_models = HashMap::new();
        let (resolved, warnings) =
            resolve_task_models(&providers, Some("anthropic"), &task_models).unwrap();
        assert!(resolved.is_empty()); // nothing to resolve; callers fall back to default themselves
        assert!(warnings.is_empty());
    }

    #[test]
    fn shorthand_override_uses_that_providers_default_model() {
        let providers = providers_fixture();
        let mut task_models = HashMap::new();
        task_models.insert(
            "subagent".to_string(),
            TaskModelOverride::ProviderOnly("deepseek".into()),
        );
        let (resolved, warnings) =
            resolve_task_models(&providers, Some("anthropic"), &task_models).unwrap();
        assert_eq!(
            resolved["subagent"],
            ResolvedModel {
                provider_id: "deepseek".into(),
                model: "deepseek-pro".into(),
            }
        );
        assert!(warnings.is_empty());
    }

    /// Mirrors the user's own example: DS supports PRO/FLASH; asking for MAX
    /// warns and falls back to DS's own default (PRO), not to the global
    /// default_provider.
    #[test]
    fn model_outside_allow_list_falls_back_to_same_providers_default_and_warns() {
        let providers = providers_fixture();
        let mut task_models = HashMap::new();
        task_models.insert(
            "subagent".to_string(),
            TaskModelOverride::Detailed {
                provider: "deepseek".into(),
                model: Some("deepseek-max".into()),
            },
        );
        let (resolved, warnings) =
            resolve_task_models(&providers, Some("anthropic"), &task_models).unwrap();
        assert_eq!(
            resolved["subagent"],
            ResolvedModel {
                provider_id: "deepseek".into(),
                model: "deepseek-pro".into(),
            }
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("deepseek-max"));
        assert!(warnings[0].contains("deepseek-pro"));
    }

    #[test]
    fn model_inside_allow_list_is_used_as_is() {
        let providers = providers_fixture();
        let mut task_models = HashMap::new();
        task_models.insert(
            "subagent".to_string(),
            TaskModelOverride::Detailed {
                provider: "deepseek".into(),
                model: Some("deepseek-flash".into()),
            },
        );
        let (resolved, warnings) =
            resolve_task_models(&providers, Some("anthropic"), &task_models).unwrap();
        assert_eq!(
            resolved["subagent"],
            ResolvedModel {
                provider_id: "deepseek".into(),
                model: "deepseek-flash".into(),
            }
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn empty_allow_list_accepts_any_model_name() {
        let providers = providers_fixture();
        let mut task_models = HashMap::new();
        task_models.insert(
            "main".to_string(),
            TaskModelOverride::Detailed {
                provider: "anthropic".into(),
                model: Some("claude-opus-4-8".into()),
            },
        );
        let (resolved, warnings) =
            resolve_task_models(&providers, Some("anthropic"), &task_models).unwrap();
        assert_eq!(
            resolved["main"],
            ResolvedModel {
                provider_id: "anthropic".into(),
                model: "claude-opus-4-8".into(),
            }
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn unknown_provider_falls_back_to_default_provider_and_warns() {
        let providers = providers_fixture();
        let mut task_models = HashMap::new();
        task_models.insert(
            "web_fetch".to_string(),
            TaskModelOverride::ProviderOnly("nonexistent".into()),
        );
        let (resolved, warnings) =
            resolve_task_models(&providers, Some("anthropic"), &task_models).unwrap();
        assert_eq!(
            resolved["web_fetch"],
            ResolvedModel {
                provider_id: "anthropic".into(),
                model: "claude-sonnet-4-6".into(),
            }
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("nonexistent"));
    }

    #[test]
    fn missing_default_provider_is_a_hard_error() {
        let providers = providers_fixture();
        let task_models = HashMap::new();
        let err = resolve_task_models(&providers, None, &task_models).unwrap_err();
        assert!(err.contains("default_provider"));
    }

    #[test]
    fn default_provider_not_in_providers_is_a_hard_error() {
        let providers = providers_fixture();
        let task_models = HashMap::new();
        let err = resolve_task_models(&providers, Some("ghost"), &task_models).unwrap_err();
        assert!(err.contains("ghost"));
    }

    /// Minimal `Model` stub — `stream()` is never called by these tests
    /// (they only exercise routing, not actual requests). Instances are
    /// told apart via `Arc::ptr_eq`, not any field on this type.
    struct TaggedModel;

    #[async_trait::async_trait]
    impl crate::interface::model::Model for TaggedModel {
        fn api_type(&self) -> crate::provider::ApiType {
            crate::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<crate::prompt::PromptBlock>,
            _tools: Vec<crate::interface::model::ToolDef>,
            _messages: Vec<crate::interface::model::ModelMessage>,
            _params: crate::interface::model::StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<crate::interface::model::ModelStream, crate::interface::model::ModelError>
        {
            unimplemented!("routing tests never call stream()")
        }
    }

    fn tagged(_label: &'static str) -> Arc<dyn crate::interface::model::Model> {
        Arc::new(TaggedModel)
    }

    #[test]
    fn task_router_routes_to_resolved_providers_model() {
        let mut providers = HashMap::new();
        providers.insert("anthropic".to_string(), tagged("anthropic"));
        providers.insert("deepseek".to_string(), tagged("deepseek"));

        let mut resolved = HashMap::new();
        resolved.insert(
            "subagent".to_string(),
            ResolvedModel {
                provider_id: "deepseek".into(),
                model: "deepseek-pro".into(),
            },
        );

        let router = TaskRouter::new(providers, resolved, tagged("anthropic"));
        let picked = router.model_for("subagent");
        assert_eq!(picked.api_type(), ApiType::Anthropic); // sanity: both tagged models report the same api_type
                                                           // Identity check via Arc pointer equality — proves the deepseek
                                                           // instance (not the default) was returned.
        assert!(Arc::ptr_eq(
            &picked,
            router.providers.get("deepseek").unwrap()
        ));
    }

    #[test]
    fn task_router_falls_back_to_default_for_unresolved_task() {
        let mut providers = HashMap::new();
        providers.insert("anthropic".to_string(), tagged("anthropic"));
        let default = tagged("anthropic");

        let router = TaskRouter::new(providers, HashMap::new(), default.clone());
        let picked = router.model_for("main"); // no task_models entry for "main"
        assert!(Arc::ptr_eq(&picked, &default));
    }

    #[test]
    fn task_router_falls_back_to_default_when_resolved_provider_is_missing() {
        // Defensive path: resolved names a provider that isn't in the
        // `providers` map (shouldn't happen via `resolve_task_models`, but
        // `TaskRouter::new` doesn't re-validate that invariant itself).
        let providers = HashMap::new();
        let mut resolved = HashMap::new();
        resolved.insert(
            "subagent".to_string(),
            ResolvedModel {
                provider_id: "ghost".into(),
                model: "whatever".into(),
            },
        );
        let default = tagged("anthropic");

        let router = TaskRouter::new(providers, resolved, default.clone());
        let picked = router.model_for("subagent");
        assert!(Arc::ptr_eq(&picked, &default));
    }

    #[test]
    fn task_model_override_deserializes_both_forms() {
        let shorthand: TaskModelOverride = serde_json::from_str("\"deepseek\"").unwrap();
        assert_eq!(
            shorthand,
            TaskModelOverride::ProviderOnly("deepseek".into())
        );

        let detailed: TaskModelOverride =
            serde_json::from_str(r#"{"provider":"deepseek","model":"deepseek-flash"}"#).unwrap();
        assert_eq!(
            detailed,
            TaskModelOverride::Detailed {
                provider: "deepseek".into(),
                model: Some("deepseek-flash".into())
            }
        );
    }

    #[test]
    fn models_take_ids_and_objects_side_by_side() {
        let cfg: ProviderConfig = serde_json::from_value(serde_json::json!({
            "models": ["a", {"id": "b", "max_output": 100, "input_types": ["text"]}]
        }))
        .unwrap();
        assert_eq!(cfg.model_ids(), ["a", "b"]);
        assert!(cfg.accepts_model("b") && !cfg.accepts_model("c"));
        assert_eq!(cfg.model_spec("a"), None);
        assert_eq!(cfg.model_spec("b").and_then(|s| s.max_output), Some(100));
        assert_eq!(
            serde_json::to_value(&cfg.models).unwrap(),
            serde_json::json!(["a", {"id": "b", "max_output": 100, "input_types": ["text"]}])
        );

        let err = serde_json::from_value::<ProviderConfig>(serde_json::json!({
            "models": [{"max_output": 1}]
        }))
        .unwrap_err();
        assert!(err.to_string().contains("`id`"), "{err}");
    }
}
