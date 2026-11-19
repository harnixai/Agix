//! The settings types: what one settings.json file may say
//! ([`SettingsFile`]), what the tiers say once merged ([`Settings`]), and what
//! loading them produced ([`LoadedSettings`] — the values, where this scene's
//! data lives, each permission rule with its tier, the managed layer and the
//! project-trust decision).
//!
//! [`LoadedSettings::load`] is the single canonical settings.json loader
//! (global → scene → project, generic recursive JSON merge). Everything
//! downstream (the AGENT itself, `Builder`) receives the already-merged
//! result; it does not perform its own multi-layer config merging.

use base::interface::model::{Effort, ThinkingMode};
use base::permission::{PermissionMode, PermissionRuleEntry};
use base::provider::{ApiType, ProviderConfig, TaskModelOverride};
use base::recorder::RecorderConfig;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

pub mod edit;
mod egress;
pub use egress::EgressProxySetting;
mod engine;
pub use engine::sandbox_settings;
pub mod file;
pub mod import;
mod loaded;
pub mod managed;
mod merge;
mod schema;
pub mod section;
mod source;
pub mod trust;

#[cfg(test)]
mod tests;

pub use loaded::LoadedSettings;
pub use merge::merge_json_values;
pub use schema::{settings_json_schema, unknown_settings_keys, KeyKind, SchemaKeys};
pub use source::{read_settings_file, FileTiers};

/// The keys only the global tier's files may set; the loader drops them from
/// every other tier.
pub const GLOBAL_ONLY_KEYS: &[&str] = &["project_trust", "network", "history"];

/// The keys a project's own files may not set; the loader drops them from the
/// project tier.
pub const NOT_FROM_PROJECT_KEYS: &[&str] = &["worktree_dir"];

/// One settings.json file: the settings it overrides, and the permission rules
/// it adds to every other tier's.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SettingsFile {
    #[serde(flatten)]
    pub settings: Settings,
    /// Allow/deny/ask rules for specific tools.
    ///
    /// Every tier's rules apply side by side: a project's rules are added to
    /// the global and scene ones, never substituted for them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_rules: Vec<PermissionRuleEntry>,
}

/// What the settings tiers say once merged. Every field is one a settings
/// file can set; what loading them produced besides is [`LoadedSettings`].
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[non_exhaustive]
pub struct Settings {
    pub model: ModelSettings,
    #[serde(default)]
    pub execution: ExecutionSettings,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,

    /// One more instruction file (e.g. a `CLAUDE.md` outside the project),
    /// read when the session is built and given to the model alongside the
    /// discovered `AGENTS.md` files. A root session with a `FileChanged`
    /// hook watches it.
    #[serde(default)]
    pub instruction_file: Option<PathBuf>,

    /// Appended to the end of the system prompt.
    #[serde(default)]
    pub prompt_append: Option<String>,
    /// Overrides the entire system prompt if set.
    #[serde(default)]
    pub prompt_override: Option<String>,

