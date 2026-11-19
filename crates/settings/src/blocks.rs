//! The smaller settings blocks: where the tools may read and write (`files`),
//! what the model may reach (`web`), the limits of a turn and of delegation
//! (`turn`, `agents`), the system prompt (`prompt`), MCP servers (`mcp`),
//! skills, memory, telemetry, and the instance-wide `app` block only the global
//! tier sets.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Where the tools may read and write beyond the project.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[non_exhaustive]
pub struct FilesSettings {
    /// Directories outside the project that tools may read and write, as if
    /// they were part of it. The instance's own state directory (`~/.agix`
    /// by default) is always readable, but only its `scratchpad` and
    /// `memory` directories are writable this way.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_dirs: Vec<PathBuf>,
    /// Directories outside the project that the file tools (`Read`, `Glob`,
    /// `Grep`) and read-only shell commands may read without asking. `"/"`
    /// lets them read anywhere. Reading anywhere else needs a person's
    /// approval, and an unattended session — one with nobody to approve — is
    /// refused.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub read_dirs: Vec<PathBuf>,
    /// Paths to re-allow writing, on top of the built-in deny defaults. Most
    /// specific wins.
    ///
    /// The write-side counterpart of `sandbox.allow_read`, and it exists for
    /// the same reason: the defaults sit on real workflows. A project that
    /// keeps a checked-in `.env.example`, or a deployment whose agent is
    /// meant to maintain a private-registry `.npmrc`, has to be able to say
    /// "that one is fine here".
    ///
    /// This exempts a path from the *deny* rules only. Where a tool may write
    /// at all is a different question, answered by `extra_dirs` — keeping
    /// them apart is what stops one setting quietly widening the project
    /// boundary.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow_write: Vec<PathBuf>,
    /// Extra filename segments a tool may never write, on top of the built-in
    /// credential defaults (`.env`, `id_rsa`, `.ssh`, `.npmrc`, ...). A path is
    /// refused if *any* of its components matches one, or begins with one
    /// followed by a dot — so `.env` also covers `.env.production`. A
    /// sandboxed `Bash` / `Monitor` command is held to the same names inside
    /// its writable roots (on Linux only for entries that already exist).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deny_write: Vec<String>,
}

impl FilesSettings {
    pub fn reads_anywhere(&self) -> bool {
        self.read_dirs.iter().any(|dir| dir == Path::new("/"))
    }

    /// `read_dirs` without the `"/"` that stands for anywhere.
    pub fn read_dirs_listed(&self) -> impl Iterator<Item = &PathBuf> {
        self.read_dirs.iter().filter(|dir| *dir != Path::new("/"))
    }
}

/// What the model's own web tools may reach.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[non_exhaustive]
pub struct WebSettings {
    /// Hosts the model may reach (`WebFetch`, `Ping`); a name also covers its
    /// subdomains. Under `sandbox.network.mode: "allowlist"` it is the whole
    /// list, but a sandboxed `Bash` / `Monitor` command reaches none of its
    /// domains: the OS sandbox cannot filter by domain, and lets through only
    /// localhost (macOS, when listed) or nothing (Linux). In every mode, a
    /// host it names may be reached at an address on this machine or a
    /// private network, which a model-directed request is otherwise refused —
    /// write `127.0.0.1` here to let `WebFetch` reach a local server.
    pub allowed_domains: Vec<String>,
}

/// The limits of one turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct TurnSettings {
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
    pub max_model_calls: u32,
    /// Maximum cumulative tokens (input + output, cache not counted) one
    /// turn may spend, its delegates included: every model call of the turn
    /// and of the sub-agents and team members it starts or hands work to —
    /// a background sub-agent's after the turn has ended too — counts toward
    /// one total. Each call asks for no more output than is left; with less
    /// than 1024 left no call is sent. The turn ends with `budget_exceeded`,
    /// and a delegate still running ends the same way before its next call.
    /// `None` = unlimited.
    ///
    /// Deliberately token-based, not a dollar figure: provider prices change
    /// over time and per contract, so enforcing a dollar budget needs a
    /// per-model price table that can silently drift out of date — a stale
    /// or missing price makes the hard cap wrong in either direction. Token
    /// counts come straight from the provider's own usage accounting on
    /// every response, so there is nothing to keep in sync.
    pub token_budget: Option<u64>,
    /// How many tool calls of one model reply run at once.
    pub parallel_tools: usize,
}

impl Default for TurnSettings {
    fn default() -> Self {
        Self {
            max_model_calls: 25,
            token_budget: None,
            parallel_tools: 10,
        }
    }
}

