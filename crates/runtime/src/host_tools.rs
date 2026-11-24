//! Tools the host declared for a session, and the engine's own way of putting
//! a call to the host: the call goes out as `AgentEvent::HostToolCall` (a
//! hook's as `AgentEvent::HostHookCall`) on the session's event stream and its
//! answer comes back as `InputMessage::HostToolResponse`, the pair a prompt
//! and its answer use. A tool call's progress comes back as
//! `InputMessage::HostToolProgress` while it is open.
//!
//! A declared tool is an ordinary `Tool` in the session's registry, so it is
//! offered, searched, permitted, hooked, wrapped and measured as every other
//! tool is; only its `call` differs, and that only hands the call to
//! [`HostCalls`].

use std::sync::Arc;

use async_trait::async_trait;
use base::error::ToolError;
use base::event::{AgentEvent, HostCallWithdrawal, PromptResolution};
use base::interface::host_call::{
    HostCall, HostCallEnd, HostCallKind, HostCalls, HostReply, HostToolDecl,
};
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::tool::{
    PathStyle, ProgressSender, ProgressUpdate, Tool, ToolContext, ToolOrigin, ToolResult, ToolSpec,
    ValidationResult,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::EventSender;
use crate::elicitation::OpenPrompt;

/// Host calls put to the host and not yet answered, keyed by `call_id`.
pub type PendingHostCalls = Arc<
    std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<HostCallEnd>>>,
>;

/// Where the progress of the open host tool calls goes, keyed by `call_id`.
pub type PendingHostProgress =
    Arc<std::sync::Mutex<std::collections::HashMap<String, ProgressSender>>>;

/// The tools the engine and its modules (agents, skills, MCP, teams) register
/// themselves, whatever the template registry holds, sorted: a
/// host connection may not provide a tool under one of these names.
pub const ENGINE_TOOL_NAMES: &[&str] = &[
    "Agent",
    "CronCreate",
    "CronDelete",
    "CronList",
    "EnterWorktree",
    "ExitWorktree",
    "ListMcpResources",
    "ListPeers",
    "MCP",
    "ReadMail",
    "ReadMcpResource",
    "RunCode",
    "SendMessage",
    "Skill",
    "StructuredOutput",
    "TaskMessage",
    "TaskOutput",
    "TaskStop",
    "TaskWait",
    "TeamCreate",
    "TeamDelete",
    "TeamList",
    "TeamMemoryRead",
    "TeamMemorySave",
    "ToolSearch",
    "WebFetch",
    "WebSearch",
];

/// How many tools a session may declare itself.
pub const MAX_DECLARED: usize = 64;
/// How many tools host connections may add to one session, apart from
/// [`MAX_DECLARED`].
pub const MAX_PROVIDED: usize = 128;
const MAX_DESCRIPTION_BYTES: usize = 8 * 1024;
const MAX_SCHEMA_BYTES: usize = 64 * 1024;

/// What is wrong with `decl`, by the field it is wrong in; empty when it
/// may be declared. Whether its name is taken is the registry's to say.
pub fn declaration_problems(decl: &HostToolDecl) -> Vec<(&'static str, String)> {
    let mut problems = Vec::new();
    let name = &decl.name;
    let well_formed = name.len() <= 64
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    if !well_formed {
        problems.push((
            "name",
            "must match ^[A-Za-z][A-Za-z0-9_-]{0,63}$".to_string(),
        ));
    } else if name.starts_with("mcp__") {
        problems.push(("name", "`mcp__` names belong to MCP servers".to_string()));
    }
    if decl.description.len() > MAX_DESCRIPTION_BYTES {
        problems.push(("description", format!("over {MAX_DESCRIPTION_BYTES} bytes")));
    }
    let schema_bytes = serde_json::to_string(&decl.input_schema).map_or(0, |s| s.len());
    if decl.input_schema.get("type").and_then(Value::as_str) != Some("object") {
        problems.push((
            "input_schema",
            "must be a `type: object` schema".to_string(),
        ));
    } else if schema_bytes > MAX_SCHEMA_BYTES {
        problems.push(("input_schema", format!("over {MAX_SCHEMA_BYTES} bytes")));
    } else if let Err(e) = jsonschema::validator_for(&decl.input_schema) {
        problems.push(("input_schema", format!("does not compile: {e}")));
    }
    if !(HostToolDecl::MIN_TIMEOUT..=HostToolDecl::MAX_TIMEOUT).contains(&decl.timeout) {
        problems.push((
            "timeout_ms",
            format!(
                "must be {}..={}",
                HostToolDecl::MIN_TIMEOUT.as_millis(),
                HostToolDecl::MAX_TIMEOUT.as_millis()
            ),
        ));
    }
    problems
}

/// A declared tool, in the session's registry.
pub struct DeclaredHostTool {
    decl: HostToolDecl,
    validator: jsonschema::Validator,
}

impl DeclaredHostTool {
    pub fn new(decl: HostToolDecl) -> Result<Self, String> {
        if let Some((field, problem)) = declaration_problems(&decl).into_iter().next() {
            return Err(format!("host tool `{}`: {field} {problem}", decl.name));
        }
        let validator = jsonschema::validator_for(&decl.input_schema)
            .map_err(|e| format!("host tool `{}`: input_schema {e}", decl.name))?;
        Ok(Self { decl, validator })
    }

    pub fn declaration(&self) -> &HostToolDecl {
        &self.decl
    }
}

#[async_trait]
impl Tool for DeclaredHostTool {
    fn name(&self) -> &str {
        &self.decl.name
    }

    fn description(&self) -> &str {
        &self.decl.description
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
            .origin(ToolOrigin::Host)
            .traits(self.decl.traits)
            .callable_from_code(false)
    }

    fn input_schema(&self, _: PathStyle) -> Value {
        self.decl.input_schema.clone()
    }

    /// Not `Reach::Operator`: that reach is contained and never asked about,
    /// and a host tool runs whatever the host does with it. Asked about like
    /// an MCP server's tool until a rule, or the host's own declaration,
    /// allows it; not fenced like one, since its result comes from the host
    /// the session answers to.
    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        let facts = PermissionFacts::new(Subject::None, Reach::Unknown)
            .with_question(format!("Allow the host's tool {}?", self.decl.name));
        match self.decl.permitted {
            true => facts.permitted_by_host(),
            false => facts,
        }
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        let problems: Vec<String> = self
            .validator
            .iter_errors(input)
            .map(|e| match e.instance_path().to_string() {
                at if at.is_empty() => e.to_string(),
                at => format!("{at}: {e}"),
            })
            .collect();
        if problems.is_empty() {
            ValidationResult::Ok
        } else {
            ValidationResult::err(
                format!(
                    "the input does not match `{}`'s schema: {}",
                    self.decl.name,
                    problems.join("; ")
                ),
                1,
            )
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let name = &self.decl.name;
        let Some(host) = ctx.host_calls.clone() else {
            return Err(no_host(name));
        };
        let call = HostCall::tool(
            format!("hc_{}", base::id::Id::new()),
            ctx.tool_use_id.clone(),
            name.clone(),
            input,
            self.decl.timeout,
        )
        .made_in(ctx.cwd.clone(), ctx.agent_depth)
        .for_host(self.decl.host)
        .with_progress(progress);
        match host.call(call, &ctx.cancel).await {
            HostCallEnd::Replied(HostReply::Result(result)) => Ok(result),
            HostCallEnd::Replied(HostReply::Failed(message)) => Err(ToolError::exec(format!(
                "host tool `{name}` failed: {message}"
            ))),
            HostCallEnd::TimedOut => Err(ToolError::exec(format!(
                "the host did not answer `{name}` within {}s",
                self.decl.timeout.as_secs()
            ))),
            HostCallEnd::Cancelled => Err(ToolError::Cancelled),
            HostCallEnd::NoHost | HostCallEnd::HostGone => Err(no_host(name)),
            _ => Err(ToolError::exec(format!(
                "host tool `{name}` ended in a way this engine does not know"
            ))),
        }
    }
}

fn no_host(name: &str) -> ToolError {
    ToolError::exec(format!("no host is connected to answer `{name}`"))
}

/// Puts calls to the host over the session's event stream.
pub struct ChannelHostCalls {
    events: EventSender,
    pending: PendingHostCalls,
    progress: PendingHostProgress,
}

impl ChannelHostCalls {
    pub fn new(
        events: EventSender,
        pending: PendingHostCalls,
        progress: PendingHostProgress,
    ) -> Self {
        Self {
            events,
            pending,
            progress,
        }
    }
}

fn resolution(
    is_hook: bool,
    call_id: String,
    resolution: PromptResolution,
    reason: Option<HostCallWithdrawal>,
) -> AgentEvent {
    if is_hook {
        AgentEvent::HostHookCallResolved {
            call_id,
            resolution,
            reason,
        }
    } else {
        AgentEvent::HostToolCallResolved {
            call_id,
            resolution,
            reason,
        }
    }
}

fn resolved(
    is_hook: bool,
    call_id: String,
    reason: Option<base::event::WithdrawalReason>,
) -> AgentEvent {
    resolution(
        is_hook,
        call_id,
        match reason {
            Some(_) => PromptResolution::Withdrawn,
            None => PromptResolution::Answered,
        },
        reason.map(HostCallWithdrawal::from),
    )
}

fn resolved_tool_call(
    call_id: String,
    reason: Option<base::event::WithdrawalReason>,
) -> AgentEvent {
    resolved(false, call_id, reason)
}

fn resolved_hook_call(
    call_id: String,
    reason: Option<base::event::WithdrawalReason>,
) -> AgentEvent {
    resolved(true, call_id, reason)
}

/// The call's progress reaches `sender` until it is dropped.
struct ProgressRoute<'a> {
    call_id: &'a str,
    routes: &'a PendingHostProgress,
}