    /// LLM interaction recording. None = pass-through.
    #[serde(default)]
    pub recorder: Option<RecorderConfig>,
    /// OTLP endpoint telemetry events are exported to. Unset, the
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` environment variable is used; neither
    /// means no export. Only a build with the `otel` feature exports — any
    /// other logs a warning when this is set.
    #[serde(default)]
    pub telemetry_url: Option<String>,
    /// The file-based memory system (`MEMORY.md` + one `.md` per memory).
    /// Default `true`. `false` drops the memory prompt, the memory index in
    /// each turn's reminder, and recall.
    #[serde(default = "default_memory_enabled")]
    pub memory_enabled: bool,

    /// Disable skill dynamic-content-injection shell commands (`` !`cmd` ``
    /// and fenced ` ```! ` blocks in `SKILL.md` bodies). Default: false
    /// (enabled). When true, each such placeholder is replaced with
    /// `[shell command execution disabled by policy]` instead of running.
    /// A higher tier can set it back, except a project that is not trusted
    /// (`project_trust`).
    #[serde(default)]
    pub disable_skill_shell_execution: bool,

    /// How the file tools word paths for the model. `absolute` keeps
    /// today's text; `workspace_relative` makes tool descriptions say paths
    /// are relative to the working directory and prints Grep / Glob / Write /
    /// Edit paths under it relative, so a recorded session replays in another
    /// working directory and long paths stop repeating in every call.
    /// Resolving paths is unchanged. Unset, the scene decides
    /// (`SceneSpec::path_style`: `harnix` is relative, the rest absolute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_style: Option<base::context::config::PathStyle>,

    /// Permission mode for tool execution.
    #[serde(default)]
    pub permission_mode: PermissionMode,

    /// Whether an RPC client may request a *more permissive* mode than
    /// `permission_mode` above.
    ///
    /// The daemon's `session.create` takes an optional `permission_mode` in
    /// its options. With this `false` — the default — a client can *tighten*
    /// the mode for its own session, but a request to loosen it is clamped to
    /// what is configured here. Otherwise any client that could authenticate
    /// could open a `bypassPermissions` session no matter how the daemon was
    /// configured (`crates/assembly/src/server/mod.rs` notes there is no
    /// per-method authorization on that socket).
    ///
    /// Set it to `true` only when every client of this daemon is as trusted
    /// as the daemon itself.
    #[serde(default)]
    pub allow_client_permission_override: bool,

    /// Whether sessions record a telemetry transcript.
    ///
    /// Defaults to **on**: a session writes one JSONL file per session under
    /// `<local_data_dir>/telemetry/`, which is what makes a run debuggable
    /// after the fact (tool calls, permission decisions, turn costs).
    #[serde(default = "default_true")]
    pub telemetry_enabled: bool,

    /// Lifecycle hooks: an object from event name (`PreToolUse`,
    /// `SessionStart`, … — `hooks::HookEvent`) to a list of hooks, each with
    /// a `type` of `command`, `prompt`, `http` or `agent`. Tiers merge per
    /// event: a tier that lists an event replaces the lower tiers' list for
    /// it. An unknown event name is skipped with a warning; the rest still
    /// apply. Format: `docs/hooks.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_config: Option<serde_json::Value>,

    /// Which projects' own configuration takes effect as written — see
    /// `settings::trust`. Read from the global tier only.
    #[serde(default)]
    pub project_trust: trust::ProjectTrustSettings,

    /// How this process's own HTTP reaches the network. Read from the global
    /// and managed tiers only: the clients are process-wide.
    #[serde(default)]
    pub network: NetworkSettings,

    /// Where sessions are kept — see [`HistorySettings`]. Read from the
    /// global tier only: one store serves every project.
    #[serde(default)]
    pub history: HistorySettings,

    /// MCP server configurations, keyed by server name (merged from
    /// global/scene/project layers). `$VAR` / `${VAR}` / `${VAR:-default}`
    /// are expanded in a server's `command`, `args`, `env`, `url` and
    /// `headers` when it connects — the only place in settings that expands
    /// anything. Every variable of the daemon's environment read there, and
    /// every `*_env` of a server's `oauth` block, counts as that server's
    /// secret: no other child process inherits it (`sandbox.subprocess_env`).
    /// Kept untyped (`serde_json::Value`
    /// rather than `mcp::config::McpServerConfig`) — `core` cannot depend on
    /// `mcp` (which already depends on `core`, for `base::paths::ConfigPaths`)
    /// without a circular dependency; downstream consumers deserialize each
    /// value into `McpServerConfig` themselves.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mcp_servers: HashMap<String, serde_json::Value>,

    /// MCP servers turned off or back on by name, whichever tier or plugin
    /// declares them; a server not named here is on. Tiers merge by name, so
    /// a higher tier turns back on what a lower one turned off — except a
    /// project that is not trusted, which may only turn servers off.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mcp_enabled: HashMap<String, bool>,

    /// Multi-provider LLM registry, keyed by provider id.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub providers: HashMap<String, ProviderConfig>,
    /// Provider id used for any task with no `task_models` entry, and as the
    /// fallback target when an entry's provider/model turns out invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    /// Per-task-type provider/model routing — see `base::provider::resolve_task_models`.
    ///
    /// Keys: `main`, `subagent`, `compact`, `memory`, `title`, `web_search`,
    /// `web_fetch`. `web_fetch` alone has no default: `WebFetch` has a model
    /// read the page only when this names one, and otherwise returns the page
    /// text.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub task_models: HashMap<String, TaskModelOverride>,

    /// User language preference (e.g. "zh-CN", "ja").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,

    /// The directory at a repository's root the engine's git worktrees go
    /// in, which is also the line it adds to that repository's `.gitignore`:
    /// one path component starting with `.`, not starting with `.git`, not a
    /// configuration root nor another tool's directory
    /// (`base::paths::Layout::with_worktrees_dir`). Unset, `.wkts`. Read from
    /// the global and scene tiers only — a project's own files cannot move
    /// it; a name that is not allowed is ignored with a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_dir: Option<String>,

    /// The module-owned keys (`team`, `plugins`, `scripts`) of the sections
    /// the build registered — see [`section::Sections`]; read one with
    /// [`Settings::section`].
    #[serde(flatten)]
    #[schemars(skip)]
    pub sections: BTreeMap<String, serde_json::Value>,
}

