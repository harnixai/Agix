//! Whether a tool call runs: the `PreToolUse` hook's answer, the permission
//! policy's conclusion on the call's facts, the question put to whoever can
//! answer it, and the conclusion recorded.

use super::process::hook_permission_mode;
use super::reporting::record_permission_decision;
use super::tool_exec::{fire_ctx_lifecycle_hook, lifecycle_input, Caller, ToolExecCtx};
use base::context::Session;
use std::sync::Arc;

/// `PreToolUse`: the input the call goes on with, what the hook answered
/// about it in advance, and the reminders its `additional_context` makes,
/// which are the caller's to deliver once the call is let through. A block
/// keeps the input as the model wrote it.
///
/// A skill's inline command is the repository's text put to the hook with no
/// model in between: the hook may refuse it, but its approval does not stand
/// in for the user's.
pub(super) async fn pre_tool_use(
    ctx: &ToolExecCtx,
    name: &str,
    input: serde_json::Value,
) -> PreToolUseAnswer {
    use base::interface::permission::HookAnswer;
    if !ctx.hooks.has_hooks_for(hooks::HookEvent::PreToolUse) {
        return (input, HookAnswer::None, Vec::new());
    }
    let pre_input = hooks::HookInput {
        hook_event_name: "PreToolUse".into(),
        session_id: ctx.session_id.clone(),
        cwd: ctx.cwd.display().to_string(),
        permission_mode: hook_permission_mode(ctx.session_state.permission_mode()),
        depth: ctx.agent_depth,
        tool_name: Some(name.to_string()),
        tool_input: Some(input.clone()),
        tool_use_id: Some(ctx.tool_use_id.clone()),
        tool_result: None,
        is_error: None,
        user_prompt: None,
        ..Default::default()
    };
    match answer_of_pre_tool_use(&ctx.hooks, &pre_input, input, &ctx.cancel).await {
        (input, HookAnswer::Approve { .. }, reminders) if ctx.caller == Caller::InlineCommand => {
            (input, HookAnswer::None, reminders)
        }
        answered => answered,
    }
}

pub(super) type PreToolUseAnswer = (
    serde_json::Value,
    base::interface::permission::HookAnswer,
    Vec<String>,
);

/// Run the `PreToolUse` hooks on `pre_input`, about a call with `input`.
async fn answer_of_pre_tool_use(
    hooks: &hooks::HookRunner,
    pre_input: &hooks::HookInput,
    input: serde_json::Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> PreToolUseAnswer {
    use base::interface::permission::HookAnswer;
    let hook_result = hooks
        .run(hooks::HookEvent::PreToolUse, pre_input, cancel)
        .await;
    if cancel.is_cancelled() {
        let answer = HookAnswer::Block {
            hook: "PreToolUse".into(),
            reason: "the turn was cancelled before its hooks answered".into(),
        };
        return (input, answer, Vec::new());
    }
    if let Some(response) = hook_result.blocked() {
        let answer = HookAnswer::Block {
            hook: "PreToolUse".into(),
            reason: response
                .message
                .clone()
                .unwrap_or_else(|| "no reason given".into()),
        };
        return (input, answer, Vec::new());
    }
    let reminders = hook_result.context_reminders(hooks::HookEvent::PreToolUse);
    let input = hook_result.updated_input().cloned().unwrap_or(input);
    let answer = match hook_result.approved() {
        Some(_) => HookAnswer::Approve {
            hook: "PreToolUse".into(),
        },
        None => HookAnswer::None,
    };
    (input, answer, reminders)
}

/// Why a call does not run: what the `PermissionDenied` hook is told, and
/// what the model is.
struct Refusal {
    reason: String,
    error: String,
}

impl Refusal {
    fn denied(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            error: format!("Denied by permission: {reason}"),
            reason,
        }
    }
}

/// How a call was settled: by the policy, or by whoever its question was put
/// to.
enum Answer {
    Decided(base::interface::permission::PermissionOutcome),
    Refused(base::interface::permission::Reason, Refusal),
    Cancelled { waited_ms: u64 },
}

/// A call let through: what it runs on, what is owed once it has run, what
/// it reaches, and whether a person approved it.
pub(super) struct Authorized {
    pub approval: base::interface::permission::Approval,
    pub obligations: base::interface::permission::Obligations,
    /// What the tool said about the call, gathered once.
    pub facts: base::interface::permission::ToolFacts,
    pub by_person: bool,
}