impl<'a> ProgressRoute<'a> {
    fn open(call_id: &'a str, routes: &'a PendingHostProgress, sender: ProgressSender) -> Self {
        routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(call_id.to_string(), sender);
        Self { call_id, routes }
    }
}

impl Drop for ProgressRoute<'_> {
    fn drop(&mut self) {
        self.routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(self.call_id);
    }
}

#[async_trait]
impl HostCalls for ChannelHostCalls {
    async fn call(&self, call: HostCall, cancel: &CancellationToken) -> HostCallEnd {
        let timeout_ms = u64::try_from(call.timeout.as_millis()).unwrap_or(u64::MAX);
        let announced = match &call.kind {
            HostCallKind::Tool { name, input } => AgentEvent::HostToolCall {
                call_id: call.call_id.clone(),
                tool_use_id: call.tool_use_id.clone(),
                name: name.clone(),
                input: input.clone(),
                timeout_ms,
                task_id: call.task_id.clone(),
                agent_session_id: call.agent_session_id.clone(),
                cwd: call.cwd.clone(),
                depth: call.depth,
                host: call.host,
            },
            HostCallKind::Hook { event, input } => AgentEvent::HostHookCall {
                call_id: call.call_id.clone(),
                hook_event: event.clone(),
                input: input.clone(),
                timeout_ms,
                task_id: call.task_id.clone(),
                agent_session_id: call.agent_session_id.clone(),
                cwd: call.cwd.clone(),
                depth: call.depth,
                host: call.host,
            },
            _ => {
                return HostCallEnd::Replied(HostReply::Failed(
                    "this engine cannot put that kind of call to its host".into(),
                ))
            }
        };
        let is_hook = matches!(call.kind, HostCallKind::Hook { .. });
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut open = OpenPrompt::register(&call.call_id, &self.pending, tx, self.events.clone())
            .announcing(if is_hook {
                resolved_hook_call
            } else {
                resolved_tool_call
            });
        let _progress = call
            .progress
            .clone()
            .map(|sender| ProgressRoute::open(&call.call_id, &self.progress, sender));
        let _ = self.events.send(announced);
        let waited = tokio::select! {
            biased;
            _ = cancel.cancelled() => return HostCallEnd::Cancelled,
            waited = open.wait(rx, Some(call.timeout)) => waited,
        };
        let withdrawn = |open: &mut OpenPrompt<HostCallEnd>, reason| {
            open.announce_with(resolution(
                is_hook,
                call.call_id.clone(),
                PromptResolution::Withdrawn,
                Some(reason),
            ))
        };
        match waited {
            None => HostCallEnd::TimedOut,
            Some(Ok(HostCallEnd::NoHost)) => {
                withdrawn(&mut open, HostCallWithdrawal::NoHost);
                HostCallEnd::NoHost
            }
            Some(Ok(HostCallEnd::HostGone)) | Some(Err(_)) => {
                withdrawn(&mut open, HostCallWithdrawal::HostGone);
                HostCallEnd::HostGone
            }
            Some(Ok(end)) => end,
        }
    }
}