/// How far and how wide delegation to sub-agents may go.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct AgentsSettings {
    /// How many delegations deep a chain of sub-agents may go: a session a
    /// host created is depth 0, and an agent at this depth is not offered
    /// the `Agent` tool. Applies to team members too.
    pub max_depth: u32,
    /// Sub-agents the model may have running at once anywhere under one
    /// session — foreground, background and resumed ones, at every depth.
    /// A spawn beyond it is refused with a message telling the model to wait
    /// or do the work itself. `TeamCreate` members and persistent team
    /// members have their own limits (`team.*`) and do not count.
    pub max_concurrent: usize,
}

impl Default for AgentsSettings {
    fn default() -> Self {
        Self {
            max_depth: 3,
            max_concurrent: 6,
        }
    }
}

/// What the system prompt says, over what the scene gives it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[non_exhaustive]
pub struct PromptSettings {
    /// Appended to the end of the system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append: Option<String>,
    /// Overrides the entire system prompt if set.
    #[serde(rename = "override", skip_serializing_if = "Option::is_none")]
    pub override_text: Option<String>,
    /// More instruction files (e.g. a `CLAUDE.md` outside the project), read
    /// in order when the session is built and given to the model alongside
    /// the discovered `AGENTS.md` files. A root session with a `FileChanged`
    /// hook watches them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub instruction_files: Vec<PathBuf>,
    /// User language preference (e.g. "zh-CN", "ja").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// How the file tools word paths for the model. `absolute` keeps
    /// today's text; `relative` makes tool descriptions say paths are
    /// relative to the working directory and prints Grep / Glob / Write /
    /// Edit paths under it relative, so a recorded session replays in another
    /// working directory and long paths stop repeating in every call.
    /// Resolving paths is unchanged. Unset, the scene decides
    /// (`SceneSpec::path_style`: `harnix` is relative, the rest absolute).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_style: Option<base::context::config::PathStyle>,
}

/// MCP servers, keyed by server name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[non_exhaustive]
pub struct McpSettings {
    /// Tiers merge by name and field, so a higher tier can change one field
    /// of a server a lower one declared. An entry holding nothing but
    /// `enabled` declares nothing: it turns off, or back on, the server of
    /// that name declared in a lower tier or by a plugin — except in a
    /// project that is not trusted, which may only turn servers off.
    ///
    /// `$VAR` / `${VAR}` / `${VAR:-default}` are expanded in a server's
    /// `command`, `args`, `env`, `url` and `headers` when it connects — the
    /// only place in settings that expands anything. Every variable of the
    /// daemon's environment read there, and every `*_env` of a server's
    /// `oauth` block, counts as that server's secret: no other child process
    /// inherits it (`sandbox.env`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<String, McpServerEntry>,
}

/// One `mcp.servers` entry: a server's configuration (`mcp::config::McpServerConfig`,
/// read by the MCP module, which this crate cannot depend on), and whether it is on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[non_exhaustive]
pub struct McpServerEntry {
    /// Unset is on. `null` is a bad value rather than unset: it would
    /// otherwise override a lower tier's `false` and read as on.
    #[serde(
        default,
        deserialize_with = "present_bool",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "bool")]
    pub enabled: Option<bool>,
    #[serde(flatten)]
    pub config: Map<String, Value>,
}

fn present_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    bool::deserialize(d).map(Some)
}

impl McpServerEntry {
    /// Whether the entry declares a server, rather than only switching one.
    pub fn declares(&self) -> bool {
        !self.config.is_empty()
    }

    pub fn config_value(&self) -> Value {
        Value::Object(self.config.clone())
    }
}

impl McpSettings {
    /// The servers these settings declare, by name, as JSON for the MCP
    /// module to read.
    pub fn declared(&self) -> impl Iterator<Item = (&String, Value)> {
        self.servers
            .iter()
            .filter(|(_, entry)| entry.declares())
            .map(|(name, entry)| (name, entry.config_value()))
    }

    pub fn declares(&self, name: &str) -> bool {
        self.servers.get(name).is_some_and(McpServerEntry::declares)
    }

    /// Whether the server called `name` is on, wherever it is declared.
    pub fn is_enabled(&self, name: &str) -> bool {
        self.servers
            .get(name)
            .and_then(|entry| entry.enabled)
            .unwrap_or(true)
    }