fn default_memory_enabled() -> bool {
    true
}

/// A value in a settings file that does not fit its setting — `"language": 5`
/// — left out of the merge on its own, so the rest of its file and every
/// other tier still apply.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IgnoredValue {
    pub tier: &'static str,
    pub origin: String,
    /// Dotted, as `sandbox.network_mode`; empty when the file as a whole is
    /// not a settings object.
    pub key: String,
    pub error: String,
}

/// Output ceiling per model call when nothing configures one. Thinking counts
/// against it, so a small ceiling cuts replies off mid-thought; 32K is within
/// every current Claude model's limit.
pub const DEFAULT_MAX_TOKENS: u32 = 32_000;

/// LLM model configuration for a single provider.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[non_exhaustive]
pub struct ModelSettings {
    pub api_type: ApiType,
    pub base_url: String,
    /// Resolved model name (upper layer already resolved slot → name).
    pub model_name: String,
    pub max_tokens: u32,
    pub thinking_mode: ThinkingMode,
    /// Sent as `output_config.effort`; `None` leaves it to the endpoint.
    #[serde(default)]
    pub effort: Option<Effort>,
    /// Fallback model for persistent Overloaded/529 errors (e.g. Opus → Sonnet).
    /// None = no fallback.
    #[serde(default)]
    pub fallback_model: Option<String>,
    /// The most `max_tokens` a reply cut off at its limit is retried with.
    /// Unset, the model's documented limit, looked up by its name.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
}

/// Execution constraints.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ExecutionSettings {
    pub max_parallelism: usize,
    /// Ceiling on LLM round trips in one turn, before the turn ends with
    /// `stop_reason: "max_turns"`.
    ///
    /// This is a runaway guard, not a task budget. Every comparable agent
    /// runtime treats it that way and lands in the same range — the OpenAI
    /// Agents SDK defaults `max_turns` to 10, LangGraph's `recursion_limit`
    /// to 25, LangChain's `max_iterations` to 15. A long task is meant to be
    /// served by compaction, delegation and resuming, not by a high ceiling:
    /// raising this does not rescue an agent stuck in a loop, it only buys
    /// the loop more iterations before it stops.
    ///
    /// Raise it deliberately for a genuinely long single turn. Nothing here
    /// detects a *lack of progress* — a model repeating the same failing
    /// tool call burns the whole allowance either way.
    pub max_api_calls_per_turn: u32,
    /// Maximum cumulative tokens (input + output, summed across every API
    /// call in a turn) before the turn is aborted with `budget_exceeded`.
    /// `None` = unlimited.
    ///
    /// Deliberately token-based, not a dollar figure: provider prices change
    /// over time and per contract, so enforcing a dollar budget needs a
    /// per-model price table that can silently drift out of date — a stale
    /// or missing price makes the hard cap wrong in either direction. Token
    /// counts come straight from the provider's own usage accounting on
    /// every response, so there is nothing to keep in sync.
    pub max_budget_tokens: Option<u64>,
    /// How long a tool call waits for a host to answer a permission prompt
    /// before the call is denied. `0` waits indefinitely (only session
    /// cancellation interrupts it).
    ///
    /// The wait itself lives in the engine (`execute_tool_inner`), so the
    /// bound has to be reachable from `EngineConfig`, not only from a
    /// daemon-side flag — a host driving the engine directly needs it too.
    ///
    /// Failing closed (deny, not permit) on expiry is deliberate: a prompt
    /// that went unanswered is not consent.
    #[serde(default = "default_permission_prompt_timeout_secs")]
    pub permission_prompt_timeout_secs: u64,
    /// Sub-agents the model may have running at once anywhere under one
    /// session — foreground, background and resumed ones, at every depth.
    /// A spawn beyond it is refused with a message telling the model to wait
    /// or do the work itself. `TeamCreate` members and persistent team
    /// members have their own limits (`team.*`) and do not count.
    #[serde(default = "default_max_concurrent_subagents")]
    pub max_concurrent_subagents: usize,
    /// How many delegations deep a chain of sub-agents may go: a session a
    /// host created is depth 0, and an agent at this depth is not offered
    /// the `Agent` tool. Applies to team members too.
    #[serde(default = "default_max_agent_depth")]
    pub max_agent_depth: u32,
}

