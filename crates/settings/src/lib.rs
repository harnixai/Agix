//! The settings types: what one settings.json file may say and what the tiers
//! say once merged ([`Settings`]), and what loading them produced
//! ([`LoadedSettings`] — the values, where this scene's data lives, each
//! permission rule with its tier, the managed layer and the project-trust
//! decision).
//!
//! [`LoadedSettings::load`] is the single canonical settings.json loader
//! (global → scene → project, generic recursive JSON merge). Everything
//! downstream (the AGENT itself, `Builder`) receives the already-merged
//! result; it does not perform its own multi-layer config merging.

use base::interface::model::{Effort, ThinkingMode};
use base::provider::{ProviderConfig, ResolvedTiers, TaskTiers};
use base::recorder::RecorderConfig;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod blocks;
pub use blocks::{
    AgentsSettings, AppSettings, FilesSettings, HistoryLocation, McpServerEntry, McpSettings,
    MemorySettings, PromptSettings, SkillsSettings, TelemetrySettings, TurnSettings, WebSettings,
};
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
pub mod permissions;
pub mod quota;
pub mod renamed;
mod schema;
pub mod section;
mod source;
pub mod trust;

#[cfg(test)]
mod tests;

pub use loaded::{LoadedSettings, ProviderOrigin};
pub use merge::merge_json_values;
pub use permissions::PermissionsSettings;
pub use schema::{settings_json_schema, unknown_settings_keys, KeyKind, SchemaKeys};
pub use source::{read_settings_file, FileTiers};

/// The keys only the global tier's files may set; the loader drops them from
/// every other tier.
pub const GLOBAL_ONLY_KEYS: &[&str] = &["app"];

/// One settings.json file, and what the settings tiers say once merged. Every
/// field is one a settings file can set; what loading them produced besides is
/// [`LoadedSettings`].
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[non_exhaustive]
pub struct Settings {
    pub model: ModelSettings,

    /// The permission mode, the allow / ask / deny rules, and how long a
    /// permission prompt waits.
    #[serde(default)]
    pub permissions: PermissionsSettings,
    #[serde(default)]
    pub files: FilesSettings,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub web: WebSettings,
    #[serde(default)]
    pub turn: TurnSettings,
    #[serde(default)]
    pub agents: AgentsSettings,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub prompt: PromptSettings,

    /// Lifecycle hooks: an object from event name (`PreToolUse`,
    /// `SessionStart`, … — `hooks::HookEvent`) to a list of hooks, each with
    /// a `type` of `command`, `prompt`, `http` or `agent`. Tiers merge per
    /// event: a tier that lists an event replaces the lower tiers' list for
    /// it. An unknown event name is skipped with a warning; the rest still
    /// apply. Format: `docs/hooks.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<serde_json::Value>,
    #[serde(default)]
    pub mcp: McpSettings,
    #[serde(default)]
    pub skills: SkillsSettings,
    #[serde(default)]
    pub memory: MemorySettings,
    #[serde(default)]
    pub telemetry: TelemetrySettings,

    /// LLM interaction recording. None = pass-through.
    #[serde(default)]
    pub recorder: Option<RecorderConfig>,

    /// Read from the global tier only — see [`AppSettings`].
    #[serde(default)]
    pub app: AppSettings,

    /// The model providers: each one's endpoint, models, tiers and quota.
    /// Tiers add to the list rather than replace it; an id already defined
    /// in a tier read earlier (global, scene, project — each tier's
    /// `settings.local.json` after its `settings.json`) is ignored with a
    /// warning. With none defined, the engine supplies `anthropic`: Anthropic's
    /// API, keyed from `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` or the
    /// host, at `ANTHROPIC_BASE_URL` when set, every tier the daemon's
    /// `--model`. Not read from a project that is not trusted.
    #[serde(
        default,
        skip_serializing_if = "no_defined_providers",
        serialize_with = "defined_providers"
    )]
    pub providers: Vec<ProviderConfig>,

    /// The module-owned keys (`team`, `plugins`, `scripts`) of the sections
    /// the build registered — see [`section::Sections`]; read one with
    /// [`Settings::section`].
    #[serde(flatten)]
    #[schemars(skip)]
    pub sections: BTreeMap<String, serde_json::Value>,
}

fn no_defined_providers(providers: &[ProviderConfig]) -> bool {
    providers.iter().all(|p| p.builtin)
}

