//! `exec.*`：只在嵌入方自己的连接上、且嵌入方装配时打开了 `host_exec` 才有；
//! 别的连接上在读参数之前就答得与不存在的方法一样（`table.rs` 的分派）。

use super::*;
use crate::rpc::methods::exec::{ExecCancelParams, ExecCancelled, ExecRunParams};
use crate::rpc::RpcError;
use crate::session_pool::api::HostExecResult;

rpc_methods! {
    "exec.run" => run(ExecRunParams) -> HostExecResult;
    "exec.cancel" => cancel(ExecCancelParams) -> ExecCancelled;
}

async fn run(s: &DaemonServer, cx: &Call, p: ExecRunParams) -> Result<HostExecResult, RpcError> {
    let run_id = p.run_id.clone();
    let request = p.into_request();
    as_named_run(cx, run_id.as_deref(), |cancel| async move {
        let shutdown = s.shutdown_token();
        let stopper = cancel.clone();
        let on_shutdown = tokio::spawn(async move {
            shutdown.cancelled().await;
            stopper.cancel();
        });
        let ran = s.pool.host_exec(request, cancel).await;
        on_shutdown.abort();
        ran
    })
    .await
    .map_err(RpcError::from)
}

async fn cancel(
    _: &DaemonServer,
    cx: &Call,
    p: ExecCancelParams,
) -> Result<ExecCancelled, RpcError> {
    Ok(ExecCancelled {
        cancelled: cx.client.cancel_run(&p.run_id),
    })
}
