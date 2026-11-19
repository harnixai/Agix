//! `PluginCall`: a function a plugin package exposes to the host
//! (`[[function]]`), called by the host outside any turn. It is not one of
//! the model's tools: the host hands one built for the call to the session's
//! tool pipeline (`base::interface::plugin_call`). The function runs on the
//! `RunExt` runtime as `fn(input, ctx)`; its sub-calls are held like a
//! command's, read-only unless the host lets this call write. What comes back
//! is structured: `{ok, value, logs}` or `{ok: false, error: {code, message},
//! logs}`. Design: `docs/runext.md` §12.

use crate::calls::Allowed;
use crate::engine::{CommandScript, ScriptEnd};
use crate::ext::read_entry;
use crate::host_libs::{call_context, HostBinding, HostLibraries, Package};
use crate::run::{execute, Ended, Front, Launch};
use crate::{default_run_slots, truncate, Library, Limits};
use async_trait::async_trait;
use base::command::PluginFunction;
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
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub const PLUGIN_CALL_TOOL_NAME: &str = "PluginCall";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PluginCallInput {
    /// The function's call name, `<plugin>.<name>`.
    pub function: String,
    /// What the function is called with; `null` when the host gives nothing.
    #[serde(default)]
    pub input: Value,
}

/// One host call of `function`.
pub struct PluginCallTool {
    function: PluginFunction,
    call_name: String,
    writes: bool,
    session: Option<String>,
    schema: Option<jsonschema::Validator>,
    limits: Limits,
    library: Library,
    host_libraries: HostLibraries,
    run_slots: Option<Arc<Semaphore>>,
}

const FUNCTION_CALLS: Allowed = Allowed {
    refused: &["Agent", "RunCode", "RunExt"],
    refusal: "cannot be called from a plugin function",
};

impl PluginCallTool {
    /// The longest a call may be given to run.
    pub fn time_limit() -> Duration {
        Limits::for_commands().time
    }

    /// A call that may write inside the function's `writes` when `writes`,
    /// and writes nothing otherwise.
    pub fn new(function: PluginFunction, writes: bool) -> Result<Self, String> {
        let call_name = function.call_name();
        let schema = function
            .input_schema
            .as_ref()
            .map(jsonschema::validator_for)
            .transpose()
            .map_err(|e| format!("`{call_name}`'s input_schema does not compile: {e}"))?;
        Ok(Self {
            function,
            call_name,
            writes,
            session: None,
            schema,
            limits: Limits::for_commands(),
            library: Library::default(),
            host_libraries: HostLibraries::default(),
            run_slots: Some(default_run_slots()),
        })
    }

    /// The call runs in the session `id`, which `ctx.session` names.
    pub fn in_session(mut self, id: impl Into<String>) -> Self {
        self.session = Some(id.into());
        self
    }

    /// At most [`Self::time_limit`].
    pub fn with_time(mut self, time: Duration) -> Result<Self, String> {
        let limit = Self::time_limit();
        if time > limit {
            return Err(format!(
                "a plugin function runs for at most {} ms, not {} ms",
                limit.as_millis(),
                time.as_millis()
            ));
        }
        self.limits.time = time;
        self.limits.time_with_agents = time;
        Ok(self)
    }

    pub fn with_run_slots(mut self, slots: Arc<Semaphore>) -> Self {
        self.run_slots = Some(slots);
        self
    }

    /// The caller took the call's slot before handing it over.
    pub fn with_slot_held(mut self) -> Self {
        self.run_slots = None;
        self
    }

    pub fn with_library(mut self, library: Library) -> Self {
        self.library = library;
        self
    }

    /// The host's libraries, of which the function gets those its package
    /// `uses`.
    pub fn with_host_libraries(mut self, libraries: HostLibraries) -> Self {
        self.host_libraries = libraries;
        self
    }