/// The providers a file defines: not the one the engine supplies.
fn defined_providers<S: serde::Serializer>(
    providers: &[ProviderConfig],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(providers.iter().filter(|p| !p.builtin))
}

/// A value in a settings file that does not fit its setting — `"memory": {"enabled": 5}`
/// — left out of the merge on its own, so the rest of its file and every
/// other tier still apply.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IgnoredValue {
    pub tier: &'static str,
    pub origin: String,
    /// Dotted, as `sandbox.network.mode`; empty when the file as a whole is
    /// not a settings object.
    pub key: String,
    pub error: String,
}

/// Output ceiling per model call when nothing configures one. Thinking counts
/// against it, so a small ceiling cuts replies off mid-thought; 32K is within
/// every current Claude model's limit.
pub const DEFAULT_MAX_TOKENS: u32 = 32_000;

/// Which providers model calls go to, and how they are made.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ModelSettings {
    /// Provider ids, from `providers`, in the order a model call tries them:
    /// the first is the default, and a call goes to the next when one is out
    /// of quota, has no key, refuses the key, or cannot be reached once its
    /// retries are spent. Every call starts from the first again. Each id must
    /// be defined. Unset, the first defined provider alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
    /// The tier — `strong`, `balanced` or `fast` — each kind of model call
    /// asks the provider for.
    #[serde(default)]
    pub tasks: TaskTiers,
    /// Sent as `output_config.effort`; `None` leaves it to the endpoint.
    #[serde(default)]
    pub effort: Option<Effort>,
    /// `auto`, `on` or `off`.
    #[serde(rename = "thinking")]
    pub thinking_mode: ThinkingMode,
    pub max_tokens: u32,
    /// The conversation's model: what `tasks.main` is at the first provider
    /// of `order`, or a session's own choice. Not read from a file.
    #[serde(skip)]
    #[schemars(skip)]
    pub model_name: String,
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
    pub clear_on_cache_expiry: bool,
    /// A tool result longer than this many characters is saved to a file in
    /// the project's `state/tool-results/` and the model gets its head, its
    /// tail and the path (default 30,000; `0` keeps every result in the
    /// conversation).
    /// `Read`, `WebFetch` and `TaskOutput` page their own results and are
    /// never saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_result_chars: Option<usize>,
}

/// The OS sandbox `Bash` / `Monitor` commands run in.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[non_exhaustive]
pub struct SandboxConfig {
    /// Run commands in the sandbox, in a build that has one.
    ///
    /// Default: true. The backend is behind the `sandbox` compile-time
    /// feature, which is on by default; a build without it has nothing to
    /// turn off and this setting changes nothing in it. It is the runtime half
    /// of a two-part answer — the build decides whether a sandbox exists, this
    /// decides whether to use the one it has.
    pub enabled: bool,
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
    pub require: bool,
    /// Paths to re-allow reading, on top of the built-in credential deny
    /// defaults (`~/.ssh`, `~/.aws`, `~/.npmrc`, ...). It re-allows exactly the
    /// entry it names; a parent directory, or a path below a denied one,
    /// changes nothing. Never a path a `Read(...)` deny rule names.
    ///
    /// Exists because those defaults are applied to `Bash`, and some of them
    /// sit on legitimate workflows — `npm install` against a private registry
    /// reads `~/.npmrc`, `docker build` reads `~/.docker/config.json`.
    /// This is the way to say "yes, that one is fine here".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow_read: Vec<PathBuf>,
    /// Paths a sandboxed `Bash` / `Monitor` command may not read, added to
    /// the built-in credential list (`~/.ssh`, `~/.aws`, `~/.npmrc`, ...);
    /// `allow_read` is how to let one of those through. The `Read(...)` deny
    /// rules that name a file or a directory are added on top; one with any
    /// other glob (`**/*.pem`) is not, so name its files here or deny them
    /// with a `Bash(...)` rule.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deny_read: Vec<PathBuf>,
    pub network: SandboxNetwork,
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
    pub env: base::interface::exec::SubprocessEnv,
    /// Package and build caches outside the project that a sandboxed `Bash`
    /// command may write — `pip`, `npm`, `cargo` and the like fail without
    /// them. Grouped by toolchain; a settings file replaces one group by
    /// naming it (`"python": []` drops it), adds a group under a new name, and
    /// leaves the rest at their defaults. `~/` means the user's home. The file
    /// tools do not get these.
    pub cache_dirs: CacheDirectories,
    /// Stop, when a daemon session ends (closed, evicted, or the daemon
    /// shutting down), every process group its commands started that is still
    /// running — including services a command deliberately left in the
    /// background. Off by default: such a service outlives the session.
    pub reap_on_close: bool,
    /// In an unattended session, a `Bash` / `Monitor` command nobody can be
    /// asked about runs without an allow rule when the sandbox fully holds
    /// it. With this on, it also needs `network.mode` other than
    /// `unrestricted`: the sandbox holds writes, and an open network is a way
    /// out it does not cover. Off by default in 0.3.x, which keeps the
    /// earlier behavior; a later 0.3.x release is planned to turn it on.
    pub unattended_needs_network_policy: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            require: false,
            allow_read: Vec::new(),
            deny_read: Vec::new(),
            network: SandboxNetwork::default(),
            env: Default::default(),
            cache_dirs: CacheDirectories::default(),
            reap_on_close: false,
            unattended_needs_network_policy: false,
        }
    }
}

