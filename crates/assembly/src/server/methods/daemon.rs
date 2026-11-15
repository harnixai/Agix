//! `daemon.*` 与 `commands.list`。

use super::*;
use crate::rpc::methods::daemon::{
    CommandList, CommandsListParams, DaemonStatus, EventsSubscribed, Pong, ShuttingDown,
};

rpc_methods! {
    "daemon.status" => status(NoParams) -> DaemonStatus;
    "daemon.ping" => ping(NoParams) -> Pong;
    "daemon.doctor" => doctor(NoParams) -> crate::doctor::DoctorReport;
    "daemon.subscribeEvents" => subscribe_events(NoParams) -> EventsSubscribed;
    "daemon.shutdown" => shutdown(NoParams) -> ShuttingDown;
    "commands.list" => commands_list(CommandsListParams) -> CommandList;
}

async fn status(s: &DaemonServer, _: &Call, _: NoParams) -> Result<DaemonStatus, PoolError> {
    Ok(DaemonStatus {
        identity: crate::discovery::identity(),
        uptime_secs: s.started_at.elapsed().as_secs(),
        sessions: s.pool.active_count().await,
    })
}

async fn ping(_: &DaemonServer, _: &Call, _: NoParams) -> Result<Pong, PoolError> {
    Ok(Pong {
        identity: crate::discovery::identity(),
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
