//! SkillTool — invokes user-invocable skills by name. A skill's expanded body
//! is injected into the conversation as a user message, or, for a
//! `context: fork` skill, delegated to a sub-agent that waits or runs in the
//! background.

use crate::bundled_files::build_bundled_files_injection;
use async_trait::async_trait;
use base::error::ToolError;
use base::interface::delegate::{
    AskRoute, CallSite, DelegateEnd, DelegateMode, DelegateRequest, Delegated, Delegator,
    StopsWith, Workdir,
};
use base::interface::permission::{Grant, GrantScope, PermissionFacts, Reach, Subject};
use base::interface::skill_catalog::SkillExpansion;
use base::interface::tool_selector::ToolSelector;
use base::tool::{
    PathStyle, ProgressSender, PromptContext, Tool, ToolContext, ToolResult, ToolSpec,
};
use serde_json::Value;
use std::sync::Arc;

/// A tool that invokes skills loaded from disk.
/// Skills are .md files in `~/.agix/skills/` (global default),
/// `~/.agix/scenes/<scene>/skills/` (scene override), and
/// `<project>/.agents/skills/` (project-level).
///
/// Any skill the `SkillManager` loaded is invocable — the only gate is each
/// skill's own `disable_model_invocation` frontmatter flag (checked in
/// `call()` below). There is deliberately no scene-level allow-list on top:
/// `disable_model_invocation` already covers "should the model be allowed
/// to call this," per-skill, which is the right granularity, and a coarser
/// list would block a project's own `.agents/skills/*.md`.
pub struct SkillTool {
    /// The session's skills: its scene's built-ins and its host directories
    /// over the shared catalog. A skill is expanded by the manager that
    /// holds it.
    catalog: Arc<crate::catalog::SceneSkills>,
    /// What a `context: fork` skill is delegated through; without one it
    /// runs inline.
    delegator: Option<Arc<dyn Delegator>>,
    /// Where the grants a skill's `allowed-tools` makes for the turn are
    /// kept; `None` when not wired up, and they grant nothing.
    grants: Option<Arc<dyn base::interface::permission::PermissionGrants>>,
    /// Per-session set of already-injected `(skill_name, rendered content)`
    /// hashes — when a re-invocation's rendered content exactly matches one
    /// already in this session, a short note is returned instead of a
    /// duplicate full copy. Keyed by `session_id`
    /// (from `ToolContext`) since one `SkillTool` instance is shared across
    /// however many sessions the process serves — a `SkillTool` isn't
    /// per-session itself, so this can't just be a bare `HashSet`.
    already_injected:
        std::sync::Mutex<std::collections::HashMap<String, std::collections::HashSet<u64>>>,
}

impl SkillTool {
    /// Every skill `skill_manager` holds.
    pub fn new(skill_manager: std::sync::Arc<crate::manager::SkillManager>) -> Self {
        Self::over(Arc::new(crate::catalog::SceneSkills::new(
            skill_manager,
            Default::default(),
        )))
    }

    pub fn over(catalog: Arc<crate::catalog::SceneSkills>) -> Self {
        Self {
            catalog,
            delegator: None,
            grants: None,
            already_injected: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn with_delegator(mut self, delegator: Arc<dyn Delegator>) -> Self {
        self.delegator = Some(delegator);
        self
    }

    pub fn with_grants(
        mut self,
        grants: Arc<dyn base::interface::permission::PermissionGrants>,
    ) -> Self {
        self.grants = Some(grants);
        self
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }
    fn description(&self) -> &str {
        "Execute a skill within the main conversation. Skills are loaded from the global, scene and project skill directories. Only skills listed in the user-invocable section are available."
    }
    fn input_schema(&self, _: PathStyle) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "The name of the skill to invoke"
                },
                "args": {
                    "type": "string",
                    "description": "Optional arguments to pass to the skill"
                }
            },
            "required": ["skill"]
        })
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
    }

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::Delegates)
    }

    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(include_str!("prompts/skill_tool.prompt.md").to_string())
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let skill_name = input["skill"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("skill name required".into()))?;
        let args = input["args"].as_str().unwrap_or("");
        let not_found = || ToolError::NotFound(format!("Skill '{skill_name}' not found."));

        let (manager, info) = self.catalog.resolve(skill_name).ok_or_else(not_found)?;
        if info.disable_model_invocation {
            return Err(ToolError::Denied(format!(
                "Skill '{skill_name}' has disable_model_invocation set. \
                 It can only be invoked by the user via slash command."
            )));
        }

        let expansion = match expand_for_invocation(&manager, &info, args, &ctx).await {
            Ok(expansion) => expansion,
            Err(ExpandError::NotFound) => return Err(not_found()),
            Err(ExpandError::Rejected(msg)) => {
                return Ok(ToolResult::error_text(msg));
            }
        };

        // The fork's `allowed-tools` narrow what it is offered. Its Turn
        // grants go with the request, for its own session; they are not
        // kept by this one.
        if info.context.as_deref() == Some("fork") {
            if let Some(delegator) = &self.delegator {
                let result = self
                    .fork(delegator.as_ref(), skill_name, &info, expansion, &ctx)
                    .await;
                if !result.is_error {
                    manager.record_invocation(skill_name);
                }
                return Ok(result);
            }
        }

        // Wrapped in command tags so the model reads it as new instructions,
        // not as tool output.
        let skill_msg = format!(
            "<command-name>{skill_name}</command-name>\n<command-args>{args}</command-args>\n\n{}",
            expansion.text
        );

        manager.record_invocation(skill_name);

        self.grant_for_turn(&expansion.grants).await;

        if !self.first_injection(&ctx.session_id, &skill_msg) {
            return Ok(ToolResult::text(format!(
                    "Skill '{skill_name}' is already loaded in this session with the same content — not re-injected."
                )));
        }

        Ok(ToolResult::text(format!(
            "Skill '{skill_name}' invoked. The skill instructions have been loaded."
        ))
        .with_new_messages(vec![serde_json::json!({
            "role": "user",
            "content": skill_msg,
        })]))
    }
}

impl SkillTool {
    async fn grant_for_turn(&self, grants: &[(String, Option<String>)]) {
        let Some(store) = &self.grants else {
            return;
        };
        for (tool_name, rule_content) in grants {
            let _ = store
                .grant(base::interface::permission::Grant::new(
                    tool_name.clone(),
                    rule_content.clone(),
                    base::interface::permission::GrantScope::Turn,
                ))
                .await;
        }
    }

    fn first_injection(&self, session_id: &str, skill_msg: &str) -> bool {
        let mut sessions = self
            .already_injected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seen = sessions.entry(session_id.to_string()).or_default();
        seen.insert(hash_skill_content(skill_msg))
    }

    async fn fork(
        &self,
        delegator: &dyn Delegator,
        skill_name: &str,
        info: &crate::manager::SkillInfo,
        expansion: SkillExpansion,
        ctx: &ToolContext,
    ) -> ToolResult {
        let background = info.background == Some(true);
        let mode = if background {
            DelegateMode::Background {
                notify_parent: true,
            }
        } else {
            DelegateMode::Wait
        };
        let mut req =
            DelegateRequest::new(expansion.text, mode, StopsWith::Call(ctx.cancel.clone()));
        req.agent_type = info.agent.clone();
        req.tools = ToolSelector::from_lists(&allowed_tool_names(info), &[]);
        req.grants = expansion
            .grants
            .into_iter()
            .map(|(tool, content)| Grant::new(tool, content, GrantScope::Turn))
            .collect();
        req.workdir = Workdir::At(ctx.cwd.clone());
        req.called_from = Some(CallSite {
            turn: ctx.turn_no,
            tool_use_id: ctx.tool_use_id.clone(),
        });
        if ctx.from_code {
            req.asks = AskRoute::Refuse;
        }
        let (text, is_error) = match delegator.delegate(req).await {
            Ok(Delegated::Started(started)) => (
                format!(
                    "Skill '{skill_name}' forked in the background \
                     (task_id: {}, status: spawned). Use TaskOutput to check on it.",
                    started.task_id
                ),
                false,
            ),
            Ok(Delegated::Finished(outcome)) => match outcome.end {
                DelegateEnd::Completed | DelegateEnd::CutShort(_) => (
                    format!(
                        "Skill '{skill_name}' executed in forked agent.\n\n{}",
                        outcome.text
                    ),
                    false,
                ),
                DelegateEnd::Cancelled => (
                    format!(
                        "Skill '{skill_name}' fork was stopped before it finished.\n\n{}",
                        outcome.text
                    ),
                    true,
                ),
                DelegateEnd::Failed(e) => (format!("Skill '{skill_name}' fork failed: {e}"), true),
            },
            Err(e) if background => (
                format!("Skill '{skill_name}' background fork failed: {e}"),
                true,
            ),
            Err(e) => (format!("Skill '{skill_name}' fork failed: {e}"), true),
        };
        ToolResult::text(text).with_error(is_error)
    }
}

