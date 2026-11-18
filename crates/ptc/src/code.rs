//! `RunCode`: the model writes a JavaScript program that calls this session's
//! tools and sub-agents, and only the program's return value comes back to
//! the conversation. Design: `docs/ptc.md`.

use crate::calls::{cut_note, Allowed};
use crate::engine::ScriptEnd;
use crate::run::{execute, Ended, Front, Launch};
use crate::{default_run_slots, first_line, truncate, Limits};
use async_trait::async_trait;
use base::context::{RunningStatus, RunningTask, TaskType};
use base::error::ToolError;
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::interface::tool_dispatch::{CallableTool, ToolDispatch};
use base::tool::{
    PathStyle, ProgressSender, PromptContext, Tool, ToolContext, ToolRegistry, ToolResult, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub const TOOL_NAME: &str = "RunCode";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunCodeInput {
    /// The JavaScript program. End it with `return <JSON value>`; that value
    /// is the result.
    pub code: String,
    /// JSON the program reads as `args`.
    #[serde(default)]
    pub args: Option<Value>,
    /// One line saying what the program does, shown in the task list.
    #[serde(default)]
    pub description: Option<String>,
    /// Run in the background: return a task id now; the result arrives as a
    /// notification and through TaskOutput.
    #[serde(default)]
    pub background: bool,
}

pub struct RunCodeTool {
    description: String,
    guide: String,
    limits: Limits,
    run_slots: Arc<Semaphore>,
}

impl RunCodeTool {
    /// `RunCode` for a session whose registry is `tools`: its description
    /// lists the tools a program may call there.
    pub fn for_registry(tools: &dyn ToolRegistry) -> Self {
        let mut callable: Vec<CallableTool> = tools
            .list()
            .into_iter()
            .filter_map(|t| {
                let spec = t.spec();
                spec.callable_from_code.then(|| CallableTool {
                    name: t.name().to_string(),
                    description: spec
                        .short_description
                        .unwrap_or_else(|| t.description().to_string()),
                    input_schema: base::tool::compact_schema(
                        t.input_schema(base::tool::PathStyle::Absolute),
                    ),
                    result: spec.code_result.map(str::to_string),
                })
            })
            .collect();
        callable.sort_by(|a, b| a.name.cmp(&b.name));
        Self::new(&callable, Limits::default())
    }

    pub fn new(callable: &[CallableTool], limits: Limits) -> Self {
        Self {
            description: describe(callable),
            guide: guide(callable),
            limits,
            run_slots: default_run_slots(),
        }
    }

    /// Share `slots` instead of a budget of this tool's own: a program waits
    /// for one before it starts and holds it until it ends.
    pub fn with_run_slots(mut self, slots: Arc<Semaphore>) -> Self {
        self.run_slots = slots;
        self
    }

    /// Only a run the top-level conversation starts takes a slot. One inside
    /// a sub-agent is already bounded by the run that started that sub-agent,
    /// and making it wait for a slot its ancestors hold would deadlock them.
    fn slots_for(&self, ctx: &ToolContext) -> Option<Arc<Semaphore>> {
        (ctx.agent_depth == 0).then(|| self.run_slots.clone())
    }
}

/// A program may call any tool its session lets code call.
const PROGRAM_CALLS: Allowed = Allowed {
    refused: &[],
    refusal: "",
};

const INTRO: &str = "\
Write a JavaScript program that calls this session's tools, and get back only its return \
value. Use it when a task needs many tool calls whose intermediate results you don't need to \
read one by one — searching many files, fetching many pages, fanning out sub-agents and \
merging what they find — and for a workflow or multi-agent orchestration the user asks for: \
stages are just awaits, a fan-out is Promise.all over agent(). For a few calls, call the tools \
directly.

Runs as a script (not a module) in an embedded QuickJS engine: ES2023 with the standard \
library (JSON, Math, Date, RegExp, Map/Set, Promise incl. all/allSettled) and top-level \
await. No require/import, npm packages, or Node/browser APIs (fs, process, Buffer, fetch, \
setTimeout). Globals: tools.<Name>(input) and callTool(name, input) → Promise of the result \
(an object when the tool returns structured content, else the same text the tool shows you \
when you call it directly; rejects on failure — unsure of a format, return a sample first); \
agent(prompt, {type, schema, label, model}) → Promise (an object when schema is given); \
args; log(text) / console.log — shown to the user as progress and handed back to you with \
the result (the last 4,000 characters); notify(text) to tell the conversation something while running \
in the background; stopRequested() — wrap up and return what you have when true; messages() \
— notes sent to this run with TaskMessage. End with `return <JSON value>`.

Calls go through the same permission checks as direct calls; one that would need the user's \
approval fails instead of asking — make that call directly. Limits: 5 minutes (60 once a \
sub-agent is started), 1,000 calls, 100 sub-agents, 8 at a time; the result is cut at \
50,000 characters.";

/// What every request carries: enough to decide to use it and to write a
/// first program. Everything else is in [`guide`].
const SUMMARY: &str = "\
Run a JavaScript program that calls this session's tools — in parallel, in loops or in stages \
— and get back only its return value. For batch work (many files, pages or items) and for \
workflows or multi-agent orchestration: agent(prompt, {type, schema}) starts a sub-agent. For \
a single task or a few calls, call the tools directly.

A script (QuickJS, ES2023, top-level await; no require/import or Node/browser APIs). \
tools.<Name>(input) and callTool(name, input) return Promises that reject on failure; log(text) \
reports progress. End with `return <JSON value>`.";

fn describe(callable: &[CallableTool]) -> String {
    let mut out = format!(
        "{SUMMARY}\n\nCallable here (MCP tools too, with callTool; the guide has the rest):"
    );
    for tool in callable.iter().filter(|t| t.name != "Agent") {
        out.push_str("\n  ");
        out.push_str(&call_shape(tool));
    }
    out
}

fn guide(callable: &[CallableTool]) -> String {
    let mut out = format!("{INTRO}\n\nTools callable here (MCP tools too, by name with callTool):");
    for tool in callable {
        out.push_str("\n  ");
        out.push_str(&signature(tool));
    }
    out
}

fn signature(tool: &CallableTool) -> String {
    let call = call_shape(tool);
    let summary = first_sentence(&tool.description, 110);
    if summary.is_empty() {
        call
    } else {
        format!("{call} — {summary}")
    }
}

fn call_shape(tool: &CallableTool) -> String {
    let props = tool.input_schema["properties"].as_object();
    let required: Vec<&str> = tool.input_schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let params: Vec<String> = props
        .map(|p| {
            p.keys()
                .map(|k| {
                    if required.contains(&k.as_str()) {
                        k.clone()
                    } else {
                        format!("{k}?")
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let call = if is_identifier(&tool.name) {
        format!("tools.{}({{{}}})", tool.name, params.join(", "))
    } else {
        format!("callTool(\"{}\", {{{}}})", tool.name, params.join(", "))
    };
    match &tool.result {
        Some(shape) => format!("{call} → {shape}"),
        None => call,
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn first_sentence(text: &str, max_chars: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    let end = line.find(". ").map(|i| i + 1).unwrap_or(line.len());
    let sentence = &line[..end];
    if sentence.chars().count() <= max_chars {
        sentence.to_string()
    } else {
        let cut: String = sentence.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

pub(crate) struct Outcome {
    pub text: String,
    pub value: Option<Value>,
    pub is_error: bool,
}

impl RunCodeTool {
    fn start_background(
        &self,
        input: RunCodeInput,
        dispatch: Arc<dyn ToolDispatch>,
        ctx: &ToolContext,
        description: String,
    ) -> ToolResult {
        let task_id = format!("code-{}", base::id::Id::new());
        // The session ending cuts the run off; a TaskStop on its own
        // token, a child of that, still gets the grace period.
        let cancel = ctx.cancel.child_token();
        let task = ctx.session.register_task(
            task_id.clone(),
            TaskType::Code,
            cancel.child_token(),
            serde_json::json!({
                "description": description,
                "background": true,
                "tool_use_id": ctx.tool_use_id,
            }),
        );
        let (session, limits, slots) = (
            ctx.session.clone(),
            self.limits.clone(),
            self.slots_for(ctx),
        );
        tokio::spawn(async move {
            let outcome = run_program(
                input,
                Launch {
                    dispatch,
                    task: &task,
                    session: &session,
                    limits: &limits,
                    allowed: PROGRAM_CALLS,
                    run_slots: slots.as_ref(),
                    turn_cancel: cancel,
                },
                true,
            )
            .await;
            session.post_notice(Some(task.task_id.clone()), notification(&task, &outcome));
            session.finish_task(
                &task.task_id,
                if outcome.is_error {
                    RunningStatus::Failed(first_line(&outcome.text))
                } else {
                    RunningStatus::Completed
                },
            );
        });
        ToolResult::text(format!(
            "Started {task_id} in the background. Its result arrives as a notification; \
             TaskOutput reads it, TaskStop stops it, TaskMessage reaches messages()."
        ))
    }
}

async fn run_program(input: RunCodeInput, launch: Launch<'_>, background: bool) -> Outcome {
    let (task, limits) = (launch.task.clone(), launch.limits.clone());
    let front = Front::Program {
        code: input.code,
        args: input.args,
        background,
    };
    let outcome = match execute(front, launch).await {
        Ok(ended) => outcome_of(&ended, &limits),
        Err(why) => Outcome {
            text: format!("The program did not start: {why}."),
            value: None,
            is_error: true,
        },
    };
    task.append_output(&format!("{}\n", outcome.text));
    outcome
}

fn outcome_of(ended: &Ended, limits: &Limits) -> Outcome {
    let made = ended.calls.made.load(std::sync::atomic::Ordering::SeqCst);
    let failed = ended.calls.failed.load(std::sync::atomic::Ordering::SeqCst);
    let summary = format!("({made} tool calls, {failed} failed){}", ended.log_tail());
    match &ended.end {
        ScriptEnd::Returned(json) => {
            let value = serde_json::from_str::<Value>(json).ok();
            let (text, cut) = truncate(json, limits.result_chars);
            let note = if cut {
                format!(" — result cut at {} characters", limits.result_chars)
            } else {
                String::new()
            };
            Outcome {
                text: format!("{text}\n{summary}{note}"),
                value: if cut { None } else { value },
                is_error: false,
            }
        }
        ScriptEnd::Threw(message) => Outcome {
            text: format!(
                "The program threw:\n{}\n{summary}{}\n\
                 The full guide to what a program can use: ToolSearch select:RunCode.",
                cut_note(message, limits.result_chars),
                ended.calls.failures()
            ),
            value: None,
            is_error: true,
        },
        ScriptEnd::Stopped => Outcome {
            text: format!(
                "The program did not finish: {}.\n{summary}{}",
                ended.why_stopped(),
                ended.calls.failures()
            ),
            value: None,
            is_error: true,
        },
    }
}

#[async_trait]
impl Tool for RunCodeTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn input_schema(&self, _: PathStyle) -> Value {
        serde_json::to_value(schemars::schema_for!(RunCodeInput)).unwrap_or(Value::Null)
    }
    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(self.guide.clone())
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().needs_dispatch(true)
    }

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::Delegates)
    }
    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: RunCodeInput = serde_json::from_value(input)?;
        if input.code.chars().count() > self.limits.code_chars {
            return Ok(ToolResult::error_text(format!(
                "the program is longer than {} characters",
                self.limits.code_chars
            )));
        }
        let Some(dispatch) = ctx.dispatch.clone() else {
            return Ok(ToolResult::error_text(
                "RunCode cannot call tools in this context",
            ));
        };
        let description = input
            .description
            .clone()
            .unwrap_or_else(|| "program".to_string());

        if input.background {
            return Ok(self.start_background(input, dispatch, &ctx, description));
        }

        let scope = base::context::TaskScope::new(
            ctx.session.clone(),
            ctx.tool_use_id.clone(),
            TaskType::Code,
            CancellationToken::new(),
            serde_json::json!({
                "description": description,
                "background": false,
                "tool_use_id": ctx.tool_use_id,
            }),
        );
        let slots = self.slots_for(&ctx);
        let outcome = run_program(
            input,
            Launch {
                dispatch,
                task: scope.task(),
                session: &ctx.session,
                limits: &self.limits,
                allowed: PROGRAM_CALLS,
                run_slots: slots.as_ref(),
                turn_cancel: ctx.cancel.clone(),
            },
            false,
        )
        .await;
        let mut result = if outcome.is_error {
            ToolResult::error_text(outcome.text)
        } else {
            ToolResult::text(outcome.text)
        };
        result.structured_content = outcome.value;
        Ok(result)
    }
}

fn notification(task: &RunningTask, outcome: &Outcome) -> String {
    let status = if outcome.is_error { "failed" } else { "done" };
    let detail = task
        .detail
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut notice = format!(
        "<task-notification>\n<task-id>{}</task-id>\n<status>{status}</status>\n",
        task.task_id
    );
    if let Some(d) = detail["description"].as_str() {
        notice.push_str(&format!("<description>{d}</description>\n"));
    }
    notice.push_str(&format!("<result>\n{}\n</result>\n", outcome.text));
    notice.push_str("</task-notification>");
    notice
}