/// The network of a sandboxed command. Not the engine's own HTTP, which
/// `app.system_proxy` decides.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[non_exhaustive]
pub struct SandboxNetwork {
    /// Outbound network policy for sandboxed `Bash` commands. `allowlist`
    /// lets through what `web.allowed_domains` names, as far as the OS
    /// sandbox can; `proxy` lets a command connect only to `proxy`, which
    /// both OS backends hold; filtering by domain is then the proxy's job.
    /// The engine's own `WebFetch` and `Ping` are not bound by it: they use
    /// the engine's network, not the proxy.
    pub mode: base::interface::exec::NetworkMode,
    /// Under `mode: "proxy"`, the one address a sandboxed command may
    /// connect to: `"127.0.0.1:3128"` for an HTTP proxy, or an object with
    /// `http`, an optional `socks` and `no_proxy`. The command gets
    /// `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY` (both cases)
    /// pointing at it. Put the proxy on this machine: macOS can hold a proxy
    /// elsewhere only to its port, and Linux needs `socat` to relay to it,
    /// cutting the network entirely without one. Ignored in other modes.
    /// Only sandboxed commands go through it: the engine's own `WebFetch` and
    /// `Ping` use the engine's network, not this proxy, and are not held to
    /// `web.allowed_domains`; filtering by domain is the host's proxy's job.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<EgressProxySetting>,
}

/// [`SandboxConfig::cache_dirs`]: toolchain name → cache paths.
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

impl Settings {
    /// Validate settings consistency. Returns Err on invalid combinations.
    pub fn validate(&self) -> Result<(), String> {
        if !self.permissions.mode.is_user_settable() {
            return Err(format!(
                "Permission mode '{:?}' is program-only and cannot be set in settings.json. \
                 Defaulting to Default.",
                self.permissions.mode
            ));
        }
        Ok(())
    }

    /// Quick default for a given model name.
    pub fn defaults_for(model_name: &str) -> Self {
        Self {
            model: ModelSettings {
                order: Vec::new(),
                tasks: TaskTiers::default(),
                model_name: model_name.to_string(),
                max_tokens: DEFAULT_MAX_TOKENS,
                thinking_mode: ThinkingMode::On,
                effort: Some(Effort::High),
            },
            permissions: PermissionsSettings::default(),
            files: FilesSettings::default(),
            sandbox: SandboxConfig::default(),
            web: WebSettings::default(),
            turn: TurnSettings::default(),
            agents: AgentsSettings::default(),
            compaction: CompactionConfig::default(),
            prompt: PromptSettings::default(),
            hooks: None,
            mcp: McpSettings::default(),
            skills: SkillsSettings::default(),
            memory: MemorySettings::default(),
            telemetry: TelemetrySettings::default(),
            recorder: None,
            app: AppSettings::default(),
            providers: Vec::new(),
            sections: BTreeMap::new(),
        }
    }

    /// The providers model calls go through, in `model.order`, each with its
    /// tiers filled — or what is wrong with them.
    pub fn routing(&self) -> Result<Vec<(&ProviderConfig, ResolvedTiers)>, String> {
        base::provider::routing_order(&self.providers, &self.model.order)
    }

    /// What `models` says about `model` at the first provider that lists it.
    pub fn model_spec(&self, model: &str) -> Option<&base::provider::ModelSpec> {
        self.providers.iter().find_map(|p| p.model_spec(model))
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.id == id)
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