/// Why a skill could not be expanded.
#[derive(Debug)]
pub enum ExpandError {
    NotFound,
    /// Told to the model as the invocation's result.
    Rejected(String),
}

/// Character cap on a skill's expanded body; its `files:` have their own
/// budget on top.
const SKILL_BODY_CHAR_CAP: usize = 8000;

/// Expand a skill for one invocation — the expansion the `Skill` tool, a
/// forked skill and a `/name` command all share.
///
/// The body (without its frontmatter) has its `` !`cmd` `` placeholders
/// replaced first, over the raw text, before argument substitution; then it
/// is capped, and the `files:` it bundles are appended. The grants are its
/// `allowed-tools`, less any blanket grant its source may not make: a bare
/// tool name switches off confirmation for every call to that tool, which is
/// only safe from a skill the user put there. `<project>/.agents/skills/` is
/// repository content — attacker-supplied on a cloned repo — and a plugin
/// package or a host's skill directory is someone else's, so those only get
/// grants that narrow the tool
/// to a content pattern (`Bash(cargo test:*)`) — not one that reaches as far
/// as the bare tool or out of the project (`Bash(b*)`, `Write(../**)`, see
/// `permissions::dangerous::is_unscoped_grant_in`). A project skill gets none at all while
/// the project is not trusted (`ctx.config.project_trust.is_untrusted()`).
pub async fn expand_for_invocation(
    manager: &crate::manager::SkillManager,
    info: &crate::manager::SkillInfo,
    args: &str,
    ctx: &ToolContext,
) -> Result<SkillExpansion, ExpandError> {
    let bundled_files_text = build_bundled_files_injection(
        &info.path,
        &info.name,
        &*ctx.exec.filesystem,
        manager.prompt_files(),
    )
    .await
    .map_err(ExpandError::Rejected)?;
    let body = manager
        .get_skill_content(&info.name)
        .ok_or(ExpandError::NotFound)?;
    // Before the grants below take effect, on purpose: a skill's own
    // frontmatter does not get to approve its own commands.
    let body = expand_dynamic_context(&body, ctx, info.source).await;
    let expanded = base::skill::expand_skill_vars_named(
        &body,
        args,
        info.arguments.as_deref().unwrap_or(&[]),
        Some(&base::skill::InvocationVars {
            skill_md: &info.path,
            session_id: Some(&ctx.session_id),
        }),
    );
    let capped = match expanded.char_indices().nth(SKILL_BODY_CHAR_CAP) {
        Some((byte_idx, _)) => format!("{}...[truncated]", &expanded[..byte_idx]),
        None => expanded,
    };

    let restricted = matches!(
        info.source,
        crate::manager::SkillSource::Project
            | crate::manager::SkillSource::Plugin
            | crate::manager::SkillSource::Host
    );
    let withheld = info.source == crate::manager::SkillSource::Project
        && ctx.config.project_trust.is_untrusted();
    if withheld && info.allowed_tools.as_ref().is_some_and(|t| !t.is_empty()) {
        tracing::warn!(
            skill = %info.name,
            "the project is not trusted; its skill's `allowed-tools` grant nothing until it is"
        );
    }
    let grants = info
        .allowed_tools
        .iter()
        .filter(|_| !withheld)
        .flatten()
        .filter_map(|entry| {
            let (tool_name, rule_content) = split_allowed_tool_entry(entry);
            if restricted
                && permissions::dangerous::is_unscoped_grant_in(tool_name, rule_content, &ctx.cwd)
            {
                tracing::warn!(
                    skill = %info.name,
                    source = info.source.as_str(),
                    tool = %tool_name,
                    "ignoring a blanket `allowed-tools` grant from a project or plugin skill; \
                     scope it as `Tool(pattern)` to a path inside the project, a program \
                     (`Bash(cargo test:*)`) or a registrable domain to have it honored"
                );
                return None;
            }
            Some((tool_name.to_string(), rule_content.map(str::to_string)))
        })
        .collect();

    Ok(SkillExpansion {
        text: format!("{capped}{bundled_files_text}"),
        grants,
    })
}

/// Cheap, non-cryptographic hash of rendered skill content for the
/// already-injected dedup check — collision risk is irrelevant here (worst
/// case: a false "already loaded" skip, immediately visible and harmless,
/// not a security boundary).
fn hash_skill_content(content: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// Split one `allowed-tools` frontmatter entry into `(tool name, content
/// pattern)`.
///
/// Accepts the same `Tool(pattern)` shape `settings.json`'s
/// `permission_rules` uses (`permissions::rule::parse_rule_string`), so a
/// skill can say `Bash(cargo test:*)` and get exactly that call approved
/// rather than every `Bash` call. A bare `Bash` yields `None` for the
/// pattern, which the caller treats as a blanket grant — see the call site
/// for why that is refused for project-supplied skills.
///
/// Deliberately lenient about malformed input: an entry with an unbalanced
/// paren is treated as a plain tool name rather than dropped, matching how
/// the rest of the skill frontmatter parser degrades. The caller's
/// project-source check is what makes that safe.
/// The tool names a skill's `allowed-tools` lists, without their content
/// patterns: `Bash(cargo test:*)` offers a fork `Bash`.
fn allowed_tool_names(info: &crate::manager::SkillInfo) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for entry in info.allowed_tools.iter().flatten() {
        let (name, _) = split_allowed_tool_entry(entry);
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

fn split_allowed_tool_entry(entry: &str) -> (&str, Option<&str>) {
    let entry = entry.trim();
    match (entry.find('('), entry.strip_suffix(')')) {
        (Some(open), Some(without_close)) if open > 0 => {
            let content = &without_close[open + 1..];
            if content.trim().is_empty() {
                (entry[..open].trim(), None)
            } else {
                (entry[..open].trim(), Some(content))
            }
        }
        _ => (entry, None),
    }
}

/// Whether a skill body holds a command its expansion would run.
pub(crate) fn runs_commands(body: &str) -> bool {
    base::skill::body_runs_commands(body)
}

const DISABLED_PLACEHOLDER: &str = "[shell command execution disabled by policy]";
const UNTRUSTED_PLACEHOLDER: &str = "[shell command not run: this project is not trusted]";

/// Dynamic context injection: `` !`command` `` (inline, `!` at start of
/// line or after whitespace only) and fenced ` ```!\n...\n``` ` blocks in a
/// skill body get replaced with the command's real output *before* the
/// model ever sees the skill content — preprocessing, not something the
/// model executes. Single pass: substituted output is not re-scanned for further
/// placeholders, so a command's own output can't trigger a second round of
/// injection.
///
/// Each command is a `Bash` call of the session (`ctx.inline_commands`),
/// dispatched as the model's own: the session's hooks, rules, middleware,
/// result transformers and telemetry all see it. A skill body is untrusted
/// input — a project skill is whatever the repository says — so nobody is
/// asked about it: what would need the user's approval does not run, and a
/// hook's approval does not stand in for theirs. The skill's own
/// `allowed-tools` are not applied yet at this point. A command that does not
/// produce an answer is replaced by why.
///
/// Respects `ctx.config.disable_skill_shell_execution` — when set, every
/// placeholder is replaced with a fixed disabled-notice string instead of
/// running anything — and does the same for a project skill while the
/// project is not trusted (`ctx.config.project_trust.is_untrusted()`).
async fn expand_dynamic_context(
    body: &str,
    ctx: &ToolContext,
    source: crate::manager::SkillSource,
) -> String {
    if !runs_commands(body) {
        return body.to_string();
    }

    #[allow(clippy::unwrap_used)]
    static PLACEHOLDER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // `!` at start-of-line or preceded by a space/tab only —
        // `KEY=!\`cmd\`` (no separating whitespace) is left as literal text.
        regex::Regex::new(r"(?s:```!\n(.*?)\n```)|(?m:(^|[ \t])!`([^`]*)`)").unwrap()
    });

    let refusal = if ctx.config.disable_skill_shell_execution {
        Some(DISABLED_PLACEHOLDER)
    } else if source == crate::manager::SkillSource::Project
        && ctx.config.project_trust.is_untrusted()
    {
        Some(UNTRUSTED_PLACEHOLDER)
    } else {
        None
    };

    let mut out = String::with_capacity(body.len());
    let mut last_end = 0;
    for caps in PLACEHOLDER.captures_iter(body) {
        let Some(m) = caps.get(0) else { continue };
        out.push_str(&body[last_end..m.start()]);
        if let Some(fenced) = caps.get(1) {
            out.push_str(&run_skill_shell_command(fenced.as_str(), ctx, refusal).await);
        } else {
            out.push_str(caps.get(2).map_or("", |c| c.as_str()));
            let command = caps.get(3).map_or("", |c| c.as_str());
            out.push_str(&run_skill_shell_command(command, ctx, refusal).await);
        }
        last_end = m.end();
    }
    out.push_str(&body[last_end..]);
    out
}

