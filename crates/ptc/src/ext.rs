//! `RunExt`: runs a command an installed plugin package declares
//! (`[[command]]`). The model writes one command line; the command's
//! JavaScript runs on the same runtime as `RunCode`, without `agent()`,
//! `notify` or `messages`, and its sub-calls are its own: a read-only call
//! writes nothing, a write call writes only inside the command's `writes`.
//! The host libraries its package `uses` are globals of its runtime.
//! Design: `docs/runext.md`.

use crate::calls::{cut_note, Allowed};
use crate::engine::{CommandScript, ScriptEnd};
use crate::host_libs::{call_context, HostBinding, HostLibraries, Package};
use crate::run::{execute, Ended, Front, Launch};
use crate::{default_run_slots, truncate, Library, Limits};
use async_trait::async_trait;
use base::command::ExtCommand;
use base::context::TaskType;
use base::error::ToolError;
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::interface::tool_dispatch::CommandScope;
use base::tool::{
    CallTraits, PathStyle, ProgressSender, Tool, ToolContext, ToolResult, ToolSpec,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub const EXT_TOOL_NAME: &str = "RunExt";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunExtInput {
    /// The command line, e.g. `forge spec list --status draft`. Quote an
    /// argument with spaces; it is not a shell.
    pub command: String,
    /// Long text or a batch of items, in the shape the command documents.
    #[serde(default)]
    pub input: Option<Value>,
    /// One line saying what this does, shown in the task list.
    #[serde(default)]
    pub description: Option<String>,
}

pub struct RunExtTool {
    commands: BTreeMap<String, ExtCommand>,
    description: String,
    limits: Limits,
    library: Library,
    host_libraries: HostLibraries,
    run_slots: Arc<Semaphore>,
}

/// What a command may not call: a sub-agent's judgment is the model's, and a
/// command does not start programs or other commands.
const COMMAND_CALLS: Allowed = Allowed {
    refused: &["Agent", "RunCode", "RunExt"],
    refusal: "cannot be called from a plugin command",
};

const SUMMARY: &str = "\
Run a command an installed plugin provides. Write it as a command line; long text or \
batches go in `input`. `<command> help` prints its usage. This is not a shell: no \
pipes, redirection, variables or globbing.";

const NOT_A_SHELL: &str = "RunExt is not a shell: the arguments go to the command as \
written. Drop pipes, redirection, `;`, `&&`, variables, substitutions and globs; put long \
content in `input`.";

/// The entry file as errors name it: relative to its package.
fn entry_name(command: &ExtCommand) -> String {
    command
        .file
        .strip_prefix(&command.root)
        .unwrap_or(&command.file)
        .to_string_lossy()
        .into_owned()
}

/// A command line resolved to the command it names.
struct Parsed<'a> {
    command: &'a ExtCommand,
    args: Vec<String>,
    read_only: bool,
    /// The words requoted where needed: what the person is shown.
    line: String,
    /// The words with every run of whitespace, in them or between them, as
    /// one space and no quotes: what permission rules are matched against,
    /// so that quoting a word differently does not slip past a deny.
    rule_line: String,
}

impl RunExtTool {
    pub fn new(commands: Vec<ExtCommand>) -> Self {
        let commands: BTreeMap<String, ExtCommand> =
            commands.into_iter().map(|c| (c.name.clone(), c)).collect();
        let mut description = format!("{SUMMARY}\n\nInstalled:");
        for command in commands.values() {
            description.push_str(&format!("\n  {} — {}", command.name, command.description));
        }
        Self {
            commands,
            description,
            limits: Limits::for_commands(),
            library: Library::default(),
            host_libraries: HostLibraries::default(),
            run_slots: default_run_slots(),
        }
    }

    pub fn with_run_slots(mut self, slots: Arc<Semaphore>) -> Self {
        self.run_slots = slots;
        self
    }

    /// Commands find the base library under `library`'s global; what the
    /// model reads about commands is their own.
    pub fn with_library(mut self, library: Library) -> Self {
        self.library = library;
        self
    }

    /// The host's libraries, of which a command gets those its package
    /// `uses`.
    pub fn with_host_libraries(mut self, libraries: HostLibraries) -> Self {
        self.host_libraries = libraries;
        self
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.commands.keys().map(String::as_str)
    }

