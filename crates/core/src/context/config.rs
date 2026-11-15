//! 引擎只读配置 + 模型窗口推断。

use crate::permission::PermissionMode;

/// 引擎只读配置。从 settings.json + CLI flags 合并出来后，整个会话不再变。
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Provider id (e.g. "anthropic", "deepseek", "xai").
    pub provider: Option<String>,
    pub model: String,
    pub max_tokens: u32,
    /// How long a tool call waits for a permission answer before denying.
    /// `0` = wait indefinitely. See
    /// `ExecutionSettings::permission_prompt_timeout_secs`.
    pub permission_prompt_timeout_secs: u64,
    /// How long a tool call still running when its turn is cancelled is given
    /// to stop on its own before it is dropped and answered as cancelled.
    pub tool_cancel_grace: std::time::Duration,
    /// Maximum number of concurrency-safe tools to run in parallel
    /// within a single batch. 0 = unlimited. Default 10.
    pub max_parallelism: usize,
    /// +** — per-turn API call budget. Default **200**.
    ///   When exceeded, engine returns gracefully with
    ///   `StopReason::MaxTurnsReached` instead of an error — partial work
    ///   is preserved and surfaced to the caller. Lower this for sub-
    ///   agents you want to cap (e.g. compactors at 1, memory-extractors
    ///   at 5).
    pub max_api_calls_per_turn: u32,
    /// Auto-routing: optional "strong" tier model. When set,
    ///   the engine starts each turn at `model` (cheap/fast tier, e.g.
    ///   `ds-v4-flash`) and **escalates to `strong_model`** mid-turn on any
    ///   of these signals:
    ///   - `tool_calls_so_far >= 3` (multi-step exploration)
    ///   - previous tool_result was an error (need smarter retry)
    ///   - turn message count >= 8 (deep conversation)
    ///
    /// Once escalated, stays at strong for rest of the turn.
    /// `None` = no routing (use `model` for everything; current default).
    /// User-facing API: surface as `--strong-model` CLI flag, or in
    /// `~/.agix/scenes/<scope>/settings.json`.
    pub strong_model: Option<String>,
    /// Optional fallback model for retry on overloaded /
    /// transport errors. When the primary model returns 503/529/transport
    /// error, the engine falls back to this model for the remaining turns.
    /// Once activated, stays active for the rest of the session to avoid
    /// oscillation.
    /// `None` = no fallback (propagate the error immediately).
    /// User-facing API: surface as `--fallback-model` CLI flag.
    pub fallback_model: Option<String>,

    pub permission_mode: PermissionMode,
    /// The session's sandbox settings; every policy a command runs under is
    /// derived from these.
    pub sandbox: std::sync::Arc<crate::interface::exec::SandboxSettings>,
    /// Mirrors `Settings.disable_skill_shell_execution` — read by the skill
    /// expansion's `` !`cmd` `` step via `ToolContext.config`.
    pub disable_skill_shell_execution: bool,
    /// Whether the project tier is in effect (`LoadedSettings::trust`); an
    /// untrusted project's skill `` !`cmd` `` is not run.
    pub project_trust: crate::permission::ProjectTrust,
    /// How the engine reads the files it puts in front of the model.
    pub prompt_files: crate::paths::PromptFiles,
    /// Mirrors `Settings.sandbox.subprocess_env`.
    pub subprocess_env: crate::interface::exec::SubprocessEnv,

    /// File read/write byte/line limits. Sub-config keeps related field
    /// values together so cross-tool tuning (Read/Write/Edit) is one
    /// import path.
    pub file_limits: FileLimits,

    /// system prompt overrides + extras (AGENTS.md walk, output styles,
    /// append/override). All affect what gets baked into block [1-5] of
    /// the system prompt.
    pub system_prompt: SystemPromptSettings,

    /// 最大 agent 嵌套深度。主 agent 深度 0，每经 AgentTool spawn 加 1。
    /// 超限时 AgentTool 返回 tool error（防模型在子 agent prompt 中写"用 AgentTool"
    /// 导致无限递归）。默认 3。设 0 禁用 AgentTool。
    pub max_agent_depth: u32,

    /// 是否保留完整工具结果文本。默认 false —— 工具结果会被截断到
    /// 2KB 再送回给模型。设 true 时原样透传（长输出场景可能显著消耗 context）。
    pub verbose_tool_results: bool,
    /// See [`PathStyle`]; mirrors `Settings.path_style`.
    pub path_style: PathStyle,
    /// Tools advertised to the model by name and description only, their
    /// schema fetched through `ToolSearch` — the scene's
    /// `ToolSurface::deferred`, fixed when the session is built. A property
    /// of the session rather than of the tool, so an MCP tool that arrives
    /// mid-session is deferred like one that was there from the start.
    pub deferred_tools: std::sync::Arc<std::collections::HashSet<String>>,
    /// Where a tool result too long for the conversation is kept.
    pub tool_results: ToolResultLimits,
}

/// How long a tool result may be before it is saved to a file and the model
/// is given a preview. A session's delegates write into the root session's
/// directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultLimits {
    /// Characters; `0` keeps every result in the conversation.
    pub inline_chars: usize,
    /// `None` when there is nowhere to write: results are then cut by the
    /// per-result budget instead.
    pub dir: Option<std::path::PathBuf>,
}

impl ToolResultLimits {
    pub const DEFAULT_INLINE_CHARS: usize = 30_000;
}

impl Default for ToolResultLimits {
    fn default() -> Self {
        Self {
            inline_chars: Self::DEFAULT_INLINE_CHARS,
            dir: None,
        }
    }
}