fn default_max_agent_depth() -> u32 {
    3
}

fn default_permission_prompt_timeout_secs() -> u64 {
    300
}

fn default_max_concurrent_subagents() -> usize {
    6
}

impl Default for ExecutionSettings {
    fn default() -> Self {
        Self {
            max_parallelism: 10,
            max_api_calls_per_turn: 25,
            max_budget_tokens: None,
            permission_prompt_timeout_secs: default_permission_prompt_timeout_secs(),
            max_concurrent_subagents: default_max_concurrent_subagents(),
            max_agent_depth: default_max_agent_depth(),
        }
    }
}

/// Context compaction, over what the scene asks for. A field left out keeps
/// the scene's own value (`SceneSpec::token_budget`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[non_exhaustive]
pub struct CompactionConfig {
    /// Compact once a request's context passes this many tokens. `0` turns
    /// automatic compaction off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_tokens: Option<usize>,
    /// How many recent rounds compaction leaves as they are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent: Option<usize>,
    /// Clear old tool results early, in the middle of a turn: once fewer than
    /// 50,000 tokens are left below the threshold, or the context grows fast
    /// enough to reach it within three model calls, the results of file,
    /// shell and search tools older than the last `keep_recent` rounds (at
    /// least five) are blanked — a `MicroCompact`, reported as a `compact`
    /// event. Stops for the session after three passes in a row that freed
    /// nothing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reactive: bool,
    /// Once an hour — the lifetime of the provider's prompt cache — blank all
    /// but the 20 newest tool results and send the provider `cache_edits`
    /// for the ones it still holds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cached_microcompact: bool,
    /// A tool result longer than this many characters is saved to a file in
    /// the project's `state/tool-results/` and the model gets its head, its
    /// tail and the path (default 30,000; `0` keeps every result in the
    /// conversation).
    /// `Read`, `WebFetch` and `TaskOutput` page their own results and are
    /// never saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_inline_chars: Option<usize>,
}

/// Egress of the engine's own HTTP: model calls, `WebFetch` / `Ping`, MCP over
/// HTTP, OAuth. Not the network of a sandboxed command, which
/// `sandbox.network_mode` and `sandbox.egress_proxy` decide.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct NetworkSettings {
    /// Follow `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` from
    /// the environment (default). `false` connects directly, ignoring them.
    /// Requests to this machine are direct either way. Takes effect when the
    /// daemon starts; `config.reload` does not apply a change.
    pub use_system_proxy: bool,
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self {
            use_system_proxy: true,
        }
    }
}

/// Where sessions' transcripts and private files are kept.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct HistorySettings {
    /// `global` (default): under the state root's `projects/` and
    /// `sessions/`. `project`: a session of a trusted project goes to that
    /// project's `<project dir>/state/history/<id>/` (a worktree's to its main
    /// checkout's), with only an index under `sessions/<id>/`; the rest stay
    /// global. What is already kept either way is read where it is, and
    /// history kept in a project is read only while the project is trusted.
    /// Takes effect when the daemon starts.
    pub location: HistoryLocation,
}

/// [`HistorySettings::location`].
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum HistoryLocation {
    #[default]
    Global,
    Project,
}