    /// What the host's library functions are told about the call: the
    /// session it runs in, none for a call outside any.
    fn host_libraries_of(&self, ctx: &ToolContext) -> HostBinding {
        let f = &self.function;
        let package = Package {
            name: &f.plugin,
            version: &f.plugin_version,
            source: f.source,
        };
        let mut context = call_context(ctx, package, &f.name);
        context.session_id = self.session.clone();
        self.host_libraries.bind(&f.uses, context)
    }

    fn mismatches(&self, input: &Value) -> Vec<String> {
        let Some(schema) = &self.schema else {
            return Vec::new();
        };
        schema
            .iter_errors(input)
            .take(20)
            .map(|e| match e.instance_path().to_string() {
                at if at.is_empty() => e.to_string(),
                at => format!("{at}: {e}"),
            })
            .collect()
    }

    fn scope(&self) -> CommandScope {
        CommandScope {
            command: self.call_name.clone(),
            plugin: self.function.plugin.clone(),
            read_only: !self.writes,
            writes: if self.writes {
                self.function.writes.clone()
            } else {
                Vec::new()
            },
        }
    }

    fn entry_name(&self) -> String {
        let f = &self.function;
        f.file
            .strip_prefix(&f.root)
            .unwrap_or(&f.file)
            .to_string_lossy()
            .into_owned()
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Answer {
        let problems = self.mismatches(&input);
        if !problems.is_empty() {
            return Answer::failed(
                "invalid_input",
                format!(
                    "the input does not match `{}`'s input_schema: {}",
                    self.call_name,
                    problems.join("; ")
                ),
            );
        }
        let Some(dispatch) = ctx
            .dispatch
            .as_ref()
            .and_then(|d| d.for_command(self.scope()))
        else {
            return Answer::failed(
                "not_started",
                "a plugin function cannot run in this context",
            );
        };
        let f = &self.function;
        let source =
            match read_entry(&f.file, &f.plugin, &self.call_name, self.limits.code_chars).await {
                Ok(source) => source,
                Err(e) => return Answer::failed("not_started", e),
            };
        let call = json!([
            input,
            {
                "via": "host",
                "projectRoot": ctx.cwd,
                "cwd": ctx.cwd,
                "plugin": { "name": f.plugin, "version": f.plugin_version },
                "function": self.call_name,
                "session": self.session,
                "readOnly": !self.writes,
            }
        ]);
        let scope = base::context::TaskScope::new(
            ctx.session.clone(),
            ctx.tool_use_id.clone(),
            TaskType::Ext,
            CancellationToken::new(),
            json!({
                "description": f.description,
                "function": self.call_name,
                "via": "host",
                "background": false,
                "tool_use_id": ctx.tool_use_id,
            }),
        );
        let front = Front::Function(CommandScript {
            filename: self.entry_name(),
            source,
            function: f.function.clone(),
            call: call.to_string(),
            cache_dir: f.bytecode_dir.clone(),
        });
        let launch = Launch {
            dispatch,
            task: scope.task(),
            session: &ctx.session,
            limits: &self.limits,
            library: &self.library,
            allowed: FUNCTION_CALLS,
            run_slots: self.run_slots.as_ref(),
            turn_cancel: ctx.cancel.clone(),
            host_libraries: self.host_libraries_of(ctx),
        };
        let answer = Answer::of_run(execute(front, launch).await, &ctx.cancel);
        scope
            .task()
            .append_output(&format!("{}\n", answer.summary(self.limits.result_chars)));
        answer
    }
}

/// How one call ended, as the host is handed it.
struct Answer {
    outcome: Result<Value, (&'static str, String)>,
    logs: Vec<String>,
}

impl Answer {
    fn of_run(run: Result<Ended, String>, cancel: &CancellationToken) -> Self {
        match run {
            Ok(ended) => Self::of(&ended, cancel),
            Err(_) if cancel.is_cancelled() => {
                Self::failed("cancelled", "the call was cancelled before it started")
            }
            Err(why) => Self::failed("not_started", format!("the function did not start: {why}")),
        }
    }

    fn failed(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            outcome: Err((code, message.into())),
            logs: Vec::new(),
        }
    }