/// Decide the call once, put a question to whoever can answer it, and record
/// the conclusion.
pub(super) async fn authorize(
    ctx: &ToolExecCtx,
    name: &str,
    tool: &Arc<dyn base::tool::Tool>,
    input: &serde_json::Value,
    tool_ctx: &base::tool::ToolContext,
    hook: &base::interface::permission::HookAnswer,
) -> Result<Authorized, String> {
    use base::interface::permission::{
        Approval, Basis, PermissionOutcome, PermissionRequest, Stage, ToolFacts,
    };
    let started = std::time::Instant::now();
    let facts = ToolFacts::gather(tool.as_ref(), input, tool_ctx).await;
    let judged: std::sync::Arc<dyn base::context::Session> = ctx.session_state.clone();
    let mut request =
        PermissionRequest::new(name, Some(tool), input, &facts, tool_ctx, &judged).with_hook(hook);
    let refused_for_command = match &ctx.caller {
        Caller::Command(calls) => {
            request = request.for_delegate(base::interface::permission::Delegate::new(
                &judged,
                judged.permission_mode(),
                &calls.grants,
            ));
            calls.refusal(name, &facts, &tool_ctx.cwd)
        }
        _ => None,
    };
    let answer = match refused_for_command {
        Some(why) => Answer::Refused(
            base::interface::permission::Reason::new(Stage::ToolRefusal, &why),
            Refusal::denied(why),
        ),
        None => settle(ctx, name, input, &facts, tool_ctx, request).await,
    };
    let concluded = match &answer {
        Answer::Decided(outcome) => outcome.clone(),
        Answer::Refused(reason, _) => PermissionOutcome::Deny {
            reason: reason.clone(),
        },
        Answer::Cancelled { .. } => {
            PermissionOutcome::deny(Stage::Attendance, "the session closed while asking")
        }
    };
    record_permission_decision(ctx, tool, name, input, &concluded, started);
    let refusal = match answer {
        Answer::Decided(PermissionOutcome::Permit {
            basis,
            profile,
            obligations,
        }) => {
            return Ok(Authorized {
                by_person: basis == Basis::Person,
                approval: Approval::new(basis, profile),
                obligations,
                facts,
            })
        }
        Answer::Decided(PermissionOutcome::Deny { reason }) if reason.stage == Stage::Hook => {
            Refusal {
                error: reason.message.clone(),
                reason: reason.message,
            }
        }
        Answer::Decided(PermissionOutcome::Deny { reason }) => Refusal::denied(reason.message),
        Answer::Decided(PermissionOutcome::Prompt { message, .. }) => Refusal::denied(message),
        Answer::Refused(_, refusal) => refusal,
        Answer::Cancelled { waited_ms } => {
            let _ = ctx
                .telemetry_handle
                .record(telemetry::TelemetryEvent::tool_cancelled(
                    &ctx.session_id,
                    ctx.turn_no,
                    Some(ctx.turn_id.clone()),
                    telemetry::ToolCancelledPayload {
                        tool_name: name.to_string(),
                        reason: "session_closing_during_permission_wait".into(),
                        elapsed_ms: waited_ms,
                    },
                ));
            return Err("permission request cancelled: session closing".into());
        }
    };
    fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::PermissionDenied, |i| {
        i.with_tool(name, input.clone())
            .with_reason(refusal.reason.clone())
    })
    .await;
    Err(refusal.error)
}

/// The policy's answer, or, when it leaves the question open, the answer of
/// whoever it is put to.
async fn settle(
    ctx: &ToolExecCtx,
    name: &str,
    input: &serde_json::Value,
    facts: &base::interface::permission::ToolFacts,
    tool_ctx: &base::tool::ToolContext,
    request: base::interface::permission::PermissionRequest<'_>,
) -> Answer {
    use base::interface::permission::PermissionOutcome;
    let mode_changes = ctx.session_state.permission_mode_changes();
    let answer = match ctx.permission.policy.decide(&request).await {
        PermissionOutcome::Prompt { .. } if ctx.caller == Caller::Person => {
            Answer::Decided(permitted_by_person(facts, tool_ctx))
        }
        PermissionOutcome::Prompt {
            prompt_id,
            message,
            paths,
            reason,
            ..
        } => {
            let asked = Asked {
                prompt_id,
                message,
                paths,
                reason,
            };
            ask(ctx, name, input, facts, tool_ctx, asked, request).await
        }
        decided => Answer::Decided(decided),
    };
    unless_overtaken(ctx, facts, mode_changes, answer)
}