/// Sandbox/security configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[non_exhaustive]
pub struct SandboxConfig {
    /// Directories outside the project that tools may read and write, as if
    /// they were part of it. The instance's own state directory (`~/.agix`
    /// by default) is always readable, but only its `scratchpad` and
    /// `memory` directories are writable this way.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<PathBuf>,
    /// Directories outside the project that the file tools (`Read`, `Glob`,
    /// `Grep`) and read-only shell commands may read without asking. Reading
    /// anywhere else needs a person's approval, and an unattended session —
    /// one with nobody to approve — is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_directories: Vec<PathBuf>,
    /// Let the file tools read anywhere, as `read_directories` would if it
    /// listed `/`. Off by default.
    #[serde(default)]
    pub read_anywhere: bool,
    /// Package and build caches outside the project that a sandboxed `Bash`
    /// command may write — `pip`, `npm`, `cargo` and the like fail without
    /// them. Grouped by toolchain; a settings file replaces one group by
    /// naming it (`"python": []` drops it), adds a group under a new name, and
    /// leaves the rest at their defaults. `~/` means the user's home. The file
    /// tools do not get these.
    #[serde(default)]
    pub cache_directories: CacheDirectories,
    /// Paths a sandboxed `Bash` / `Monitor` command may not read. Empty — the
    /// default — means the built-in credential list (`~/.ssh`, `~/.aws`,
    /// `~/.npmrc`, ...); a non-empty list replaces that list as a whole
    /// rather than adding to it, so repeat any of it you still want. The
    /// `Read(...)` deny rules that name a file or a directory are added on
    /// top; one with any other glob (`**/*.pem`) is not, so name its files
    /// here or deny them with a `Bash(...)` rule.
    pub deny_read: Vec<PathBuf>,
    /// Paths to re-allow reading, on top of the built-in credential deny
    /// defaults (`~/.ssh`, `~/.aws`, `~/.npmrc`, ...). It re-allows exactly the
    /// entry it names; a parent directory, or a path below a denied one,
    /// changes nothing. Never a path a `Read(...)` deny rule names.
    ///
    /// Exists because those defaults are applied to `Bash`, and some of them
    /// sit on legitimate workflows — `npm install` against a private registry
    /// reads `~/.npmrc`, `docker build` reads `~/.docker/config.json`.
    /// This is the way to say "yes, that one is fine here".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_read: Vec<PathBuf>,
    /// Extra filename segments a tool may never write, on top of the built-in
    /// credential defaults (`.env`, `id_rsa`, `.ssh`, `.npmrc`, ...). A path is
    /// refused if *any* of its components matches one, or begins with one
    /// followed by a dot — so `.env` also covers `.env.production`. A
    /// sandboxed `Bash` / `Monitor` command is held to the same names inside
    /// its writable roots (on Linux only for entries that already exist).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_write: Vec<String>,
    /// Paths to re-allow writing, on top of the built-in deny defaults. Most
    /// specific wins.
    ///
    /// The write-side counterpart of [`allow_read`](Self::allow_read), and it
    /// exists for the same reason: the defaults sit on real workflows. A
    /// project that keeps a checked-in `.env.example`, or a deployment whose
    /// agent is meant to maintain a private-registry `.npmrc`, has to be able
    /// to say "that one is fine here".
    ///
    /// This exempts a path from the *deny* rules only. Where a tool may write
    /// at all is a different question, answered by `additional_directories` —
    /// keeping them apart is what stops one setting quietly widening the
    /// project boundary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_write: Vec<PathBuf>,
    /// Hosts the model may reach (`WebFetch`, `Ping`); a name also covers its
    /// subdomains. Under `network_mode: "allowlist"` it is the whole list, but
    /// a sandboxed `Bash` / `Monitor` command reaches none of its domains: the
    /// OS sandbox cannot filter by domain, and lets through only localhost
    /// (macOS, when listed) or nothing (Linux). In every mode, a host it
    /// names may be reached at an address on this machine or a private
    /// network, which a model-directed request is otherwise refused — write
    /// `127.0.0.1` here to let `WebFetch` reach a local server.
    pub allowed_domains: Vec<String>,
    /// Outbound network policy for sandboxed `Bash` commands. `proxy` lets a
    /// command connect only to `egress_proxy`, which both OS backends hold;
    /// filtering by domain is then the proxy's job. The engine's own
    /// `WebFetch` and `Ping` are not bound by it: they use the engine's
    /// network, not the proxy, and are not held to `allowed_domains` either.
    #[serde(default)]
    pub network_mode: base::interface::exec::NetworkMode,
    /// Under `network_mode: "proxy"`, the one address a sandboxed command may
    /// connect to: `"127.0.0.1:3128"` for an HTTP proxy, or an object with
    /// `http`, an optional `socks` and `no_proxy`. The command gets
    /// `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY` (both cases)
    /// pointing at it. Put the proxy on this machine: macOS can hold a proxy
    /// elsewhere only to its port, and Linux needs `socat` to relay to it,
    /// cutting the network entirely without one. Ignored in other modes.
    /// Only sandboxed commands go through it: the engine's own `WebFetch` and
    /// `Ping` use the engine's network, not this proxy, and are not held to
    /// `allowed_domains`; filtering by domain is the host's proxy's job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_proxy: Option<EgressProxySetting>,
    /// Bypass the sandbox entirely, in a build that has one.
    ///
    /// Default: false. The backend is behind the `sandbox` compile-time
    /// feature, which is on by default; a build without it has nothing to
    /// bypass and this setting changes nothing in it. It is the runtime half
    /// of a two-part answer — the build decides whether a sandbox exists, this
    /// decides whether to use the one it has.
    #[serde(default)]
    pub dangerously_disable_sandbox: bool,
    /// Refuse to run a sandboxed command when this build on this platform
    /// cannot actually enforce the policy, instead of running it
    /// unconstrained.
    ///
    /// Two ways to be unable: the `sandbox` compile-time feature (on by
    /// default) was turned off, so no backend was compiled at all, or it is
    /// on and the platform's tool is missing. Both refuse here, and the
    /// message says which.
    ///
    /// Off by default, which is the historical behavior: on Linux without
    /// `bwrap`, and on platforms with no backend at all, a command whose
    /// policy cannot be honored runs anyway with a warning. That is a silent
    /// downgrade from "constrained" to "unconstrained" — fine for a laptop,
    /// not fine for a host that treats the sandbox as a boundary.
    ///
    /// Turning it on makes such a command fail with an error naming the
    /// missing backend. The default is `false` rather than `true` only
    /// because flipping it would break every Linux box without bubblewrap
    /// installed, and every Windows host, on upgrade.
    #[serde(default)]
    pub require_enforcement: bool,
    /// In an unattended session, a `Bash` / `Monitor` command nobody can be
    /// asked about runs without an allow rule when the sandbox fully holds
    /// it. With this on, it also needs `network_mode` other than
    /// `unrestricted`: the sandbox holds writes, and an open network is a way
    /// out it does not cover. Off by default in 0.3.x, which keeps the
    /// earlier behavior; a later 0.3.x release is planned to turn it on.
    #[serde(default)]
    pub unattended_requires_network_policy: bool,
    /// The environment `Bash` / `Monitor` commands, command hooks and MCP
    /// stdio servers start from, sandboxed or not. By default the host's
    /// environment minus the engine's own credentials: `ANTHROPIC_*`,
    /// `AGIX_DAEMON_TOKEN`, and every variable a setting names as holding a
    /// secret — a provider's `api_key_env`, an MCP server's
    /// `oauth.client_secret_env` / `oauth.private_key_jwt.key_env` and every
    /// `$VAR` in its `command`, `args`, `env`, `url` or `headers` (outside
    /// the project's own tier), a plugin's MCP servers likewise, the
    /// enterprise IdP's `client_secret_env`. `"mode": "allowlist"` keeps only
    /// `PATH`, `HOME`, the locale and a few others, plus whatever `allow`
    /// names. An MCP server takes the policy of the settings it is declared
    /// in, with its own `env` added on top: what it is given there it gets,
    /// another server's secret it does not.
    #[serde(default)]
    pub subprocess_env: base::interface::exec::SubprocessEnv,
    /// Stop, when a daemon session ends (closed, evicted, or the daemon
    /// shutting down), every process group its commands started that is still
    /// running — including services a command deliberately left in the
    /// background. Off by default: such a service outlives the session.
    #[serde(default)]
    pub reap_process_groups: bool,
}