    /// A failure says what the function had already done, which the failure
    /// does not undo.
    fn of(ended: &Ended, cancel: &CancellationToken) -> Self {
        let logs = ended
            .log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .text
            .lines()
            .map(str::to_string)
            .collect();
        let done = ended.calls.changes();
        let outcome = match &ended.end {
            ScriptEnd::Returned(json) => {
                let returned: Value = serde_json::from_str(json).unwrap_or(Value::Null);
                Ok(match returned.get("text") {
                    Some(Value::String(text)) => Value::String(text.clone()),
                    _ => returned.get("value").cloned().unwrap_or(Value::Null),
                })
            }
            ScriptEnd::Threw(message) => Err(("threw", format!("{message}{done}"))),
            ScriptEnd::Stopped if cancel.is_cancelled() => {
                Err(("cancelled", format!("the call was cancelled{done}")))
            }
            ScriptEnd::Stopped
                if crate::engine::now_ms() >= ended.deadline.load(Ordering::Relaxed) =>
            {
                Err(("timeout", format!("the function ran out of time{done}")))
            }
            ScriptEnd::Stopped => Err((
                "stopped",
                format!("the function did not finish: {}{done}", ended.why_stopped()),
            )),
        };
        Self { outcome, logs }
    }

    fn structured(&self) -> Value {
        match &self.outcome {
            Ok(value) => json!({ "ok": true, "value": value, "logs": self.logs }),
            Err((code, message)) => json!({
                "ok": false,
                "error": { "code": code, "message": message },
                "logs": self.logs,
            }),
        }
    }

    /// What hooks, telemetry and the task's output read.
    fn summary(&self, max_chars: usize) -> String {
        let text = match &self.outcome {
            Ok(Value::String(text)) => text.clone(),
            Ok(value) => value.to_string(),
            Err((code, message)) => format!("{code}: {message}"),
        };
        let (shown, cut) = truncate(&text, max_chars);
        if cut {
            format!("{shown}\n… (cut at {max_chars} characters)")
        } else {
            shown
        }
    }

    fn into_result(self, max_chars: usize) -> ToolResult {
        let text = self.summary(max_chars);
        let mut result = if self.outcome.is_ok() {
            ToolResult::text(text)
        } else {
            ToolResult::error_text(text)
        };
        result.structured_content = Some(self.structured());
        result
    }
}

#[async_trait]
impl Tool for PluginCallTool {
    fn name(&self) -> &str {
        PLUGIN_CALL_TOOL_NAME
    }

    fn description(&self) -> &str {
        &self.function.description
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        serde_json::to_value(schemars::schema_for!(PluginCallInput)).unwrap_or(Value::Null)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
            .needs_dispatch(true)
            .callable_from_code(false)
            .short_description("Call a plugin function for the host")
    }

    fn call_traits(&self, _: &Value) -> CallTraits {
        CallTraits::default()
            .read_only(!self.writes)
            .concurrency_safe(!self.writes)
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match input["function"].as_str() {
            Some(name) if name == self.call_name => ValidationResult::Ok,
            Some(name) => ValidationResult::err(
                format!("this call is of `{}`, not `{name}`", self.call_name),
                1,
            ),
            None => ValidationResult::err("`function` is missing", 1),
        }
    }

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        let subject = Subject::Opaque(self.call_name.clone());
        if !self.writes {
            return PermissionFacts::new(subject, Reach::Delegates);
        }
        let writes = match self.function.writes.as_slice() {
            [] => "writes no files".to_string(),
            globs => format!("may write {}", globs.join(", ")),
        };
        PermissionFacts::new(subject, Reach::EditsProject).with_question(format!(
            "Call `{}` (plugin `{}`, {writes})?",
            self.call_name, self.function.plugin
        ))
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: PluginCallInput = serde_json::from_value(input)?;
        Ok(self
            .run(input.input, &ctx)
            .await
            .into_result(self.limits.result_chars))
    }
}