    fn parse(&self, line: &str) -> Result<Parsed<'_>, String> {
        let words = split_command_line(line)?;
        let Some((name, args)) = words.split_first() else {
            return Err(format!("the command line is empty. {}", self.installed()));
        };
        let Some(command) = self.commands.get(name) else {
            return Err(format!(
                "there is no command `{name}`. {}",
                self.installed()
            ));
        };
        Ok(Parsed {
            command,
            read_only: command.is_read_only(args),
            args: args.to_vec(),
            line: words
                .iter()
                .map(|w| requote(w))
                .collect::<Vec<_>>()
                .join(" "),
            rule_line: words
                .iter()
                .flat_map(|w| w.split_whitespace())
                .collect::<Vec<_>>()
                .join(" "),
        })
    }

    fn installed(&self) -> String {
        let names: Vec<String> = self
            .commands
            .values()
            .map(|c| format!("`{}` ({})", c.name, c.description))
            .collect();
        format!("Installed: {}.", names.join(", "))
    }

    /// Only a command the top-level conversation runs takes a slot: one a
    /// program runs is inside that program's, one a sub-agent runs inside
    /// its parent's.
    fn slots_for(&self, ctx: &ToolContext) -> Option<Arc<Semaphore>> {
        (ctx.agent_depth == 0 && !ctx.from_code).then(|| self.run_slots.clone())
    }

    async fn run(
        &self,
        parsed: Parsed<'_>,
        input: Option<Value>,
        description: String,
        ctx: &ToolContext,
    ) -> ToolResult {
        let command = parsed.command;
        let Some(dispatch) = ctx.dispatch.as_ref().and_then(|d| {
            d.for_command(CommandScope {
                command: command.name.clone(),
                plugin: command.plugin.clone(),
                read_only: parsed.read_only,
                writes: if parsed.read_only {
                    Vec::new()
                } else {
                    command.writes.clone()
                },
            })
        }) else {
            return ToolResult::error_text("RunExt cannot run a command in this context");
        };
        let source = match self.entry_source(command).await {
            Ok(source) => source,
            Err(e) => return ToolResult::error_text(e),
        };
        let call = json!([
            parsed.args,
            input,
            {
                "projectRoot": ctx.cwd,
                "cwd": ctx.cwd,
                "command": command.name,
                "plugin": { "name": command.plugin, "version": command.plugin_version },
                "readOnly": parsed.read_only,
            }
        ]);
        let scope = base::context::TaskScope::new(
            ctx.session.clone(),
            ctx.tool_use_id.clone(),
            TaskType::Ext,
            CancellationToken::new(),
            json!({
                "description": description,
                "command": parsed.line,
                "background": false,
                "tool_use_id": ctx.tool_use_id,
            }),
        );
        let slots = self.slots_for(ctx);
        let front = Front::Command(CommandScript {
            filename: entry_name(command),
            source,
            function: command.function.clone(),
            call: call.to_string(),
            cache_dir: command.bytecode_dir.clone(),
        });
        let launch = Launch {
            dispatch,
            task: scope.task(),
            session: &ctx.session,
            limits: &self.limits,
            library: &self.library,
            allowed: COMMAND_CALLS,
            run_slots: slots.as_ref(),
            turn_cancel: ctx.cancel.clone(),
            host_libraries: self.host_libraries_of(command, ctx),
        };
        let result = match execute(front, launch).await {
            Ok(ended) => result_of(&ended, &self.limits),
            Err(why) => ToolResult::error_text(format!("The command did not start: {why}.")),
        };
        scope
            .task()
            .append_output(&format!("{}\n", text_of(&result)));
        result
    }

    fn host_libraries_of(&self, command: &ExtCommand, ctx: &ToolContext) -> HostBinding {
        let package = Package {
            name: &command.plugin,
            version: &command.plugin_version,
            source: command.source,
        };
        let context = call_context(ctx, package, &command.name);
        self.host_libraries.bind(&command.uses, context)
    }

    async fn entry_source(&self, command: &ExtCommand) -> Result<String, String> {
        let max = self.limits.code_chars;
        read_entry(&command.file, &command.plugin, &command.name, max).await
    }
}