    /// The names an entry turns off.
    pub fn disabled(&self) -> impl Iterator<Item = &String> {
        self.servers
            .iter()
            .filter(|(_, entry)| entry.enabled == Some(false))
            .map(|(name, _)| name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[non_exhaustive]
pub struct SkillsSettings {
    /// Run skill dynamic-content-injection shell commands (`` !`cmd` `` and
    /// fenced ` ```! ` blocks in `SKILL.md` bodies). Default: true. When
    /// false, each such placeholder is replaced with
    /// `[shell command execution disabled by policy]` instead of running.
    /// A higher tier can set it back, except a project that is not trusted
    /// (`app.trusted_projects`).
    pub allow_shell: bool,
}

impl Default for SkillsSettings {
    fn default() -> Self {
        Self { allow_shell: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[non_exhaustive]
pub struct MemorySettings {
    /// The file-based memory system (`MEMORY.md` + one `.md` per memory).
    /// Default `true`. `false` drops the memory prompt, the memory index in
    /// each turn's reminder, and recall.
    pub enabled: bool,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[non_exhaustive]
pub struct TelemetrySettings {
    /// Whether sessions record a telemetry transcript.
    ///
    /// Defaults to **on**: a session writes one JSONL file per session under
    /// `<local_data_dir>/telemetry/`, which is what makes a run debuggable
    /// after the fact (tool calls, permission decisions, turn costs).
    pub enabled: bool,
    /// OTLP endpoint telemetry events are exported to. Unset, the
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` environment variable is used; neither
    /// means no export. Only a build with the `otel` feature exports — any
    /// other logs a warning when this is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub otlp_endpoint: Option<String>,
}

impl Default for TelemetrySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            otlp_endpoint: None,
        }
    }
}

/// What is decided once for the whole instance: read from the global tier's
/// files only (and from what an embedder sets in memory), never from a scene
/// or a project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[non_exhaustive]
pub struct AppSettings {
    /// Projects whose own configuration takes effect as written — see
    /// `settings::trust`. Absolute paths, or globs where `*` stays within one
    /// path segment and `**` crosses them; a leading `~/` is the home
    /// directory. The global `settings.json` and `settings.local.json` lists
    /// add up rather than one replacing the other; `project.trust` writes to
    /// the second.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trusted_projects: Vec<String>,
    /// Trust every project no `trusted_projects` entry names.
    pub trust_all_projects: bool,
    /// Where sessions are kept. `global` (default): under the state root's
    /// `projects/` and `sessions/`. `project`: a session of a trusted project
    /// goes to that project's `<project dir>/state/history/<id>/` (a
    /// worktree's to its main checkout's), with only an index under
    /// `sessions/<id>/`; the rest stay global. What is already kept either
    /// way is read where it is, and history kept in a project is read only
    /// while the project is trusted. Takes effect when the daemon starts.
    pub history: HistoryLocation,
    /// The engine's own HTTP (model calls, `WebFetch` / `Ping`, MCP over
    /// HTTP, OAuth) follows `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` /
    /// `NO_PROXY` from the environment (default). `false` connects directly,
    /// ignoring them. Requests to this machine are direct either way. Not the
    /// network of a sandboxed command, which `sandbox.network` decides. Takes
    /// effect when the daemon starts; `config.reload` does not apply a change.
    pub system_proxy: bool,
    /// The directory at a repository's root the engine's git worktrees go
    /// in, which is also the line it adds to that repository's `.gitignore`:
    /// one path component starting with `.`, not starting with `.git`, not a
    /// configuration root nor another tool's directory
    /// (`base::paths::Layout::with_worktrees_dir`). Unset, `.wkts`. A name
    /// that is not allowed is ignored with a warning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_dir: Option<String>,
    /// How many sessions the daemon keeps in memory. Creating or bringing
    /// back one more takes the least recently active idle session out of
    /// memory; its transcript stays and the next use brings it back. Takes
    /// effect when the daemon starts; `config.reload` does not apply a change.
    pub session_cap: usize,
    /// Seconds a session may go without activity before it is taken out of
    /// memory, as at the cap. Takes effect when the daemon starts;
    /// `config.reload` does not apply a change.
    pub session_idle_timeout_secs: u64,
    /// `agixd`'s async worker threads, at least 1; unset, one per CPU core.
    /// Blocking work (file I/O, searches, scripts) runs on a pool of its own
    /// whatever this is. Read once, before `agixd` starts anything else;
    /// `config.reload` does not apply a change. An embedded engine runs on
    /// its host's runtime and does not read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_threads: Option<std::num::NonZeroUsize>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            trusted_projects: Vec::new(),
            trust_all_projects: false,
            history: HistoryLocation::default(),
            system_proxy: true,
            worktree_dir: None,
            session_cap: 32,
            session_idle_timeout_secs: 3600,
            worker_threads: None,
        }
    }
}

/// [`AppSettings::history`].
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