/// The calls of a sub-agent, put to its parent's host as the parent's own,
/// marked with the task and session they come from.
pub struct DelegateHostCalls {
    parent: Arc<dyn HostCalls>,
    task_id: Option<String>,
    agent_session_id: String,
}

impl DelegateHostCalls {
    pub fn new(
        parent: Arc<dyn HostCalls>,
        task_id: Option<String>,
        agent_session_id: impl Into<String>,
    ) -> Self {
        Self {
            parent,
            task_id,
            agent_session_id: agent_session_id.into(),
        }
    }
}

#[async_trait]
impl HostCalls for DelegateHostCalls {
    async fn call(&self, call: HostCall, cancel: &CancellationToken) -> HostCallEnd {
        let call = call.from_delegate(self.task_id.clone(), &self.agent_session_id);
        self.parent.call(call, cancel).await
    }
}

/// Hand the host's progress report on the open tool call `call_id` to the
/// call. One nobody waits on is ignored.
pub(crate) fn report_host_progress(
    progress: &PendingHostProgress,
    call_id: &str,
    update: ProgressUpdate,
) {
    let sender = progress
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(call_id)
        .cloned();
    if let Some(sender) = sender {
        sender.update(update);
    }
}

/// Wake the host call `call_id` with `end`. One nobody waits on — answered,
/// timed out, cancelled, never made — is ignored.
pub(crate) fn resolve_host_call(pending: &PendingHostCalls, call_id: &str, end: HostCallEnd) {
    if let Some(sender) = pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(call_id)
    {
        let _ = sender.send(end);
    }
}

