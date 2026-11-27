//! `daemon.*` 与 `commands.list`。

use super::*;
use crate::rpc::methods::daemon::{
    CommandList, CommandsListParams, DaemonStatus, EventsSubscribed, Pong, SetHooksParams,
    SetHostToolsParams, ShuttingDown,
};
use crate::session_pool::api::{HooksSet, HostToolsSet};

rpc_methods! {
    "daemon.status" => status(NoParams) -> DaemonStatus;
    "daemon.ping" => ping(NoParams) -> Pong;
    "daemon.doctor" => doctor(NoParams) -> crate::doctor::DoctorReport;
    "daemon.subscribeEvents" => subscribe_events(NoParams) -> EventsSubscribed;
    "daemon.setHostTools" => set_host_tools(SetHostToolsParams) -> HostToolsSet;
    "daemon.setHooks" => set_hooks(SetHooksParams) -> HooksSet;
    "daemon.shutdown" => shutdown(NoParams) -> ShuttingDown;
    "commands.list" => commands_list(CommandsListParams) -> CommandList;
}

async fn status(s: &DaemonServer, cx: &Call, _: NoParams) -> Result<DaemonStatus, PoolError> {
    Ok(DaemonStatus {
        identity: s.identity_for(&cx.client),
        uptime_secs: s.started_at.elapsed().as_secs(),
        sessions: s.pool.active_count().await,
    })
}

async fn ping(s: &DaemonServer, cx: &Call, _: NoParams) -> Result<Pong, PoolError> {
    Ok(Pong {
        identity: s.identity_for(&cx.client),
        pong: true,
    })
}

async fn doctor(
    s: &DaemonServer,
    _: &Call,
    _: NoParams,
) -> Result<crate::doctor::DoctorReport, PoolError> {
    Ok(s.pool.doctor_report().await)
}

async fn subscribe_events(
    s: &DaemonServer,
    cx: &Call,
    _: NoParams,
) -> Result<EventsSubscribed, PoolError> {
    Ok(s.daemon_subscribe_events(cx.client.clone()).await)
}

async fn set_host_tools(
    s: &DaemonServer,
    cx: &Call,
    p: SetHostToolsParams,
) -> Result<HostToolsSet, PoolError> {
    Ok(s.pool.set_host_tools(cx.client.clone(), p.host_tools))
}

async fn set_hooks(s: &DaemonServer, cx: &Call, p: SetHooksParams) -> Result<HooksSet, PoolError> {
    s.pool
        .set_hooks(cx.client.clone(), p.hooks.unwrap_or_default())
}

async fn shutdown(s: &DaemonServer, _: &Call, _: NoParams) -> Result<ShuttingDown, PoolError> {
    s.shutdown().await;
    Ok(ShuttingDown {
        shutting_down: true,
    })
}

async fn commands_list(
    s: &DaemonServer,
    _: &Call,
    p: CommandsListParams,
) -> Result<CommandList, PoolError> {
    let commands = match p.session_id {
        Some(session_id) => s.pool.list_session_commands(&session_id).await?,
        None => s.pool.list_commands().await,
    };
    Ok(CommandList { commands })
}