/// File-op limits sub-config: the "tuning the FileRead/Write envelope"
/// cluster as one knob.
#[derive(Debug, Clone)]
pub struct FileLimits {
    /// 文件读 / 写的字节上限。FileRead 检查 metadata.len() 用。
    pub max_file_read_bytes: u64,
    /// 单次 FileRead 默认行数（offset/limit 没填时）
    pub default_read_lines: usize,
    /// 单行字符数上限（超了截断 + `[truncated]` 标记）
    pub max_line_chars: usize,
}

impl Default for FileLimits {
    fn default() -> Self {
        Self {
            max_file_read_bytes: 10 * 1024 * 1024,
            default_read_lines: 2000,
            max_line_chars: 2000,
        }
    }
}

/// System-prompt assembly knobs sub-config.
#[derive(Debug, Clone, Default)]
pub struct SystemPromptSettings {
    pub append: Option<String>,
    pub override_text: Option<String>,
    /// Preferred response language. None = use the user's language/context.
    pub language: Option<String>,
    /// Enable Anthropic prompt-caching `scope: "global"` for the static system
    /// prompt prefix. Requires the prompt-caching-scope beta header and may not
    /// be supported by non-Anthropic-compatible endpoints.
    pub global_cache_scope: bool,
    /// True when at least one connected MCP tool is rendered in the tool list.
    /// MCP tools are per-user/session, so system prompt global caching is
    /// downgraded to normal org/provider caching in this case.
    pub mcp_tools_present: bool,
    /// Instructions returned by connected MCP servers during initialize.
    pub mcp_instructions: Vec<McpServerInstruction>,
    /// 是否在 cwd 之上向上爬找 AGENTS.md。默认 true。
    /// monorepo 子目录不想吃父级 monorepo 上下文时设 false。
    pub memory_walk_up: bool,
    /// 选中的 output style 名称（user/project 级别均可）。
    /// 引擎在 collect FrozenContext 时按此名称从
    /// `~/.agix/scenes/<scope>/output-styles/<name>.md` 或 `<cwd>/.agix/output-styles/<name>.md`
    /// 读取内容，注入 system prompt 末段。None = 不注入额外 style。
    pub output_style: Option<String>,
    /// When true (default), use TaskCreate/Update/List/Get/Stop instead of
    /// TodoWrite for task tracking. When false, TodoWrite is used and the V2 Task tools are hidden.
    pub todo_v2_enabled: bool,
    /// When true, register TeamCreate/TeamDelete for multi-agent orchestration.
    /// Default false.
    pub agent_teams_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct McpServerInstruction {
    pub name: String,
    pub instructions: String,
}

/// How file tools word the paths the model reads: in tool descriptions and in
/// the paths Grep, Glob, Write and Edit print. Resolving a path the model
/// passes in is the same in both styles (relative against the working
/// directory, absolute as is).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PathStyle {
    #[default]
    Absolute,
    WorkspaceRelative,
}

impl PathStyle {
    /// `path` as the model should see it: under `cwd` and relative in
    /// `WorkspaceRelative`, otherwise exactly `path.display()`.
    pub fn render(self, path: &std::path::Path, cwd: &std::path::Path) -> String {
        if self == PathStyle::WorkspaceRelative {
            if let Ok(rel) = path.strip_prefix(cwd) {
                return if rel.as_os_str().is_empty() {
                    ".".to_string()
                } else {
                    rel.display().to_string()
                };
            }
        }
        path.display().to_string()
    }
}

impl EngineConfig {
    /// 合理默认值；具体使用方根据 CLI 与 settings 覆盖。
    pub fn defaults_for(model: impl Into<String>) -> Self {
        let model = model.into();
        Self {
            // model / runtime
            provider: None,
            model,
            max_tokens: 16384,
            permission_prompt_timeout_secs: 300,
            tool_cancel_grace: std::time::Duration::from_secs(2),
            max_parallelism: 10,
            max_api_calls_per_turn: 25,
            strong_model: None,
            fallback_model: None,

            // permission / sandbox
            permission_mode: PermissionMode::Default,
            sandbox: Default::default(),
            disable_skill_shell_execution: false,
            project_trust: crate::permission::ProjectTrust::default(),
            prompt_files: crate::paths::PromptFiles::unscoped(),
            subprocess_env: Default::default(),

            // grouped sub-configs
            file_limits: FileLimits::default(),
            system_prompt: SystemPromptSettings {
                memory_walk_up: true,
                todo_v2_enabled: true,
                agent_teams_enabled: false,
                ..Default::default()
            },

            // misc
            max_agent_depth: 3,
            verbose_tool_results: false,
            path_style: PathStyle::default(),
            deferred_tools: Default::default(),
            tool_results: ToolResultLimits::default(),
        }
    }
}

/// 依据模型名推断其上下文窗口大小。
///
/// 依据"模型能装多少"来确定 auto-compact / blocking limit 阈值，
/// 而非对所有模型使用相同的固定值。
///
/// 当前实现是保守启发式，允许用户通过 CLI / settings 覆盖。
pub fn infer_context_window_tokens(model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    if m.contains("deepseek") || m.starts_with("ds-v") || m.starts_with("ds_") {
        1_000_000
    } else if m.contains("codex") {
        400_000
    } else {
        200_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_relative_only_under_the_working_directory() {
        use std::path::Path;
        let cwd = Path::new("/ws");
        let inside = Path::new("/ws/src/a.rs");
        assert_eq!(PathStyle::Absolute.render(inside, cwd), "/ws/src/a.rs");
        assert_eq!(PathStyle::WorkspaceRelative.render(inside, cwd), "src/a.rs");
        assert_eq!(PathStyle::WorkspaceRelative.render(cwd, cwd), ".");
        assert_eq!(
            PathStyle::WorkspaceRelative.render(Path::new("/other/a.rs"), cwd),
            "/other/a.rs"
        );
    }
}