/// `answer`, except that a plan-mode exit permitted after the mode changed
/// under it — a host switched it while the policy decided or the question
/// was open — changes nothing: the host's switch is the later word.
fn unless_overtaken(
    ctx: &ToolExecCtx,
    facts: &base::interface::permission::ToolFacts,
    mode_changes: u64,
    answer: Answer,
) -> Answer {
    use base::interface::permission::{ModeStance, Reason, Stage};
    let overtaken = facts.stance == ModeStance::LeavesPlan
        && matches!(&answer, Answer::Decided(outcome) if outcome.is_permit())
        && ctx.session_state.permission_mode_changes() != mode_changes;
    if !overtaken {
        return answer;
    }
    let now = serde_json::to_value(ctx.session_state.permission_mode()).unwrap_or_default();
    let reason = format!(
        "the permission mode was switched to {now} while this waited for approval; \
         it is left as switched"
    );
    Answer::Refused(
        Reason::new(Stage::PlanMode, &reason),
        Refusal::denied(reason),
    )
}

/// The question the policy left open.
struct Asked {
    prompt_id: String,
    message: String,
    paths: Vec<std::path::PathBuf>,
    reason: base::interface::permission::Reason,
}

/// Put the policy's question to the host and wait for it. Only the session
/// going away interrupts the wait; an unanswered prompt within the deadline is
/// not consent.
async fn announce_prompt(ctx: &ToolExecCtx, name: &str, input: &serde_json::Value) {
    // `Notification` rings when a person's attention is needed.
    fire_ctx_lifecycle_hook(ctx, hooks::HookEvent::Notification, |i| {
        i.with_tool(name, input.clone())
            .with_reason(format!("waiting for a permission decision on `{name}`"))
    })
    .await;
}

/// `PermissionRequested`: a hook may settle the question before a person is
/// asked. An approval is put to the policy as a hook's answer, so it counts
/// as much as for `PreToolUse`: not for what only a person may answer, not
/// for a skill's inline command.
async fn settled_by_hook(
    ctx: &ToolExecCtx,
    name: &str,
    input: &serde_json::Value,
    asked: &Asked,
    request: base::interface::permission::PermissionRequest<'_>,
) -> Option<Answer> {
    use base::interface::permission::{HookAnswer, PermissionOutcome, Reason};
    if !ctx
        .hooks
        .has_hooks_for(hooks::HookEvent::PermissionRequested)
    {
        return None;
    }
    let hook_input = lifecycle_input(ctx, hooks::HookEvent::PermissionRequested)
        .with_tool(name, input.clone())
        .with_reason(&asked.message);
    let result = ctx
        .hooks
        .run(
            hooks::HookEvent::PermissionRequested,
            &hook_input,
            &ctx.cancel,
        )
        .await;
    if ctx.cancel.is_cancelled() {
        return None;
    }
    if let Some(response) = result.blocked() {
        let why = response.message.as_deref().unwrap_or("no reason given");
        let message = format!("Denied by PermissionRequested hook: {why}");
        return Some(Answer::Refused(
            Reason::new(asked.reason.stage, &message),
            Refusal {
                reason: message.clone(),
                error: message,
            },
        ));
    }
    if result.approved().is_none() || ctx.caller == Caller::InlineCommand {
        return None;
    }
    let approval = HookAnswer::Approve {
        hook: "PermissionRequested".into(),
    };
    match ctx
        .permission
        .policy
        .decide(&request.with_hook(&approval))
        .await
    {
        permitted @ PermissionOutcome::Permit { .. } => Some(Answer::Decided(permitted)),
        _ => None,
    }
}