async fn run_skill_shell_command(
    command: &str,
    ctx: &ToolContext,
    refusal: Option<&str>,
) -> String {
    if let Some(refusal) = refusal {
        return refusal.to_string();
    }
    let Some(commands) = &ctx.inline_commands else {
        return format!("[`{command}` not run: nothing in this session runs inline commands]");
    };
    match commands.run_bash(command, ctx.cancel.clone()).await {
        // One trailing newline goes, as shell `$(...)` drops it: otherwise
        // every splice leaves a stray line break before what follows it.
        Ok(answer) => answer
            .text
            .strip_suffix('\n')
            .unwrap_or(&answer.text)
            .to_string(),
        Err(reason) => {
            tracing::warn!(command = %command, reason = %reason, "skill inline command failed");
            format!("[`{command}` failed: {reason}]")
        }
    }
}

/// Register the `Skill` tool over `catalog` in `r`.
pub fn register(
    r: &base::tool::InMemoryToolRegistry,
    catalog: Arc<crate::catalog::SceneSkills>,
    delegator: Option<Arc<dyn Delegator>>,
    grants: Option<Arc<dyn base::interface::permission::PermissionGrants>>,
) {
    let mut tool = SkillTool::over(catalog);
    if let Some(delegator) = delegator {
        tool = tool.with_delegator(delegator);
    }
    if let Some(grants) = grants {
        tool = tool.with_grants(grants);
    }
    r.register(std::sync::Arc::new(tool));
}

#[cfg(test)]
mod dynamic_context_tests {
    use super::*;
    use base::interface::inline_command::InlineCommands;
    use base::interface::tool_dispatch::SubCallAnswer;

    /// Runs each command with `sh` in `cwd`, as the session's `Bash` would
    /// once it let it through, and remembers what it was asked to run.
    struct Shell {
        cwd: std::path::PathBuf,
        ran: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl InlineCommands for Shell {
        async fn run_bash(
            &self,
            command: &str,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<SubCallAnswer, String> {
            self.ran.lock().unwrap().push(command.to_string());
            let out = std::process::Command::new("sh")
                .args(["-c", command])
                .current_dir(&self.cwd)
                .output()
                .map_err(|e| e.to_string())?;
            Ok(SubCallAnswer {
                text: String::from_utf8_lossy(&out.stdout).into_owned(),
                structured: None,
                reported_failure: !out.status.success(),
            })
        }
    }

    /// Refuses every command, as the session's dispatch refuses one.
    struct Refuses;

    #[async_trait]
    impl InlineCommands for Refuses {
        async fn run_bash(
            &self,
            _: &str,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<SubCallAnswer, String> {
            Err("Denied by permission: denied by rule".into())
        }
    }

    fn shell_ctx(cwd: std::path::PathBuf) -> (ToolContext, Arc<Shell>) {
        let mut ctx =
            session_state::testing::tool_context_for_test(platform::local_providers(), cwd.clone());
        let shell = Arc::new(Shell {
            cwd,
            ran: Default::default(),
        });
        ctx.inline_commands = Some(shell.clone());
        (ctx, shell)
    }

    async fn expand(body: &str, ctx: &ToolContext) -> String {
        expand_dynamic_context(body, ctx, crate::manager::SkillSource::User).await
    }

    #[tokio::test]
    async fn a_command_the_session_does_not_answer_is_replaced_by_why() {
        let mut ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            std::env::temp_dir(),
        );
        ctx.inline_commands = Some(Arc::new(Refuses));
        assert_eq!(
            expand("Value: !`echo 42`.", &ctx).await,
            "Value: [`echo 42` failed: Denied by permission: denied by rule]."
        );
    }

    #[tokio::test]
    async fn nothing_runs_where_the_session_runs_no_inline_commands() {
        let ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            std::env::temp_dir(),
        );
        assert_eq!(
            expand("!`echo 42`", &ctx).await,
            "[`echo 42` not run: nothing in this session runs inline commands]"
        );
    }

    #[tokio::test]
    async fn inline_placeholder_at_line_start_is_substituted() {
        let body = "## Info\n!`echo hello-world`\n\nDone.";
        let (ctx, _) = shell_ctx(std::env::temp_dir());
        let out = expand(body, &ctx).await;
        assert!(out.contains("hello-world"));
        assert!(!out.contains("!`echo"));
    }

    #[tokio::test]
    async fn inline_placeholder_after_whitespace_is_substituted() {
        let body = "Value: !`echo 42`.";
        let (ctx, _) = shell_ctx(std::env::temp_dir());
        let out = expand(body, &ctx).await;
        assert!(out.contains("Value: 42."));
    }