/// [`SandboxConfig::cache_directories`]: toolchain name → cache paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct CacheDirectories(pub BTreeMap<String, Vec<String>>);

impl Default for CacheDirectories {
    fn default() -> Self {
        let groups: &[(&str, &[&str])] = &[
            (
                "python",
                &[
                    "~/.cache/pip",
                    "~/.cache/uv",
                    "~/.cache/pypoetry",
                    "~/Library/Caches/pip",
                    "~/Library/Caches/pypoetry",
                    "~/Library/Caches/com.apple.python",
                ],
            ),
            (
                "node",
                &[
                    "~/.npm",
                    "~/.cache/yarn",
                    "~/Library/Caches/Yarn",
                    "~/.cache/node-gyp",
                    "~/Library/Caches/node-gyp",
                    "~/.local/share/pnpm",
                    "~/Library/pnpm",
                    "~/.bun/install/cache",
                ],
            ),
            (
                "rust",
                &[
                    "~/.cargo/registry",
                    "~/.cargo/git",
                    "~/.cargo/.package-cache",
                    "~/.cargo/.global-cache",
                ],
            ),
            (
                "go",
                &[
                    "~/go/pkg/mod",
                    "~/.cache/go-build",
                    "~/Library/Caches/go-build",
                ],
            ),
            (
                "java",
                &["~/.m2/repository", "~/.gradle/caches", "~/.gradle/wrapper"],
            ),
            ("ruby", &["~/.gem", "~/.bundle/cache"]),
            ("dotnet", &["~/.nuget/packages"]),
            (
                "swift",
                &[
                    "~/Library/Caches/org.swift.swiftpm",
                    "~/Library/org.swift.swiftpm",
                ],
            ),
        ];
        Self(
            groups
                .iter()
                .map(|(name, dirs)| {
                    (
                        name.to_string(),
                        dirs.iter().map(|d| d.to_string()).collect(),
                    )
                })
                .collect(),
        )
    }
}

