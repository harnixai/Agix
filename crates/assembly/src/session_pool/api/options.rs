//! 会话的 `options` 领域类型。serde 形状会写进会话日志的 `agix.session/spec` 条目，
//! 改字段名或默认值就是改持久化格式。

use super::AskKind;

/// Optional per-session configuration passed via `session.create` or
/// `session.run_turn` params.
///
/// Taken when the session is created and kept with it: every rebuild of the
/// session's engine — a settings change, an idle eviction, a daemon restart —
/// applies the same options again. Options sent with a later call to an
/// existing session are ignored, except by `session.create` under the
/// session's id, which refuses ones other than it was created with.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SessionOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorder: Option<RecorderOptions>,
    /// Where this session's kept time starts (the date in its prompt, its log's timestamps).
    /// Omitted: the machine's clock — except in replay, where it starts when the recording did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<ClockOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetryOptions>,
    /// This session's permission mode. `None` takes the settings'
    /// `permissions.mode`. A session may always ask for a stricter mode than
    /// the settings'; a looser one is clamped unless the settings set
    /// `permissions.client_may_loosen`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<base::permission::PermissionMode>,
    /// Allow/deny/ask rules for the `Policy` this session's
    /// `permission_mode` (if set) constructs. Ignored when `permission_mode`
    /// is `None`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_rules: Vec<base::permission::PermissionRule>,
    /// JSON Schema the session's structured output must satisfy — see
    /// `runtime::agent::Builder::output_schema`.
    pub output_schema: Option<serde_json::Value>,
    /// A provider, by its id in `providers`, put first in this session's
    /// `model.order` — for its conversation, sub-agents, compaction and every
    /// other call; the rest of the order follows as the settings have it.
    /// `None` keeps the settings' order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The conversation's model: a tier — `strong`, `balanced`, `fast` — or a
    /// model id. `None` takes `model.tasks.main`'s tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The conversation's reasoning effort; `None` takes the settings'
    /// `model.effort`. Its sub-agents keep the settings'.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<base::interface::model::Effort>,
    /// The conversation's thinking mode; `None` takes the settings'
    /// `model.thinking`. Its sub-agents keep the settings'.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_mode: Option<base::interface::model::ThinkingMode>,
    /// A system-prompt append / override for this session's own agent, over
    /// the settings files'. Its delegates do not get it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<runtime::agent::SessionPrompt>,
    /// The kinds of `prompt` frame this session's host answers: `permission`
    /// always, `question` and `mcp_elicitation` when listed. Names this daemon does not know are
    /// ignored rather than voiding the options.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_types: Vec<String>,
    /// Tools this session's host runs itself, offered to the model as the
    /// session's own; a call to one is a `host_tool_call` frame, answered
    /// with `session.respondToolCall`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_tools: Vec<HostToolOption>,
    /// Whether someone answers this session's questions and permission
    /// prompts; `None` is its scene's `interactive`. Its sub-agents and team
    /// members are attended as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attended: Option<bool>,
    /// `false`: the session is not named after its first turn, its name is
    /// left to `session.rename`. `None` or `true`: as its scene says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_name: Option<bool>,
    /// Skill directories of the host's for this session, merged over the
    /// tiers by name: a built-in, a package's or an untrusted project's skill
    /// yields to one of these; the user's tiers and a trusted project's do
    /// not. Of two listed directories the first wins a name. Read-only to
    /// every session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_dirs: Vec<SkillDirOption>,
}

/// One entry of `options.skill_dirs` and `session.setSkillDirs`.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SkillDirOption {
    /// Absolute, on the machine the engine runs on. Need not exist yet.
    pub path: String,
}