/// The source of `plugin`'s entry `file`, for its command or function
/// `name`: at most `max_bytes`, and UTF-8.
pub(crate) async fn read_entry(
    file: &std::path::Path,
    plugin: &str,
    name: &str,
    max_bytes: usize,
) -> Result<String, String> {
    let bytes = tokio::fs::read(file).await.map_err(|e| {
        format!(
            "the plugin `{plugin}` is missing its entry {}: {e}",
            file.display()
        )
    })?;
    if bytes.len() > max_bytes {
        return Err(format!(
            "the entry of `{name}` is larger than {max_bytes} bytes"
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| format!("the entry of `{name}` is not UTF-8 JavaScript source"))
}

fn text_of(result: &ToolResult) -> String {
    match &result.content {
        base::tool::ToolResultContent::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// What the model is handed: a string as it is, anything else as JSON, with
/// what the command logged after it. A failure says what it had already
/// done, which the failure does not undo.
fn result_of(ended: &Ended, limits: &Limits) -> ToolResult {
    let log = ended.log_tail();
    let failed = |message: String| {
        ToolResult::error_text(format!(
            "{}{}{log}",
            cut_note(&message, limits.result_chars),
            ended.calls.changes()
        ))
    };
    match &ended.end {
        ScriptEnd::Returned(json) => {
            let returned: Value = serde_json::from_str(json).unwrap_or(Value::Null);
            let (text, value) = match returned.get("text").and_then(Value::as_str) {
                Some(text) => (text.to_string(), Value::String(text.to_string())),
                None => {
                    let value = returned.get("value").cloned().unwrap_or(Value::Null);
                    (value.to_string(), value)
                }
            };
            let (shown, cut) = truncate(&text, limits.result_chars);
            let note = if cut {
                format!("\n… (result cut at {} characters)", limits.result_chars)
            } else {
                String::new()
            };
            let mut result = ToolResult::text(format!("{shown}{note}{log}"));
            result.structured_content = (!cut).then_some(value);
            result
        }
        ScriptEnd::Threw(message) => failed(message.clone()),
        ScriptEnd::Stopped => failed(format!(
            "The command did not finish: {}.",
            ended.why_stopped()
        )),
    }
}

#[async_trait]
impl Tool for RunExtTool {
    fn name(&self) -> &str {
        EXT_TOOL_NAME
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn input_schema(&self, _: PathStyle) -> Value {
        serde_json::to_value(schemars::schema_for!(RunExtInput)).unwrap_or(Value::Null)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
            .needs_dispatch(true)
            .callable_from_code(true)
            .short_description("Run a command an installed plugin provides")
    }

    fn call_traits(&self, input: &Value) -> CallTraits {
        let read_only = input["command"]
            .as_str()
            .and_then(|line| self.parse(line).ok())
            .is_some_and(|p| p.read_only);
        CallTraits::default()
            .read_only(read_only)
            .concurrency_safe(read_only)
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match input["command"].as_str().map(|line| self.parse(line)) {
            Some(Ok(_)) => ValidationResult::Ok,
            Some(Err(e)) => ValidationResult::err(e, 1),
            None => ValidationResult::err("`command` is missing", 1),
        }
    }

    async fn permission_facts(&self, input: &Value, _: &ToolContext) -> PermissionFacts {
        let Some(parsed) = input["command"].as_str().and_then(|l| self.parse(l).ok()) else {
            return PermissionFacts::unknown();
        };
        let subject = Subject::Opaque(parsed.rule_line.clone());
        if parsed.read_only {
            return PermissionFacts::new(subject, Reach::Delegates);
        }
        let writes = match parsed.command.writes.as_slice() {
            [] => "writes no files".to_string(),
            globs => format!("may write {}", globs.join(", ")),
        };
        PermissionFacts::new(subject, Reach::EditsProject).with_question(format!(
            "Run `{}` (plugin `{}`, {writes})?",
            parsed.line, parsed.command.plugin
        ))
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: RunExtInput = serde_json::from_value(input)?;
        let parsed = match self.parse(&input.command) {
            Ok(parsed) => parsed,
            Err(e) => return Ok(ToolResult::error_text(e)),
        };
        let description = input
            .description
            .clone()
            .unwrap_or_else(|| parsed.line.clone());
        Ok(self.run(parsed, input.input, description, &ctx).await)
    }
}

/// `line` cut into words by the shell's quoting rules — single quotes,
/// double quotes, backslash escapes — refusing whatever a shell would have
/// acted on instead of passing along: pipes, redirection, command separators,
/// substitutions, variables and globs.
pub fn split_command_line(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("a single quote is not closed".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some('\n') => {}
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("a double quote is not closed".into()),
                        },
                        Some('`') => return Err(NOT_A_SHELL.into()),
                        Some('$')
                            if chars
                                .peek()
                                .is_some_and(|&c| c != '"' && !c.is_whitespace()) =>
                        {
                            return Err(NOT_A_SHELL.into())
                        }
                        Some(c) => word.push(c),
                        None => return Err("a double quote is not closed".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some('\n') => {}
                    Some(c) => word.push(c),
                    None => word.push('\\'),
                }
            }
            '|' | '>' | '<' | ';' | '&' | '`' | '*' | '?' | '[' | '\n' | '\r' => {
                return Err(NOT_A_SHELL.into())
            }
            '$' if chars.peek().is_some_and(|c| !c.is_whitespace()) => {
                return Err(NOT_A_SHELL.into())
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// `word` as it would be written back on a command line.
fn requote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_alphanumeric() || "-_./:=,+@%^".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}