async fn ask(
    ctx: &ToolExecCtx,
    name: &str,
    input: &serde_json::Value,
    facts: &base::interface::permission::ToolFacts,
    tool_ctx: &base::tool::ToolContext,
    asked: Asked,
    request: base::interface::permission::PermissionRequest<'_>,
) -> Answer {
    use base::interface::permission::Reason;
    if let Some(settled) = settled_by_hook(ctx, name, input, &asked, request).await {
        return settled;
    }
    let Asked {
        prompt_id,
        message,
        paths,
        reason: asked,
        ..
    } = asked;
    let refused =
        |refusal: Refusal| Answer::Refused(Reason::new(asked.stage, &refusal.reason), refusal);
    let waited = std::time::Instant::now();
    announce_prompt(ctx, name, input).await;
    let prompt_timeout = ctx.config.permission_prompt_timeout_secs;
    let ask = ctx.elicitation.ask_within(
        base::interface::elicitation::ElicitRequest {
            id: prompt_id.clone(),
            kind: base::interface::elicitation::ElicitKind::Authorization {
                tool_name: name.to_string(),
                paths,
                tool_use_id: Some(ctx.tool_use_id.clone()),
            },
            message,
            options: crate::elicitation::ChannelElicitation::authorization_options(
                ctx.permission.grants.is_some(),
            ),
        },
        (prompt_timeout > 0).then(|| std::time::Duration::from_secs(prompt_timeout)),
    );
    // An answer that already took the prompt is the answer, cancelled or not:
    // the host was told it was answered, and an "always allow" is kept.
    let outcome = tokio::select! {
        biased;
        outcome = ask => match outcome {
            Some(outcome) => outcome,
            None => {
                return refused(Refusal {
                    reason: "permission prompt timed out".into(),
                    error: format!(
                        "Denied by permission: no answer to the permission prompt within \
                         {prompt_timeout}s"
                    ),
                });
            }
        },
        // Dropping the ask is what discards a late answer for this id — see
        // `ChannelElicitation`'s registration guard.
        _ = ctx.cancel.cancelled() => {
            return Answer::Cancelled { waited_ms: waited.elapsed().as_millis() as u64 };
        }
    };
    let permitted = Answer::Decided(permitted_by_person(facts, tool_ctx));
    match outcome.answer_as::<crate::agent::PermissionDecision>() {
        Some(crate::agent::PermissionDecision::Permit) => permitted,
        Some(crate::agent::PermissionDecision::PermitAlways { scope }) => {
            grant_always(ctx, name, facts.subject.content(), scope).await;
            permitted
        }
        Some(crate::agent::PermissionDecision::Deny { reason }) => refused(Refusal::denied(reason)),
        // Declined, or answered with something that is not a decision: the
        // decliner's own words are what the model is told.
        None => {
            let reason = outcome
                .decline_reason()
                .unwrap_or("the answer to the permission prompt was not a decision")
                .to_string();
            refused(Refusal {
                error: reason.clone(),
                reason,
            })
        }
    }
}

fn permitted_by_person(
    facts: &base::interface::permission::ToolFacts,
    tool_ctx: &base::tool::ToolContext,
) -> base::interface::permission::PermissionOutcome {
    use base::interface::permission::{Basis, ExecProfile, Obligations, PermissionOutcome};
    let profile = ExecProfile::for_permit(&Basis::Person, facts.runs, tool_ctx.attendance);
    PermissionOutcome::Permit {
        basis: Basis::Person,
        profile,
        obligations: Obligations::after(facts.runs, profile),
    }
}

/// Keep a person's "always allow", for exactly what they were asked about.
async fn grant_always(
    ctx: &ToolExecCtx,
    name: &str,
    content: Option<&str>,
    scope: crate::agent::PersistScope,
) {
    use base::interface::permission::{Grant, GrantScope};
    let Some(grants) = &ctx.permission.grants else {
        tracing::warn!(
            tool_name = %name,
            "answered permit_always, which this session did not offer: it keeps no grants, so \
             the call is allowed once"
        );
        return;
    };
    let scope = match scope {
        crate::agent::PersistScope::Session => GrantScope::Session,
        crate::agent::PersistScope::Local => GrantScope::Project,
    };
    let grant = Grant::new(name, content.map(str::to_string), scope);
    if let Err(e) = grants.grant(grant).await {
        tracing::warn!(tool_name = %name, error = %e, "permit-always not kept as asked");
        if let Some(events) = &ctx.events {
            let _ = events.send(base::event::AgentEvent::Notification {
                message: format!("allow `{name}` every time: {e}"),
                status: None,
                turn_id: ctx.turn_id.clone(),
            });
        }
    }
}