    #[tokio::test]
    async fn inline_placeholder_without_separating_whitespace_is_left_literal() {
        // `!` must be at line start or after whitespace — `KEY=!\`cmd\`` has
        // no separator, so it's untouched.
        let body = "KEY=!`echo should-not-run`";
        let (ctx, shell) = shell_ctx(std::env::temp_dir());
        let out = expand(body, &ctx).await;
        assert_eq!(out, body);
        assert!(shell.ran.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn fenced_block_is_substituted() {
        let body = "## Environment\n```!\necho line-one\necho line-two\n```\n";
        let (ctx, _) = shell_ctx(std::env::temp_dir());
        let out = expand(body, &ctx).await;
        assert!(out.contains("line-one"));
        assert!(out.contains("line-two"));
        assert!(!out.contains("```!"));
    }

    #[tokio::test]
    async fn disable_skill_shell_execution_replaces_with_fixed_placeholder_not_running_anything() {
        let (mut ctx, shell) = shell_ctx(std::env::temp_dir());
        ctx.config = Arc::new({
            let mut c = base::context::EngineConfig::defaults_for("test-model");
            c.disable_skill_shell_execution = true;
            c
        });
        let body = "!`echo should-not-run`";
        let out = expand(body, &ctx).await;
        assert_eq!(out, DISABLED_PLACEHOLDER);
        assert!(shell.ran.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn project_skill_commands_wait_for_trust() {
        let dir = tempfile::tempdir().unwrap();
        let (mut ctx, _) = shell_ctx(dir.path().to_path_buf());
        ctx.config = Arc::new({
            let mut c = base::context::EngineConfig::defaults_for("test-model");
            c.project_trust = base::permission::ProjectTrust::Untrusted;
            c
        });
        let body = "!`echo ran > ran.txt`";
        let expand_from = |source| expand_dynamic_context(body, &ctx, source);

        assert_eq!(
            expand_from(crate::manager::SkillSource::Project).await,
            UNTRUSTED_PLACEHOLDER
        );
        assert!(!dir.path().join("ran.txt").exists());

        expand_from(crate::manager::SkillSource::User).await;
        assert!(
            dir.path().join("ran.txt").exists(),
            "only the project's own skills wait for trust"
        );
    }

    #[tokio::test]
    async fn body_with_no_placeholders_is_returned_unchanged() {
        let body = "Just plain instructions, no shell syntax here.";
        let (ctx, _) = shell_ctx(std::env::temp_dir());
        let out = expand(body, &ctx).await;
        assert_eq!(out, body);
    }

    #[tokio::test]
    async fn command_output_is_not_rescanned_for_further_placeholders() {
        // The command's own output happens to contain inline-placeholder-
        // shaped text — it must NOT be expanded again (single pass). Uses
        // printf's octal escape (\140 = backtick) so the *skill body*
        // itself contains no literal backtick — this simple parser has no
        // escaping support for a backtick inside the command text, so a
        // command that needs to *produce* a backtick has to build it this
        // way rather than embed one directly.
        let body = "!`printf '!\\140echo nested\\140'`";
        let (ctx, _) = shell_ctx(std::env::temp_dir());
        let out = expand(body, &ctx).await;
        assert_eq!(out, "!`echo nested`", "output was: {out}");
    }

    #[tokio::test]
    async fn a_fenced_blocks_output_is_not_run_as_an_inline_command() {
        let dir = tempfile::tempdir().unwrap();
        let body = "```!\nprintf '!\\140touch marker\\140'\n```";
        let (ctx, _) = shell_ctx(dir.path().to_path_buf());
        let out = expand(body, &ctx).await;
        assert_eq!(out, "!`touch marker`", "output was: {out}");
        assert!(
            !dir.path().join("marker").exists(),
            "the output of a fenced block was executed"
        );
    }

    #[derive(Default)]
    struct Forks(std::sync::Mutex<Vec<String>>);

    pub(super) fn finished(
        text: &str,
    ) -> Result<Delegated, base::interface::delegate::DelegateError> {
        Ok(Delegated::Finished(
            base::interface::delegate::DelegateOutcome {
                text: text.into(),
                end: DelegateEnd::Completed,
                structured: None,
                session_id: "fork-session".into(),
            },
        ))
    }

    #[async_trait]
    impl Delegator for Forks {
        async fn delegate(
            &self,
            request: DelegateRequest,
        ) -> Result<Delegated, base::interface::delegate::DelegateError> {
            assert!(
                matches!(request.mode, DelegateMode::Wait),
                "no skill here forks in the background"
            );
            self.0.lock().unwrap().push(request.prompt);
            finished("done")
        }

        async fn stop_member(&self, _: &base::interface::delegate::TeamSeat) {}

        fn live_members(&self) -> Vec<base::interface::delegate::LiveMember> {
            Vec::new()
        }
    }

    /// The `Skill` tool inline, a forked skill and a `/name` command (which
    /// reaches skills through the catalog) hand the conversation the same
    /// expansion: no frontmatter, `!cmd` output, arguments, the body cap.
    #[tokio::test]
    async fn every_invocation_path_expands_a_skill_the_same_way() {
        let dir = tempfile::tempdir().unwrap();
        let skill = |name: &str, extra: &str| {
            let folder = dir.path().join(name);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(
                folder.join("SKILL.md"),
                format!(
                    "---\nname: {name}\ndescription: ships\n{extra}---\nShip $ARGUMENTS.\n\
                     !`echo stamped`\n{}",
                    "x".repeat(9000)
                ),
            )
            .unwrap();
        };
        skill("ship", "");
        skill("ship-forked", "context: fork\n");
        let mgr = Arc::new(crate::manager::SkillManager::new());
        mgr.load_dir_subdirs(dir.path(), crate::manager::SkillSource::User)
            .unwrap();
        let (ctx, _) = shell_ctx(dir.path().to_path_buf());
        let forks = Arc::new(Forks::default());
        let tool = SkillTool::new(mgr.clone()).with_delegator(forks.clone());

        let inline = tool
            .call(
                serde_json::json!({"skill": "ship", "args": "v1"}),
                ctx.clone(),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        let inline = inline.new_messages.unwrap()[0]["content"]
            .as_str()
            .unwrap()
            .split_once("\n\n")
            .unwrap()
            .1
            .to_string();
        tool.call(
            serde_json::json!({"skill": "ship-forked", "args": "v1"}),
            ctx.clone(),
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();
        let forked = forks.0.lock().unwrap()[0].clone();
        let slash = base::interface::skill_catalog::SkillCatalog::expand(&*mgr, "ship", "v1", &ctx)
            .await
            .unwrap()
            .unwrap()
            .text;

        assert!(
            inline.starts_with("Ship v1.\nstamped\nxxx") && inline.ends_with("...[truncated]"),
            "{inline:.40}"
        );
        assert_eq!(forked, inline);
        assert_eq!(slash, inline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::skill::{SkillEntry, SkillSource as FrozenSkillSource};
    use base::tool::ToolResultContent;

    fn register_test_skill(
        mgr: &crate::manager::SkillManager,
        name: &str,
        body: &str,
        disable_model_invocation: bool,
    ) {
        register_test_skill_with(mgr, name, body, disable_model_invocation, None, None);
    }

    fn register_test_skill_with(
        mgr: &crate::manager::SkillManager,
        name: &str,
        body: &str,
        disable_model_invocation: bool,
        context: Option<&str>,
        agent: Option<&str>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so the file survives past this function — fine for
        // a short-lived test process.
        let path = dir.keep().join(format!("{name}.md"));
        std::fs::write(&path, body).unwrap();
        mgr.load_from(std::sync::Arc::new(
            base::interface::skill_provider::StaticSkills::new(
                "test",
                vec![SkillEntry {
                    name: name.into(),
                    description: format!("{name} skill"),
                    source: FrozenSkillSource::Project,
                    path,
                    disable_model_invocation,
                    context: context.map(String::from),
                    agent: agent.map(String::from),
                    ..Default::default()
                }],
            ),
        ));
    }

    /// What a fork's request carried.
    struct Forked {
        agent_type: Option<String>,
        background: bool,
        called_from: Option<CallSite>,
        grants: Vec<Grant>,
        stops_with_the_call: bool,
    }

    /// Records the request it was called with, so the fork path's wiring
    /// can be asserted end to end; a background fork is started as
    /// `mock-task-id-1`.
    #[derive(Default)]
    struct MockDelegator {
        last: std::sync::Mutex<Option<Forked>>,
    }

    impl MockDelegator {
        fn last<T>(&self, f: impl FnOnce(&Forked) -> T) -> T {
            f(self.last.lock().unwrap().as_ref().expect("a fork"))
        }
    }

    #[async_trait]
    impl Delegator for MockDelegator {
        async fn delegate(
            &self,
            request: DelegateRequest,
        ) -> Result<Delegated, base::interface::delegate::DelegateError> {
            let background = match request.mode {
                DelegateMode::Wait => false,
                DelegateMode::Background { notify_parent } => {
                    assert!(notify_parent);
                    true
                }
                _ => panic!("a fork waits or runs in the background"),
            };
            *self.last.lock().unwrap() = Some(Forked {
                agent_type: request.agent_type,
                background,
                called_from: request.called_from,
                grants: request.grants,
                stops_with_the_call: matches!(request.stops_with, StopsWith::Call(_)),
            });
            if background {
                return Ok(Delegated::Started(
                    base::interface::delegate::StartedAgent::new("mock-task-id-1", "fork-session"),
                ));
            }
            super::dynamic_context_tests::finished("forked output")
        }

        async fn stop_member(&self, _: &base::interface::delegate::TeamSeat) {}

        fn live_members(&self) -> Vec<base::interface::delegate::LiveMember> {
            Vec::new()
        }
    }

    /// A project-defined skill (e.g. loaded from `.agents/skills/`) is
    /// invocable unless its own frontmatter says otherwise.
    #[tokio::test]
    async fn project_skill_without_disable_flag_is_invocable() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "code-review", "Review the changed files.", false);
        let tool = SkillTool::new(mgr);

        let result = tool
            .call(
                serde_json::json!({"skill": "code-review"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    std::env::temp_dir(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");

        assert!(!result.is_error);
        assert!(
            result.new_messages.is_some(),
            "expanded skill content should be injected as a new message"
        );
    }

    /// `allowed_tools` end to end: before the skill runs, `Bash` has no
    /// rule and would `Ask` in `Default` mode (see
    /// `permissions::policy`'s own `default_mode_with_no_rule_prompts`
    /// test for that baseline) — invoking a skill that scopes its grant
    /// (`Bash(cargo test)`) must make an immediately-following check for
    /// exactly that call come back `Permit`, via the real `Policy`,
    /// not a test double.
    ///
    /// The grant is *scoped*. A bare `Bash` from a project-supplied
    /// skill is refused outright — see
    /// `project_skill_blanket_allowed_tools_grant_is_refused` below.
    #[tokio::test]
    async fn allowed_tools_injects_a_real_temporary_allow_rule() {
        use base::interface::permission::PermissionOutcome;
        use permissions::policy::Policy;

        let mgr = Arc::new(crate::manager::SkillManager::new());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("uses-bash.md");
        std::fs::write(&path, "Run some commands.").unwrap();
        mgr.load_from(std::sync::Arc::new(
            base::interface::skill_provider::StaticSkills::new(
                "test",
                vec![SkillEntry {
                    name: "uses-bash".into(),
                    description: "uses-bash skill".into(),
                    source: FrozenSkillSource::Project,
                    path,
                    allowed_tools: Some(vec!["Bash(cargo test)".into()]),
                    ..Default::default()
                }],
            ),
        ));

        struct FakeBashTool;
        #[async_trait]
        impl Tool for FakeBashTool {
            fn name(&self) -> &str {
                "Bash"
            }
            fn spec(&self) -> ToolSpec {
                ToolSpec::new()
            }

            fn description(&self) -> &str {
                "fake bash tool (test tool)"
            }

            fn input_schema(&self, _: PathStyle) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn guide(&self, _: &PromptContext) -> Option<String> {
                Some("fake bash".into())
            }
            async fn permission_facts(
                &self,
                _: &Value,
                _: &ToolContext,
            ) -> base::interface::permission::PermissionFacts {
                use base::interface::permission::{PermissionFacts, Reach, Subject};
                PermissionFacts::new(Subject::Opaque("cargo test".into()), Reach::Unknown)
                    .with_question("run?")
            }
            async fn call(
                &self,
                _: Value,
                _: ToolContext,
                _: ProgressSender,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::text("ok"))
            }
        }

        let bash: Arc<dyn Tool> = Arc::new(FakeBashTool);
        let real_permission = Arc::new(Policy::empty());

        let sanity = decide_bash(real_permission.as_ref(), &bash).await;
        assert!(
            matches!(sanity, PermissionOutcome::Prompt { .. }),
            "sanity: Bash should Ask before the skill runs, got {sanity:?}"
        );

        let tool = SkillTool::new(mgr).with_grants(real_permission.clone());
        let result = tool
            .call(
                serde_json::json!({"skill": "uses-bash"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    std::env::temp_dir(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");
        assert!(!result.is_error);

        let after = decide_bash(real_permission.as_ref(), &bash).await;
        assert!(
            matches!(after, PermissionOutcome::Permit { .. }),
            "expected allowed_tools to have injected a real Allow rule, got {after:?}"
        );
    }

    /// `<project>/.agents/skills/` is repository content. On a cloned repo
    /// it is attacker-supplied, so a bare `allowed-tools: [Bash]` there must
    /// not switch off confirmation for every `Bash` call.
    #[tokio::test]
    async fn project_skill_blanket_allowed_tools_grant_is_refused() {
        assert!(
            matches!(
                bash_after_a_blanket_grant_from(FrozenSkillSource::Project).await,
                base::interface::permission::PermissionOutcome::Prompt { .. }
            ),
            "a project skill's blanket grant must not silence the Bash prompt"
        );
    }

    /// `${AGIX_SKILL_DIR}` is the folder of the skill's own `SKILL.md`, and
    /// `${AGIX_SESSION_ID}` the session that invoked it — not the process's
    /// cwd and environment.
    #[tokio::test]
    async fn the_invocation_variables_come_from_the_skill_and_its_session() {
        let mgr = crate::manager::SkillManager::new();
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("where-am-i");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        std::fs::write(&path, "dir=${AGIX_SKILL_DIR} session=${AGIX_SESSION_ID}").unwrap();
        mgr.load_from(std::sync::Arc::new(
            base::interface::skill_provider::StaticSkills::new(
                "test",
                vec![SkillEntry {
                    name: "where-am-i".into(),
                    description: "where".into(),
                    source: FrozenSkillSource::User,
                    path,
                    ..Default::default()
                }],
            ),
        ));
        let info = mgr
            .list()
            .into_iter()
            .find(|s| s.name == "where-am-i")
            .unwrap();
        let mut ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            dir.path().to_path_buf(),
        );
        ctx.session_id = "session-of-the-invoker".into();

        let text = expand_for_invocation(&mgr, &info, "", &ctx)
            .await
            .unwrap()
            .text;

        assert!(
            text.contains(&format!("dir={}", skill_dir.display())),
            "{text}"
        );
        assert!(text.contains("session=session-of-the-invoker"), "{text}");
    }

    /// Until the project is trusted, its skills grant nothing — not even a
    /// scoped `Bash(python3:*)`, which would otherwise wave through every
    /// `python3 -c …` once the model invoked the skill.
    #[tokio::test]
    async fn an_untrusted_projects_skills_grant_nothing() {
        let grants = |source: FrozenSkillSource, untrusted: bool| async move {
            let mgr = crate::manager::SkillManager::new();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("py.md");
            std::fs::write(&path, "Use python.").unwrap();
            mgr.load_from(std::sync::Arc::new(
                base::interface::skill_provider::StaticSkills::new(
                    "test",
                    vec![SkillEntry {
                        name: "py".into(),
                        description: "py".into(),
                        source,
                        path,
                        allowed_tools: Some(vec!["Bash(python3:*)".into()]),
                        ..Default::default()
                    }],
                ),
            ));
            let info = mgr.list().into_iter().find(|s| s.name == "py").unwrap();
            let mut ctx = session_state::testing::tool_context_for_test(
                platform::local_providers(),
                dir.path().to_path_buf(),
            );
            ctx.config = Arc::new({
                let mut c = base::context::EngineConfig::defaults_for("test-model");
                c.project_trust = if untrusted {
                    base::permission::ProjectTrust::Untrusted
                } else {
                    base::permission::ProjectTrust::Trusted
                };
                c
            });
            expand_for_invocation(&mgr, &info, "", &ctx)
                .await
                .unwrap()
                .grants
        };
        let scoped = vec![("Bash".to_string(), Some("python3:*".to_string()))];

        assert!(grants(FrozenSkillSource::Project, true).await.is_empty());
        assert_eq!(grants(FrozenSkillSource::Project, false).await, scoped);
        assert_eq!(
            grants(FrozenSkillSource::User, true).await,
            scoped,
            "only the project's own skills wait for trust"
        );
    }

    /// `Bash(*)` names content, but its `*` matches every command — the same
    /// blanket grant as a bare `Bash`.
    #[tokio::test]
    async fn a_wildcard_grant_from_a_project_or_plugin_skill_is_refused() {
        let grants = |source: FrozenSkillSource| async move {
            let mgr = crate::manager::SkillManager::new();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("wild.md");
            std::fs::write(&path, "Anything goes.").unwrap();
            mgr.load_from(std::sync::Arc::new(
                base::interface::skill_provider::StaticSkills::new(
                    "test",
                    vec![SkillEntry {
                        name: "wild".into(),
                        description: "wild".into(),
                        source,
                        path,
                        allowed_tools: Some(vec![
                            "Bash(*)".into(),
                            "Write(**)".into(),
                            "Edit(/**)".into(),
                            "Write(x/../../../../../../../../**)".into(),
                            "Edit(/U*/**)".into(),
                            "Bash(b*)".into(),
                            "Bash(bash:*)".into(),
                            "WebFetch(https://*.co.uk/**)".into(),
                            "Bash(cargo test:*)".into(),
                            "Write(src/**)".into(),
                            "WebFetch(https://*.example.com/**)".into(),
                        ]),
                        ..Default::default()
                    }],
                ),
            ));
            let info = mgr.list().into_iter().find(|s| s.name == "wild").unwrap();
            let ctx = session_state::testing::tool_context_for_test(
                platform::local_providers(),
                dir.path().to_path_buf(),
            );
            expand_for_invocation(&mgr, &info, "", &ctx)
                .await
                .unwrap()
                .grants
        };
        let scoped: Vec<_> = [
            ("Bash", "cargo test:*"),
            ("Write", "src/**"),
            ("WebFetch", "https://*.example.com/**"),
        ]
        .into_iter()
        .map(|(tool, content)| (tool.to_string(), Some(content.to_string())))
        .collect();
        assert_eq!(grants(FrozenSkillSource::Project).await, scoped);
        assert_eq!(grants(FrozenSkillSource::Plugin).await, scoped);
        assert_eq!(grants(FrozenSkillSource::User).await.len(), 11);
    }

    /// A plugin package is someone else's code just the same.
    #[tokio::test]
    async fn plugin_skill_blanket_allowed_tools_grant_is_refused() {
        assert!(matches!(
            bash_after_a_blanket_grant_from(FrozenSkillSource::Plugin).await,
            base::interface::permission::PermissionOutcome::Prompt { .. }
        ));
    }

    #[tokio::test]
    async fn user_skill_blanket_allowed_tools_grant_is_honored() {
        assert!(matches!(
            bash_after_a_blanket_grant_from(FrozenSkillSource::User).await,
            base::interface::permission::PermissionOutcome::Permit { .. }
        ));
    }

    async fn bash_after_a_blanket_grant_from(
        source: FrozenSkillSource,
    ) -> base::interface::permission::PermissionOutcome {
        use permissions::policy::Policy;

        let mgr = Arc::new(crate::manager::SkillManager::new());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("grabby.md");
        std::fs::write(&path, "Run whatever you like.").unwrap();
        mgr.load_from(std::sync::Arc::new(
            base::interface::skill_provider::StaticSkills::new(
                "test",
                vec![SkillEntry {
                    name: "grabby".into(),
                    description: "grabby skill".into(),
                    source,
                    path,
                    // Bare tool name = blanket grant.
                    allowed_tools: Some(vec!["Bash".into()]),
                    ..Default::default()
                }],
            ),
        ));

        struct AskingBash;
        #[async_trait]
        impl Tool for AskingBash {
            fn name(&self) -> &str {
                "Bash"
            }
            fn spec(&self) -> ToolSpec {
                ToolSpec::new()
            }

            fn description(&self) -> &str {
                "asking bash (test tool)"
            }

            fn input_schema(&self, _: PathStyle) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn guide(&self, _: &PromptContext) -> Option<String> {
                Some("fake bash".into())
            }
            async fn call(
                &self,
                _: Value,
                _: ToolContext,
                _: ProgressSender,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::text("ok"))
            }
        }

        let bash: Arc<dyn Tool> = Arc::new(AskingBash);
        let permission = Arc::new(Policy::empty());

        let tool = SkillTool::new(mgr).with_grants(permission.clone());
        let result = tool
            .call(
                serde_json::json!({"skill": "grabby"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    std::env::temp_dir(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");
        assert!(!result.is_error, "the skill itself still runs");

        decide_bash(permission.as_ref(), &bash).await
    }

    /// `Bash` with an empty input put to `policy` in a `Default` session.
    async fn decide_bash(
        policy: &dyn base::interface::permission::PermissionPolicy,
        bash: &Arc<dyn Tool>,
    ) -> base::interface::permission::PermissionOutcome {
        let input = serde_json::json!({});
        let session: Arc<dyn base::context::Session> =
            Arc::new(session_state::SessionState::for_tests("/tmp".into()));
        let mut ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            "/tmp".into(),
        );
        ctx.session = session.clone();
        let facts =
            base::interface::permission::ToolFacts::gather(bash.as_ref(), &input, &ctx).await;
        policy
            .decide(&base::interface::permission::PermissionRequest::new(
                "Bash",
                Some(bash),
                &input,
                &facts,
                &ctx,
                &session,
            ))
            .await
    }

    /// Feeds `build_skills_text`'s budget-drop order (task tracking
    /// `SkillManager::invocation_count`/`last_invoked_seq`) — a successful
    /// call through the Skill tool must record the invocation.
    #[tokio::test]
    async fn successful_call_records_invocation_on_the_skill_manager() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "code-review", "Review the changed files.", false);
        let tool = SkillTool::new(mgr.clone());

        assert_eq!(mgr.invocation_count("code-review"), 0);
        tool.call(
            serde_json::json!({"skill": "code-review"}),
            session_state::testing::tool_context_for_test(
                platform::local_providers(),
                std::env::temp_dir(),
            ),
            ProgressSender::noop("t"),
        )
        .await
        .expect("call should not error");
        assert_eq!(mgr.invocation_count("code-review"), 1);

        tool.call(
            serde_json::json!({"skill": "code-review"}),
            session_state::testing::tool_context_for_test(
                platform::local_providers(),
                std::env::temp_dir(),
            ),
            ProgressSender::noop("t"),
        )
        .await
        .expect("call should not error");
        assert_eq!(mgr.invocation_count("code-review"), 2);
    }

    /// A denied call (disable_model_invocation) must NOT count as an
    /// invocation — the metric tracks actual use, not attempted use.
    #[tokio::test]
    async fn denied_call_does_not_record_an_invocation() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "manual-only", "Only run via slash command.", true);
        let tool = SkillTool::new(mgr.clone());

        let _ = tool
            .call(
                serde_json::json!({"skill": "manual-only"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    std::env::temp_dir(),
                ),
                ProgressSender::noop("t"),
            )
            .await;
        assert_eq!(mgr.invocation_count("manual-only"), 0);
    }

    /// Re-invoking the same skill with identical rendered content in the
    /// same session must not inject a second full copy.
    #[tokio::test]
    async fn identical_reinvocation_in_same_session_skips_duplicate_content() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "code-review", "Review the changed files.", false);
        let tool = SkillTool::new(mgr);

        let ctx1 = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            std::env::temp_dir(),
        );
        let session_id = ctx1.session_id.clone();
        let first = tool
            .call(
                serde_json::json!({"skill": "code-review"}),
                ctx1,
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");
        assert!(first.new_messages.is_some());

        let mut ctx2 = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            std::env::temp_dir(),
        );
        ctx2.session_id = session_id;
        let second = tool
            .call(
                serde_json::json!({"skill": "code-review"}),
                ctx2,
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");
        assert!(
            second.new_messages.is_none(),
            "identical re-invocation must not inject a second full copy"
        );
        match &second.content {
            ToolResultContent::Text(t) => assert!(t.contains("already loaded")),
            other => panic!("expected Text content, got {other:?}"),
        }
    }

    /// A different session must NOT see another session's dedup state —
    /// the same skill+args invoked for the first time in a fresh session
    /// must inject full content, even if another session already has it.
    #[tokio::test]
    async fn identical_content_in_a_different_session_still_injects_full_content() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "code-review", "Review the changed files.", false);
        let tool = SkillTool::new(mgr);

        tool.call(
            serde_json::json!({"skill": "code-review"}),
            session_state::testing::tool_context_for_test(
                platform::local_providers(),
                std::env::temp_dir(),
            ),
            ProgressSender::noop("t"),
        )
        .await
        .expect("call should not error");

        // A fresh `tool_context_for_test` has a different/independent identity
        // from the harness's perspective — but `tool_context_for_test` always uses the
        // same literal session_id ("test"), so exercise the real
        // distinguishing behavior by asserting the dedup is keyed off
        // session_id explicitly instead of relying on `tool_context_for_test`'s default.
        let mut ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            std::env::temp_dir(),
        );
        ctx.session_id = "a-different-session".into();
        let result = tool
            .call(
                serde_json::json!({"skill": "code-review"}),
                ctx,
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");
        assert!(
            result.new_messages.is_some(),
            "a different session must get the full content, not the dedup note"
        );
    }

    /// `context: fork` skills pass their `agent:` frontmatter through to
    /// the delegator as `agent_type` — the field that selects which subagent
    /// type to fork into.
    #[tokio::test]
    async fn fork_context_passes_agent_field_to_delegator() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill_with(
            &mgr,
            "deep-research",
            "Research the topic thoroughly.",
            false,
            Some("fork"),
            Some("Explore"),
        );
        let delegator = Arc::new(MockDelegator::default());
        let tool = SkillTool::new(mgr).with_delegator(delegator.clone());

        let result = tool
            .call(
                serde_json::json!({"skill": "deep-research"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    std::env::temp_dir(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");

        assert!(!result.is_error);
        assert_eq!(
            delegator.last(|f| f.agent_type.clone()),
            Some("Explore".to_string()),
            "the skill's agent: field must reach the delegator"
        );
    }

    /// `context: fork` + `background: true` must return a task id
    /// immediately, not block on a waiting one's result.
    #[tokio::test]
    async fn fork_context_with_background_true_is_started_in_the_background() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("bg-research.md");
        std::fs::write(&path, "Research the topic thoroughly.").unwrap();
        mgr.load_from(std::sync::Arc::new(
            base::interface::skill_provider::StaticSkills::new(
                "test",
                vec![SkillEntry {
                    name: "bg-research".into(),
                    description: "bg-research skill".into(),
                    source: FrozenSkillSource::Project,
                    path,
                    context: Some("fork".into()),
                    background: Some(true),
                    ..Default::default()
                }],
            ),
        ));
        let delegator = Arc::new(MockDelegator::default());
        let tool = SkillTool::new(mgr).with_delegator(delegator.clone());

        let result = tool
            .call(
                serde_json::json!({"skill": "bg-research"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    std::env::temp_dir(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");

        assert!(!result.is_error);
        assert!(delegator.last(|f| f.background));
        let ToolResultContent::Text(text) = result.content else {
            panic!("expected text content");
        };
        assert!(
            text.contains("mock-task-id-1"),
            "expected the task id in the response, got: {text}"
        );
    }

    /// A `context: fork` skill with no `agent:` field passes `None` through
    /// — `Delegator::delegate` decides the
    /// default (general-purpose), this tool doesn't hardcode one.
    #[tokio::test]
    async fn fork_context_without_agent_field_passes_none() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill_with(
            &mgr,
            "pr-summary",
            "Summarize this pull request.",
            false,
            Some("fork"),
            None,
        );
        let delegator = Arc::new(MockDelegator::default());
        let tool = SkillTool::new(mgr).with_delegator(delegator.clone());

        tool.call(
            serde_json::json!({"skill": "pr-summary"}),
            session_state::testing::tool_context_for_test(
                platform::local_providers(),
                std::env::temp_dir(),
            ),
            ProgressSender::noop("t"),
        )
        .await
        .expect("call should not error");

        assert_eq!(delegator.last(|f| f.agent_type.clone()), None);
    }

    /// A fork carries the call it came from and the skill's `allowed-tools`
    /// as Turn grants for its own session, and stops with the call — whether
    /// it waits or runs in the background.
    #[tokio::test]
    async fn a_fork_carries_its_call_site_and_its_turn_grants() {
        for background in [false, true] {
            let mgr = Arc::new(crate::manager::SkillManager::new());
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("echo.md");
            std::fs::write(&path, "Say hi.").unwrap();
            mgr.load_from(std::sync::Arc::new(
                base::interface::skill_provider::StaticSkills::new(
                    "test",
                    vec![SkillEntry {
                        name: "echo".into(),
                        description: "echo".into(),
                        source: FrozenSkillSource::User,
                        path,
                        context: Some("fork".into()),
                        background: Some(background),
                        allowed_tools: Some(vec!["Bash(echo hi)".into()]),
                        ..Default::default()
                    }],
                ),
            ));
            let delegator = Arc::new(MockDelegator::default());
            let tool = SkillTool::new(mgr).with_delegator(delegator.clone());
            let mut ctx = session_state::testing::tool_context_for_test(
                platform::local_providers(),
                dir.path().to_path_buf(),
            );
            ctx.turn_no = 3;
            ctx.tool_use_id = "toolu_skill".into();

            let result = tool
                .call(
                    serde_json::json!({"skill": "echo"}),
                    ctx,
                    ProgressSender::noop("t"),
                )
                .await
                .unwrap();

            assert!(!result.is_error, "{:?}", result.content);
            delegator.last(|f| {
                assert_eq!(f.background, background);
                assert_eq!(
                    f.called_from,
                    Some(CallSite {
                        turn: 3,
                        tool_use_id: "toolu_skill".into(),
                    })
                );
                assert_eq!(
                    f.grants,
                    [Grant::new("Bash", Some("echo hi".into()), GrantScope::Turn)]
                );
                assert!(f.stops_with_the_call);
            });
        }
    }

    /// A skill's own `disable_model_invocation: true` is still respected —
    /// this is the one gate that survived (per-skill, not per-scene).
    #[tokio::test]
    async fn skill_with_disable_model_invocation_is_denied() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "manual-only", "Only run via slash command.", true);
        let tool = SkillTool::new(mgr);

        let err = tool
            .call(
                serde_json::json!({"skill": "manual-only"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    std::env::temp_dir(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .expect_err("disable_model_invocation should deny the call");

        assert!(matches!(err, ToolError::Denied(_)));
    }

    #[tokio::test]
    async fn unknown_skill_returns_not_found() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        let tool = SkillTool::new(mgr);

        let err = tool
            .call(
                serde_json::json!({"skill": "does-not-exist"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    std::env::temp_dir(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .expect_err("unknown skill should error");

        assert!(matches!(err, ToolError::NotFound(_)));
    }

    /// A skill with `files: [notes.md]` gets that file's content read and
    /// injected alongside the body, wrapped in a `## Bundled file:` header.
    #[tokio::test]
    async fn bundled_file_content_is_injected() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("bundled-notes");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: has bundled notes\nfiles: [notes.md]\n---\nBody text.",
        )
        .unwrap();
        std::fs::write(skill_dir.join("notes.md"), "KNOWN_MARKER_CONTENT_XYZ").unwrap();

        let mgr = Arc::new(crate::manager::SkillManager::new());
        mgr.load_from(std::sync::Arc::new(
            base::interface::skill_provider::StaticSkills::new(
                "test",
                vec![SkillEntry {
                    name: "bundled-notes".into(),
                    description: "has bundled notes".into(),
                    source: FrozenSkillSource::Project,
                    path: skill_dir.join("SKILL.md"),
                    files: Some(vec!["notes.md".into()]),
                    ..Default::default()
                }],
            ),
        ));
        let tool = SkillTool::new(mgr);

        let result = tool
            .call(
                serde_json::json!({"skill": "bundled-notes"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    std::env::temp_dir(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");

        assert!(!result.is_error);
        let messages = result.new_messages.expect("expected injected message");
        let content = messages[0]["content"].as_str().unwrap();
        assert!(
            content.contains("KNOWN_MARKER_CONTENT_XYZ"),
            "bundled file content missing from injected message: {content}"
        );
        assert!(content.contains("## Bundled file: notes.md"));
    }

    /// A skill body itself (not a bundled file) that's multi-byte UTF-8 and
    /// exceeds the 8000-char cap must be truncated without panicking, at a
    /// valid char boundary.
    #[tokio::test]
    async fn skill_body_truncation_does_not_panic_on_multibyte_content() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        // Each "你" is a 3-byte UTF-8 char; 9000 of them is well over the
        // 8000-char body cap and, at a byte-index cut, lands mid-char for
        // almost any cap value that isn't a multiple of 3.
        let long_cjk_body = "你".repeat(9000);
        register_test_skill(&mgr, "cjk-body", &long_cjk_body, false);
        let tool = SkillTool::new(mgr);

        let result = tool
            .call(
                serde_json::json!({"skill": "cjk-body"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    std::env::temp_dir(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error (must not panic on truncation)");

        assert!(!result.is_error);
        let messages = result.new_messages.expect("expected injected message");
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.contains("...[truncated]"));
        // Exactly 8000 valid "你" chars should have survived the cut, no
        // partial/garbled character.
        assert!(content.contains(&"你".repeat(8000)));
        assert!(!content.contains(&"你".repeat(8001)));
    }

    /// A bundled file whose content is multi-byte UTF-8 (Chinese) and
    /// exceeds `BUNDLED_FILE_CHAR_CAP` must be truncated without panicking,
    /// at a valid char boundary.
    #[tokio::test]
    async fn bundled_file_truncation_does_not_panic_on_multibyte_content() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("cjk-notes");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: has cjk bundled notes\nfiles: [notes.md]\n---\nBody text.",
        )
        .unwrap();
        // Each "你" is a 3-byte UTF-8 char; 2500 of them is well over the
        // 2000-char per-file cap and, at a byte-index cut, lands mid-char
        // for almost any cap value that isn't a multiple of 3.
        let long_cjk = "你".repeat(2500);
        std::fs::write(skill_dir.join("notes.md"), &long_cjk).unwrap();

        let mgr = Arc::new(crate::manager::SkillManager::new());
        mgr.load_from(std::sync::Arc::new(
            base::interface::skill_provider::StaticSkills::new(
                "test",
                vec![SkillEntry {
                    name: "cjk-notes".into(),
                    description: "has cjk bundled notes".into(),
                    source: FrozenSkillSource::Project,
                    path: skill_dir.join("SKILL.md"),
                    files: Some(vec!["notes.md".into()]),
                    ..Default::default()
                }],
            ),
        ));
        let tool = SkillTool::new(mgr);

        let result = tool
            .call(
                serde_json::json!({"skill": "cjk-notes"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    std::env::temp_dir(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error (must not panic on truncation)");

        assert!(!result.is_error);
        let messages = result.new_messages.expect("expected injected message");
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.contains("## Bundled file: notes.md"));
        assert!(content.contains("...[truncated]"));
        // Exactly 2000 valid "你" chars should have survived the cut, no
        // partial/garbled character.
        assert!(content.contains(&"你".repeat(2000)));
        assert!(!content.contains(&"你".repeat(2001)));
    }

    /// A skill declaring a `files:` entry that escapes its own folder (via
    /// `..`) must be rejected with a visible `is_error: true` result naming
    /// both the skill and the offending path — and none of the outside
    /// file's content may leak into the result.
    #[tokio::test]
    async fn bundled_file_escaping_skill_folder_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("escaping-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        // `outside.md` lives one level above the skill's own folder.
        std::fs::write(dir.path().join("outside.md"), "SECRET_SHOULD_NOT_LEAK").unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: tries to escape\nfiles: [../outside.md]\n---\nBody text.",
        )
        .unwrap();

        let mgr = Arc::new(crate::manager::SkillManager::new());
        mgr.load_from(std::sync::Arc::new(
            base::interface::skill_provider::StaticSkills::new(
                "test",
                vec![SkillEntry {
                    name: "escaping-skill".into(),
                    description: "tries to escape".into(),
                    source: FrozenSkillSource::Project,
                    path: skill_dir.join("SKILL.md"),
                    files: Some(vec!["../outside.md".into()]),
                    ..Default::default()
                }],
            ),
        ));
        let tool = SkillTool::new(mgr);

        let result = tool
            .call(
                serde_json::json!({"skill": "escaping-skill"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    std::env::temp_dir(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .expect("validation failure surfaces as is_error, not Err");

        assert!(result.is_error);
        let ToolResultContent::Text(text) = result.content else {
            panic!("expected text content");
        };
        assert!(
            text.contains("escaping-skill"),
            "error should name the skill: {text}"
        );
        assert!(
            text.contains("outside.md"),
            "error should name the offending path: {text}"
        );
        assert!(
            !text.contains("SECRET_SHOULD_NOT_LEAK"),
            "content from outside the skill folder must never leak: {text}"
        );
        assert!(
            result.new_messages.is_none(),
            "a rejected bundled file must not still inject a message"
        );
    }

    /// A skill with no `files` field at all gets no `## Bundled file:`
    /// section — just the `<command-name>`/`<command-args>`-wrapped message.
    #[tokio::test]
    async fn skill_without_files_field_is_unaffected() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        register_test_skill(&mgr, "no-files", "Just a plain skill body.", false);
        let tool = SkillTool::new(mgr);

        let result = tool
            .call(
                serde_json::json!({"skill": "no-files"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    std::env::temp_dir(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .expect("call should not error");

        assert!(!result.is_error);
        let messages = result.new_messages.expect("expected injected message");
        let content = messages[0]["content"].as_str().unwrap();
        assert!(!content.contains("## Bundled file:"));
        assert_eq!(
            content,
            "<command-name>no-files</command-name>\n<command-args></command-args>\n\nJust a plain skill body."
        );
    }

    async fn states_a_contained_call(tool: &dyn base::tool::Tool, input: serde_json::Value) {
        use base::interface::permission::Reach;
        let ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            std::path::PathBuf::from("/tmp"),
        );
        let stated = tool.permission_facts(&input, &ctx).await;
        assert!(
            matches!(
                stated.reach,
                Reach::SessionOnly | Reach::Delegates | Reach::Operator | Reach::Inside
            ),
            "{}: {stated:?}",
            tool.name()
        );
        assert_eq!(stated.runs, None, "{}", tool.name());
    }

    #[tokio::test]
    async fn a_skill_delegates() {
        let mgr = Arc::new(crate::manager::SkillManager::new());
        let tool = SkillTool::new(mgr);
        let input = serde_json::json!({ "skill": "anything" });
        let ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            std::env::temp_dir(),
        );
        let facts = tool.permission_facts(&input, &ctx).await;
        assert_eq!(facts.reach, base::interface::permission::Reach::Delegates);
        states_a_contained_call(&tool, input).await;
    }
}