impl CacheDirectories {
    /// Every listed path: `~/…` kept as it is for the command runner to
    /// expand, anything else relative resolved against `project`.
    pub fn paths(&self, project: &Path) -> Vec<PathBuf> {
        self.0
            .values()
            .flatten()
            .map(|dir| {
                if dir.starts_with("~/") {
                    PathBuf::from(dir)
                } else {
                    project.join(dir)
                }
            })
            .collect()
    }
}

fn default_true() -> bool {
    true
}

impl Settings {
    /// Validate settings consistency. Returns Err on invalid combinations.
    pub fn validate(&self) -> Result<(), String> {
        if !self.permission_mode.is_user_settable() {
            return Err(format!(
                "Permission mode '{:?}' is program-only and cannot be set in settings.json. \
                 Defaulting to Default.",
                self.permission_mode
            ));
        }
        Ok(())
    }

    /// Quick default for a given model name.
    pub fn defaults_for(model_name: &str) -> Self {
        Self {
            model: ModelSettings {
                api_type: ApiType::Anthropic,
                base_url: String::new(),
                model_name: model_name.to_string(),
                max_tokens: DEFAULT_MAX_TOKENS,
                thinking_mode: ThinkingMode::On,
                effort: Some(Effort::High),
                fallback_model: None,
                max_output_tokens: None,
            },
            execution: ExecutionSettings::default(),
            compaction: CompactionConfig::default(),
            sandbox: SandboxConfig::default(),
            instruction_file: None,
            prompt_append: None,
            prompt_override: None,
            recorder: None,
            telemetry_url: None,
            telemetry_enabled: true,
            allow_client_permission_override: false,
            memory_enabled: true,
            disable_skill_shell_execution: false,
            path_style: Default::default(),
            permission_mode: PermissionMode::default(),
            hooks_config: None,
            project_trust: Default::default(),
            network: Default::default(),
            history: Default::default(),
            mcp_servers: HashMap::new(),
            mcp_enabled: HashMap::new(),
            providers: HashMap::new(),
            default_provider: None,
            task_models: HashMap::new(),
            language: None,
            worktree_dir: None,
            sections: BTreeMap::new(),
        }
    }

    /// The section `S`, or `S::default()` when the settings carry none.
    pub fn section<S: section::Section>(&self) -> S {
        let Some(value) = self.sections.get(S::KEY) else {
            return S::default();
        };
        serde_json::from_value(value.clone()).unwrap_or_else(|error| {
            tracing::warn!(key = S::KEY, %error, "settings section does not read, using its defaults");
            S::default()
        })
    }

    pub fn set_section<S: section::Section>(&mut self, value: &S) {
        match serde_json::to_value(value) {
            Ok(value) => {
                self.sections.insert(S::KEY.to_string(), value);
            }
            Err(error) => {
                tracing::warn!(key = S::KEY, %error, "settings section does not serialize, leaving it as it was")
            }
        }
    }
}