/// The session's own host tools, then those host connections provide.
pub(crate) type DeclaredTools = (Vec<Arc<dyn Tool>>, Vec<Arc<dyn Tool>>);

/// The session's own declared tools, and those host connections provide.
/// A bad declaration of the session's own fails the build; a connection's is
/// left out with a warning, since it must not keep an unrelated session from
/// being built.
pub(crate) fn declared_tools(
    decls: &[HostToolDecl],
) -> Result<DeclaredTools, crate::agent::EngineError> {
    let (provided, own): (Vec<_>, Vec<_>) = decls.iter().partition(|d| d.host.is_some());
    if own.len() > MAX_DECLARED {
        return Err(crate::agent::EngineError::HostTools(format!(
            "more than {MAX_DECLARED} host tools"
        )));
    }
    let own = own
        .into_iter()
        .map(|decl| DeclaredHostTool::new(decl.clone()).map(|t| Arc::new(t) as Arc<dyn Tool>))
        .collect::<Result<_, _>>()
        .map_err(crate::agent::EngineError::HostTools)?;
    if provided.len() > MAX_PROVIDED {
        tracing::warn!(
            provided = provided.len(),
            max = MAX_PROVIDED,
            "host connections provide more tools than one session takes; the rest are left out"
        );
    }
    let provided = provided
        .into_iter()
        .take(MAX_PROVIDED)
        .filter_map(|decl| match DeclaredHostTool::new(decl.clone()) {
            Ok(t) => Some(Arc::new(t) as Arc<dyn Tool>),
            Err(e) => {
                tracing::warn!(error = %e, "a host connection's tool is left out of the session");
                None
            }
        })
        .collect();
    Ok((own, provided))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::host_call::HostCallEnd as End;
    use std::time::Duration;

    fn bus() -> (
        EventSender,
        tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (crate::event_bus::EventBus::new(tx), rx)
    }

    fn call(id: &str, within: Duration) -> HostCall {
        HostCall::tool(
            id,
            "toolu_1",
            "HostEcho",
            serde_json::json!({"text": "hi"}),
            within,
        )
    }

    fn ends(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|e| matches!(e, AgentEvent::HostToolCallResolved { .. }))
            .collect()
    }

    async fn asked(pending: &PendingHostCalls, id: &str) {
        while !pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
        {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn an_answer_comes_back_and_the_call_is_resolved_as_answered() {
        let (events, mut rx) = bus();
        let pending = PendingHostCalls::default();
        let host = Arc::new(ChannelHostCalls::new(
            events,
            pending.clone(),
            PendingHostProgress::default(),
        ));
        let asking = {
            let host = host.clone();
            tokio::spawn(async move {
                host.call(
                    call("hc_1", Duration::from_secs(60)),
                    &CancellationToken::new(),
                )
                .await
            })
        };
        asked(&pending, "hc_1").await;
        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::HostToolCall { ref call_id, ref tool_use_id, .. })
                if call_id == "hc_1" && tool_use_id == "toolu_1"
        ));
        resolve_host_call(
            &pending,
            "hc_1",
            End::Replied(HostReply::Result(ToolResult::text("echo: hi"))),
        );
        assert!(matches!(
            asking.await.unwrap(),
            End::Replied(HostReply::Result(_))
        ));
        assert!(matches!(
            ends(&mut rx)[..],
            [AgentEvent::HostToolCallResolved {
                resolution: PromptResolution::Answered,
                reason: None,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn a_delegates_call_goes_out_with_the_cwd_and_depth_it_was_made_in() {
        let (events, mut rx) = bus();
        let pending = PendingHostCalls::default();
        let root: Arc<dyn HostCalls> = Arc::new(ChannelHostCalls::new(
            events,
            pending.clone(),
            PendingHostProgress::default(),
        ));
        let child: Arc<dyn HostCalls> =
            Arc::new(DelegateHostCalls::new(root, Some("t1".into()), "child"));
        let grandchild = DelegateHostCalls::new(child, None, "grandchild");
        let asking = tokio::spawn(async move {
            grandchild
                .call(
                    call("hc_1", Duration::from_secs(60)).made_in("/repo/wt", 2),
                    &CancellationToken::new(),
                )
                .await
        });
        asked(&pending, "hc_1").await;
        let Some(AgentEvent::HostToolCall {
            cwd,
            depth,
            task_id,
            agent_session_id,
            ..
        }) = rx.recv().await
        else {
            panic!("no host tool call");
        };
        assert_eq!(cwd, std::path::PathBuf::from("/repo/wt"));
        assert_eq!(depth, 2);
        assert_eq!(task_id.as_deref(), Some("t1"));
        assert_eq!(agent_session_id.as_deref(), Some("grandchild"));
        resolve_host_call(&pending, "hc_1", End::Cancelled);
        asking.await.unwrap();
    }

    /// The answer and the deadline cannot both win: whichever takes the
    /// registration counts, and the late one finds nothing to wake.
    #[tokio::test(start_paused = true)]
    async fn an_answer_and_the_deadline_arriving_together_count_once() {
        let (events, mut rx) = bus();
        let pending = PendingHostCalls::default();
        let host = ChannelHostCalls::new(events, pending.clone(), PendingHostProgress::default());
        let end = host
            .call(
                call("hc_late", Duration::from_secs(5)),
                &CancellationToken::new(),
            )
            .await;
        assert!(matches!(end, End::TimedOut));
        resolve_host_call(
            &pending,
            "hc_late",
            End::Replied(HostReply::Failed("late".into())),
        );
        assert!(pending.lock().unwrap().is_empty());
        assert!(matches!(
            ends(&mut rx)[..],
            [AgentEvent::HostToolCallResolved {
                resolution: PromptResolution::Withdrawn,
                reason: Some(HostCallWithdrawal::TimedOut),
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn no_host_and_cancellation_end_the_wait_at_once_and_say_why() {
        let (events, mut rx) = bus();
        let pending = PendingHostCalls::default();
        let host = Arc::new(ChannelHostCalls::new(
            events,
            pending.clone(),
            PendingHostProgress::default(),
        ));
        let asking = {
            let host = host.clone();
            tokio::spawn(async move {
                host.call(
                    call("hc_none", Duration::from_secs(600)),
                    &CancellationToken::new(),
                )
                .await
            })
        };
        asked(&pending, "hc_none").await;
        resolve_host_call(&pending, "hc_none", End::NoHost);
        assert!(matches!(asking.await.unwrap(), End::NoHost));

        let cancel = CancellationToken::new();
        let asking = {
            let (host, cancel) = (host.clone(), cancel.clone());
            tokio::spawn(async move {
                host.call(call("hc_cancel", Duration::from_secs(600)), &cancel)
                    .await
            })
        };
        asked(&pending, "hc_cancel").await;
        cancel.cancel();
        assert!(matches!(asking.await.unwrap(), End::Cancelled));
        assert!(pending.lock().unwrap().is_empty());

        let reasons: Vec<_> = ends(&mut rx)
            .into_iter()
            .map(|e| match e {
                AgentEvent::HostToolCallResolved { reason, .. } => reason,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            reasons,
            [
                Some(HostCallWithdrawal::NoHost),
                Some(HostCallWithdrawal::Cancelled)
            ]
        );
    }

    #[test]
    fn a_declaration_is_refused_by_the_field_that_is_wrong() {
        let schema = serde_json::json!({"type": "object"});
        let fields = |decl: HostToolDecl| -> Vec<&'static str> {
            declaration_problems(&decl)
                .into_iter()
                .map(|(f, _)| f)
                .collect()
        };
        assert!(fields(HostToolDecl::new("HostEcho", "d", schema.clone())).is_empty());
        assert!(fields(HostToolDecl::new(
            "ext__hello-app__echo",
            "d",
            schema.clone()
        ))
        .is_empty());
        assert_eq!(
            fields(HostToolDecl::new("-x", "d", schema.clone())),
            ["name"]
        );
        assert_eq!(
            fields(HostToolDecl::new("mcp__x", "d", schema.clone())),
            ["name"]
        );
        assert_eq!(
            fields(HostToolDecl::new("1abc", "d", schema.clone())),
            ["name"]
        );
        assert_eq!(
            fields(HostToolDecl::new(
                "A",
                "d",
                serde_json::json!({"type": "string"})
            )),
            ["input_schema"]
        );
        assert_eq!(
            fields(HostToolDecl::new(
                "A",
                "d",
                serde_json::json!({"type": "object", "properties": {"x": {"type": 7}}})
            )),
            ["input_schema"]
        );
        assert_eq!(
            fields(HostToolDecl::new("A", "d", schema).timeout(Duration::ZERO)),
            ["timeout_ms"]
        );
    }

    fn hook_call(id: &str, within: Duration) -> HostCall {
        HostCall::hook(
            id,
            "toolu_1",
            "PreToolUse",
            serde_json::json!({"tool_name": "Bash"}),
            within,
        )
        .for_host(Some(base::interface::host_call::HostId(3)))
    }

    fn hook_ends(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|e| {
                assert!(!matches!(e, AgentEvent::HostToolCallResolved { .. }));
                matches!(e, AgentEvent::HostHookCallResolved { .. })
            })
            .collect()
    }

    fn channel() -> (
        Arc<ChannelHostCalls>,
        PendingHostCalls,
        PendingHostProgress,
        tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (events, rx) = bus();
        let pending = PendingHostCalls::default();
        let progress = PendingHostProgress::default();
        let host = Arc::new(ChannelHostCalls::new(
            events,
            pending.clone(),
            progress.clone(),
        ));
        (host, pending, progress, rx)
    }

    #[tokio::test]
    async fn a_hook_call_is_announced_with_its_host_and_resolved_as_a_hook_call() {
        let (host, pending, _, mut rx) = channel();
        let asking = {
            let host = host.clone();
            tokio::spawn(async move {
                host.call(
                    hook_call("hk_1", Duration::from_secs(60)),
                    &CancellationToken::new(),
                )
                .await
            })
        };
        asked(&pending, "hk_1").await;
        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::HostHookCall { ref call_id, ref hook_event, host: Some(h), .. })
                if call_id == "hk_1" && hook_event == "PreToolUse" && h.0 == 3
        ));
        resolve_host_call(
            &pending,
            "hk_1",
            End::Replied(HostReply::Hook(serde_json::json!({}))),
        );
        assert!(matches!(
            asking.await.unwrap(),
            End::Replied(HostReply::Hook(_))
        ));
        assert!(matches!(
            hook_ends(&mut rx)[..],
            [AgentEvent::HostHookCallResolved {
                resolution: PromptResolution::Answered,
                reason: None,
                ..
            }]
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_hook_call_that_times_out_is_withdrawn_as_a_hook_call() {
        let (host, _, _, mut rx) = channel();
        let end = host
            .call(
                hook_call("hk_t", Duration::from_secs(5)),
                &CancellationToken::new(),
            )
            .await;
        assert!(matches!(end, End::TimedOut));
        assert!(matches!(
            hook_ends(&mut rx)[..],
            [AgentEvent::HostHookCallResolved {
                resolution: PromptResolution::Withdrawn,
                reason: Some(HostCallWithdrawal::TimedOut),
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn a_cancelled_hook_call_is_withdrawn_as_a_hook_call() {
        let (host, pending, _, mut rx) = channel();
        let cancel = CancellationToken::new();
        let asking = {
            let (host, cancel) = (host.clone(), cancel.clone());
            tokio::spawn(async move {
                host.call(hook_call("hk_c", Duration::from_secs(600)), &cancel)
                    .await
            })
        };
        asked(&pending, "hk_c").await;
        cancel.cancel();
        assert!(matches!(asking.await.unwrap(), End::Cancelled));
        assert!(matches!(
            hook_ends(&mut rx)[..],
            [AgentEvent::HostHookCallResolved {
                resolution: PromptResolution::Withdrawn,
                reason: Some(HostCallWithdrawal::Cancelled),
                ..
            }]
        ));
    }

    struct Recording(std::sync::Mutex<Vec<(String, String)>>);

    impl base::tool::ProgressCallback for Recording {
        fn on_progress(&self, tool_use_id: &str, data: &str) {
            self.0
                .lock()
                .unwrap()
                .push((tool_use_id.to_string(), data.to_string()));
        }
    }

    #[tokio::test]
    async fn a_tool_calls_progress_route_exists_only_while_the_call_is_open() {
        let (host, pending, progress, _rx) = channel();
        let seen = Arc::new(Recording(Default::default()));
        let sender = ProgressSender::with_callback("toolu_1", seen.clone());
        let asking = {
            let host = host.clone();
            tokio::spawn(async move {
                host.call(
                    call("hc_p", Duration::from_secs(60)).with_progress(sender),
                    &CancellationToken::new(),
                )
                .await
            })
        };
        asked(&pending, "hc_p").await;
        assert!(progress.lock().unwrap().contains_key("hc_p"));
        report_host_progress(&progress, "hc_p", ProgressUpdate::text("halfway"));
        assert_eq!(
            *seen.0.lock().unwrap(),
            [("toolu_1".to_string(), "halfway".to_string())]
        );

        resolve_host_call(
            &pending,
            "hc_p",
            End::Replied(HostReply::Result(ToolResult::text("done"))),
        );
        asking.await.unwrap();
        assert!(progress.lock().unwrap().is_empty());
        report_host_progress(&progress, "hc_p", ProgressUpdate::text("late"));
        assert_eq!(seen.0.lock().unwrap().len(), 1);
    }

    fn decls(own: usize, provided: usize, bad_provided: bool) -> Vec<HostToolDecl> {
        let schema = serde_json::json!({"type": "object"});
        let mut all: Vec<HostToolDecl> = (0..own)
            .map(|n| HostToolDecl::new(format!("Own{n}"), "d", schema.clone()))
            .collect();
        all.extend((0..provided).map(|n| {
            HostToolDecl::new(format!("Given{n}"), "d", schema.clone())
                .host(base::interface::host_call::HostId(1))
        }));
        if bad_provided {
            all.push(
                HostToolDecl::new("mcp__bad", "d", schema)
                    .host(base::interface::host_call::HostId(1)),
            );
        }
        all
    }

    #[test]
    fn declared_tools_keeps_own_and_provided_apart() {
        let (own, provided) = declared_tools(&decls(2, 3, false)).unwrap();
        assert_eq!(own.len(), 2);
        assert_eq!(provided.len(), 3);
    }

    #[test]
    fn more_than_the_own_limit_fails_the_build() {
        assert!(declared_tools(&decls(MAX_DECLARED, 0, false)).is_ok());
        assert!(declared_tools(&decls(MAX_DECLARED + 1, 0, false)).is_err());
    }

    #[test]
    fn a_bad_provided_declaration_is_skipped_not_failing() {
        let (own, provided) = declared_tools(&decls(1, 2, true)).unwrap();
        assert_eq!((own.len(), provided.len()), (1, 2));
    }

    #[test]
    fn provided_tools_past_the_limit_are_left_out() {
        let (own, provided) =
            declared_tools(&decls(MAX_DECLARED, MAX_PROVIDED + 10, false)).unwrap();
        assert_eq!(own.len(), MAX_DECLARED);
        assert_eq!(provided.len(), MAX_PROVIDED);
    }
}
