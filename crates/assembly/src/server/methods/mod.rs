//! 各命名空间的方法表与类型化的处理函数，一个命名空间一个文件。

use super::table::Call;
use super::*;
use crate::rpc::NoParams;
use crate::session_pool::api::PoolError;

pub(super) mod config;
pub(super) mod daemon;
pub(super) mod exec;
pub(super) mod goal;
pub(super) mod history;
pub(super) mod import;
pub(super) mod mcp;
pub(super) mod plugin;
pub(super) mod project;
pub(super) mod scene;
pub(super) mod schedule;
pub(super) mod session;
pub(super) mod tasks;
pub(super) mod turn;

/// `run`, stopped by its token: `plugin.cancel` / `mcp.cancel` with
/// `run_id` on this connection, or the connection closing.
async fn as_named_run<T, F>(
    cx: &Call,
    run_id: Option<&str>,
    run: impl FnOnce(CancellationToken) -> F,
) -> Result<T, PoolError>
where
    F: std::future::Future<Output = Result<T, PoolError>>,
{
    let cancel = CancellationToken::new();
    let _named = match run_id {
        Some(run_id) => Some(cx.client.name_run(run_id, cancel.clone()).ok_or_else(|| {
            PoolError::InvalidRequest(format!(
                "a call of this connection is already running as `{run_id}`"
            ))
        })?),
        None => None,
    };
    let running = run(cancel.clone());
    tokio::pin!(running);
    tokio::select! {
        result = &mut running => result,
        _ = cx.client.closed() => {
            cancel.cancel();
            running.await
        }
    }
}
