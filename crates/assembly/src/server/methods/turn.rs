//! 跑一轮与上下文：`session.run_turn` / `compact` / `clear` / `respondToPrompt` /
//! `respondToolCall` / `inspectContext` / `dropMessages` / `editMessage` /
//! `rewind` / `checkpoints`。

use super::*;
use crate::rpc::methods::session::SessionRef;
use crate::rpc::methods::turn::*;
use crate::rpc::{read_options, RpcError};
use crate::session_pool::api::{
    AskKind, CheckpointList, ContextReport, Initiator, PoolError, RewindOutcome, RewindRequest,
    TurnRequest,
};
use crate::session_pool::{OnBusy, PromptAnswer};
use base::context::goal::TurnOrigin;

rpc_methods! {
    "session.run_turn" => run_turn(RunTurnParams) -> TurnReply;
    "session.compact" => compact(CompactParams) -> TurnReply;
    "session.clear" => clear(SessionRef) -> TurnReply;
    "session.respondToPrompt" => respond_to_prompt(RespondToPromptParams) -> PromptAnswered;
    "session.respondToolCall" => respond_tool_call(RespondToolCallParams) -> ToolCallAnswered;
    "session.inspectContext" => inspect_context(SessionRef) -> ContextReport;
    "session.dropMessages" => drop_messages(DropMessagesParams) -> Dropped;
    "session.editMessage" => edit_message(EditMessageParams) -> Edited;
    "session.rewind" => rewind(RewindParams) -> RewindOutcome;
    "session.checkpoints" => checkpoints(SessionRef) -> CheckpointList;
}

fn invalid(message: impl Into<String>) -> RpcError {
    PoolError::InvalidRequest(message.into()).into()
}

async fn run_turn(s: &DaemonServer, cx: &Call, p: RunTurnParams) -> Result<TurnReply, RpcError> {
    let turn_id = p.turn_id.unwrap_or_else(|| Id::new().to_string());
    // A turn's origin is read from its id, so a client may not use the
    // form of a goal round or a scheduled job, which the daemon starts itself.
    if TurnOrigin::from_turn_id(&turn_id) != TurnOrigin::Human {
        return Err(invalid(format!(
            "turn_id `{turn_id}` has the form of a turn the daemon starts itself \
             (a goal round or a scheduled job)"
        )));
    }
    let options = read_options(p.options.as_ref())?;
    if p.message.trim().is_empty() && p.attachments.is_empty() {
        return Err(invalid("message is empty and there are no attachments"));
    }
    let request = TurnRequest {
        session_id: p.session_id,
        message: p.message,
        attachments: p.attachments,
        turn_id,
        options,
        on_busy: p.on_busy.unwrap_or_default(),
        initiator: Initiator::Client,
    };
    let outcome = s.pool.run_turn(request, Some(cx.client.clone())).await?;
    Ok(outcome.into())
}

async fn compact(s: &DaemonServer, cx: &Call, p: CompactParams) -> Result<TurnReply, PoolError> {
    let command = p.command().map_err(PoolError::InvalidRequest)?;
    run_command(s, cx, p.session_id, &command).await
}

async fn clear(s: &DaemonServer, cx: &Call, p: SessionRef) -> Result<TurnReply, PoolError> {
    run_command(s, cx, p.session_id, "/clear").await
}

/// The slash command run as a turn of its own, so it streams and finishes
/// like one and is refused like one while another turn holds the session.
async fn run_command(
    s: &DaemonServer,
    cx: &Call,
    session_id: String,
    command: &str,
) -> Result<TurnReply, PoolError> {
    let request = TurnRequest {
        session_id: Some(session_id),
        message: command.to_string(),
        attachments: vec![],
        turn_id: Id::new().to_string(),
        options: None,
        on_busy: OnBusy::Reject,
        initiator: Initiator::Client,
    };
    let outcome = s.pool.run_turn(request, Some(cx.client.clone())).await?;
    Ok(outcome.into())
}

async fn respond_to_prompt(
    s: &DaemonServer,
    _: &Call,
    p: RespondToPromptParams,
) -> Result<PromptAnswered, RpcError> {
    let answer = match p.prompt_type.unwrap_or(AskKind::Permission) {
        AskKind::Permission => {
            PromptAnswer::Permission(p.decision.ok_or_else(|| invalid("missing decision"))?)
        }
        AskKind::Question => {
            PromptAnswer::Question(p.answer.ok_or_else(|| invalid("missing answer"))?)
        }
        AskKind::HostToolCall => {
            return Err(invalid(
                "a host tool call is answered with session.respondToolCall",
            ))
        }
    };
    s.pool
        .respond_to_prompt(&p.session_id, p.prompt_id.clone(), answer)
        .await?;
    Ok(PromptAnswered {
        prompt_id: p.prompt_id,
    })
}

async fn respond_tool_call(
    s: &DaemonServer,
    _: &Call,
    p: RespondToolCallParams,
) -> Result<ToolCallAnswered, RpcError> {
    let reply = p.reply().map_err(invalid)?;
    s.pool
        .respond_tool_call(&p.session_id, p.call_id.clone(), reply)
        .await?;
    Ok(ToolCallAnswered { call_id: p.call_id })
}

async fn inspect_context(
    s: &DaemonServer,
    _: &Call,
    p: SessionRef,
) -> Result<ContextReport, PoolError> {
    s.pool.inspect_context(&p.session_id).await
}

async fn drop_messages(
    s: &DaemonServer,
    cx: &Call,
    p: DropMessagesParams,
) -> Result<Dropped, RpcError> {
    if p.at.is_empty() {
        return Err(invalid(format!(
            "{} needs `at`, a list of message positions",
            cx.method
        )));
    }
    let dropped = s.pool.drop_messages(&p.session_id, p.at, p.total).await?;
    Ok(Dropped { dropped })
}

async fn edit_message(
    s: &DaemonServer,
    _: &Call,
    p: EditMessageParams,
) -> Result<Edited, PoolError> {
    let edited = s
        .pool
        .edit_message(&p.session_id, p.at, p.text, p.total)
        .await?;
    Ok(Edited { edited })
}

async fn rewind(s: &DaemonServer, _: &Call, p: RewindParams) -> Result<RewindOutcome, PoolError> {
    s.pool
        .rewind_session(RewindRequest {
            session_id: p.session_id,
            cut: p.cut,
            scope: p.scope,
            force: p.force,
            allow_partial: p.allow_partial,
            dry_run: p.dry_run,
            fork_first: p.fork_first,
        })
        .await
}

async fn checkpoints(
    s: &DaemonServer,
    _: &Call,
    p: SessionRef,
) -> Result<CheckpointList, PoolError> {
    s.pool.session_checkpoints(&p.session_id).await
}
