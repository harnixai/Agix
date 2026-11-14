//! Tools the host declared for a session, and the engine's own way of putting
//! a call to the host: the call goes out as `AgentEvent::HostToolCall` on the
//! session's event stream and its answer comes back as
//! `InputMessage::HostToolResponse`, the pair a prompt and its answer use.
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
    PathStyle, ProgressSender, Tool, ToolContext, ToolOrigin, ToolResult, ToolSpec,
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

pub const MAX_DECLARED: usize = 64;
const MAX_DESCRIPTION_BYTES: usize = 8 * 1024;
const MAX_SCHEMA_BYTES: usize = 64 * 1024;

/// What is wrong with `decl`, by the field it is wrong in; empty when it
/// may be declared. Whether its name is taken is the registry's to say.
pub fn declaration_problems(decl: &HostToolDecl) -> Vec<(&'static str, String)> {
    let mut problems = Vec::new();
    let name = &decl.name;
    let well_formed = name.len() <= 64
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !well_formed {
        problems.push((
            "name",
            "must match ^[A-Za-z][A-Za-z0-9_]{0,63}$".to_string(),
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

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::Operator)
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
        _: ProgressSender,
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
        );
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
}

impl ChannelHostCalls {
    pub fn new(events: EventSender, pending: PendingHostCalls) -> Self {
        Self { events, pending }
    }
}

fn resolved(call_id: String, reason: Option<base::event::WithdrawalReason>) -> AgentEvent {
    AgentEvent::HostToolCallResolved {
        call_id,
        resolution: match reason {
            Some(_) => PromptResolution::Withdrawn,
            None => PromptResolution::Answered,
        },
        reason: reason.map(HostCallWithdrawal::from),
    }
}

#[async_trait]
impl HostCalls for ChannelHostCalls {
    async fn call(&self, call: HostCall, cancel: &CancellationToken) -> HostCallEnd {
        let (name, input) = match &call.kind {
            HostCallKind::Tool { name, input } => (name.clone(), input.clone()),
            _ => {
                return HostCallEnd::Replied(HostReply::Failed(
                    "this engine cannot put that kind of call to its host".into(),
                ))
            }
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut open = OpenPrompt::register(&call.call_id, &self.pending, tx, self.events.clone())
            .announcing(resolved);
        let _ = self.events.send(AgentEvent::HostToolCall {
            call_id: call.call_id.clone(),
            tool_use_id: call.tool_use_id.clone(),
            name,
            input,
            timeout_ms: u64::try_from(call.timeout.as_millis()).unwrap_or(u64::MAX),
            task_id: call.task_id.clone(),
            agent_session_id: call.agent_session_id.clone(),
        });
        let waited = tokio::select! {
            biased;
            _ = cancel.cancelled() => return HostCallEnd::Cancelled,
            waited = open.wait(rx, Some(call.timeout)) => waited,
        };
        let withdrawn = |open: &mut OpenPrompt<HostCallEnd>, reason| {
            open.announce_with(AgentEvent::HostToolCallResolved {
                call_id: call.call_id.clone(),
                resolution: PromptResolution::Withdrawn,
                reason: Some(reason),
            })
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

pub(crate) fn declared_tools(
    decls: &[HostToolDecl],
) -> Result<Vec<Arc<dyn Tool>>, crate::agent::EngineError> {
    decls
        .iter()
        .take(MAX_DECLARED + 1)
        .enumerate()
        .map(|(i, decl)| {
            if i == MAX_DECLARED {
                return Err(format!("more than {MAX_DECLARED} host tools"));
            }
            DeclaredHostTool::new(decl.clone()).map(|t| Arc::new(t) as Arc<dyn Tool>)
        })
        .collect::<Result<_, _>>()
        .map_err(crate::agent::EngineError::HostTools)
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
        let host = Arc::new(ChannelHostCalls::new(events, pending.clone()));
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

    /// The answer and the deadline cannot both win: whichever takes the
    /// registration counts, and the late one finds nothing to wake.
    #[tokio::test(start_paused = true)]
    async fn an_answer_and_the_deadline_arriving_together_count_once() {
        let (events, mut rx) = bus();
        let pending = PendingHostCalls::default();
        let host = ChannelHostCalls::new(events, pending.clone());
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
        let host = Arc::new(ChannelHostCalls::new(events, pending.clone()));
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
}