/// One entry of `options.host_tools`.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostToolOption {
    /// `^[A-Za-z][A-Za-z0-9_-]{0,63}$`, not starting `mcp__`, not taken.
    pub name: String,
    /// At most 8 KiB.
    pub description: String,
    /// A `type: object` JSON Schema, at most 64 KiB; the host only ever gets
    /// input that matches it.
    pub input_schema: serde_json::Value,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub destructive: bool,
    /// Omitted: `read_only`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency_safe: Option<bool>,
    /// 1000..=600000.
    #[serde(default = "HostToolOption::default_timeout_ms")]
    pub timeout_ms: u64,
    /// Offered by name only; the schema comes from `ToolSearch`.
    #[serde(default)]
    pub deferred: bool,
    /// The session's sub-agents are offered it too.
    #[serde(default)]
    pub subagents: bool,
}

impl HostToolOption {
    fn default_timeout_ms() -> u64 {
        120_000
    }

    pub fn declaration(&self) -> base::interface::host_call::HostToolDecl {
        let traits = base::tool::CallTraits::default()
            .read_only(self.read_only)
            .destructive(self.destructive)
            .concurrency_safe(self.concurrency_safe.unwrap_or(self.read_only));
        base::interface::host_call::HostToolDecl::new(
            &self.name,
            &self.description,
            self.input_schema.clone(),
        )
        .traits(traits)
        .timeout(std::time::Duration::from_millis(self.timeout_ms))
        .deferred(self.deferred)
        .subagents(self.subagents)
    }
}

/// Record or replay this session's LLM calls.
///
/// A recorder that records is kept with the session's spec, so the session
/// goes on recording after a restart; one that replays or reruns is not, and
/// is given again by the call that brings the session back.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct RecorderOptions {
    pub mode: String, // "record" | "replay" | "rerun"
    /// Directory name for the recording under `dir`: one path component.
    /// Omitted, the session id — recording, the session's sub-agents then
    /// record into that same directory; a replay must name the recording it
    /// wants back.
    #[serde(default)]
    pub name: Option<String>,
    /// Absolute path to the recordings root. Omitted, the daemon's own root —
    /// `settings.recorder.root` if configured, else
    /// `ConfigPaths::global_recordings_dir()` — as it is when the session is
    /// created; a recording session keeps that root from then on.
    #[serde(default)]
    pub dir: Option<String>,
    /// On replay, whether a live request that differs from the recorded one
    /// fails the call (`true`) or only warns (`false`).
    #[serde(default)]
    pub strict: bool,
}

impl RecorderOptions {
    pub fn records(&self) -> bool {
        self.mode == "record"
    }

    /// What is wrong with `dir` and `name`, by field.
    pub fn problems(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(dir) = &self.dir {
            if !std::path::Path::new(dir).is_absolute() {
                out.push(("dir", format!("`{dir}` is not an absolute path")));
            }
        }
        if let Some(name) = &self.name {
            let one = matches!(
                std::path::Path::new(name)
                    .components()
                    .collect::<Vec<_>>()
                    .as_slice(),
                [std::path::Component::Normal(_)]
            );
            if !one {
                out.push(("name", format!("`{name}` is not a single path component")));
            }
        }
        out
    }
}

#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(default)]
pub struct ClockOptions {
    /// RFC 3339, e.g. `2026-10-28T09:00:00Z`. The clock reads this when the session is built
    /// and runs on at wall-clock speed.
    pub start: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct TelemetryOptions {
    pub output: String, // absolute path to telemetry output file
}

impl SessionOptions {
    /// Whether the session may be named after its first turn: unless
    /// `auto_name` says no.
    pub fn names_itself(&self) -> bool {
        self.auto_name != Some(false)
    }

    /// Whether the host answers the model's questions (`AskUserQuestion`).
    pub fn answers_questions(&self) -> bool {
        self.prompt_types
            .iter()
            .any(|t| t == AskKind::Question.as_str())
    }

    /// Whether the host answers MCP servers' questions.
    pub fn answers_mcp_elicitation(&self) -> bool {
        self.prompt_types
            .iter()
            .any(|t| t == AskKind::McpElicitation.as_str())
    }
}
