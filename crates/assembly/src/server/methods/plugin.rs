//! `plugin.*`：包的管理，以及宿主调用包里的函数——`plugin.call` 的 `run_id` 按连接
//! 登记，连接断开时它还没完的调用就取消。插件子系统没编进来或被策略关掉时，除
//! `plugin.cancel` 外每个方法都答 `PLUGINS_DISABLED`，这一关在池里。

use super::*;
use crate::rpc::methods::plugin::{
    PluginCallParams, PluginCancelParams, PluginCancelled, PluginFunctionList,
    PluginFunctionsParams, PluginInstallParams, PluginList, PluginToggleParams,
    PluginUninstallParams,
};
use crate::session_pool::api::{
    PluginCall, PluginCallResult, PluginCallTarget, PluginInstalled, PluginRemoved, PluginToggled,
};

rpc_methods! {
    "plugin.list" => list(NoParams) -> PluginList;
    "plugin.install" => install(PluginInstallParams) -> PluginInstalled;
    "plugin.uninstall" => uninstall(PluginUninstallParams) -> PluginRemoved;
    "plugin.enable" => enable(PluginToggleParams) -> PluginToggled;
    "plugin.disable" => disable(PluginToggleParams) -> PluginToggled;
    "plugin.reload" => reload(NoParams) -> PluginList;
    "plugin.functions" => functions(PluginFunctionsParams) -> PluginFunctionList;
    "plugin.call" => call(PluginCallParams) -> PluginCallResult;
    "plugin.cancel" => cancel(PluginCancelParams) -> PluginCancelled;
}

fn target_of(
    session_id: Option<String>,
    cwd: Option<std::path::PathBuf>,
) -> Result<Option<PluginCallTarget>, PoolError> {
    match (session_id, cwd) {
        (Some(_), Some(_)) => Err(PoolError::InvalidRequest(
            "give `session_id` or `cwd`, not both".into(),
        )),
        (Some(id), None) => Ok(Some(PluginCallTarget::Session(id))),
        (None, Some(cwd)) if cwd.is_absolute() => Ok(Some(PluginCallTarget::Project(cwd))),
        (None, Some(cwd)) => Err(PoolError::InvalidRequest(format!(
            "`cwd` is an absolute path, not `{}`",
            cwd.display()
        ))),
        (None, None) => Ok(None),
    }
}

async fn functions(
    s: &DaemonServer,
    _: &Call,
    params: PluginFunctionsParams,
) -> Result<PluginFunctionList, PoolError> {
    let target = target_of(params.session_id, params.cwd)?;
    Ok(PluginFunctionList {
        functions: s.pool.plugin_functions(target.as_ref()).await?,
    })
}

async fn call(
    s: &DaemonServer,
    cx: &Call,
    params: PluginCallParams,
) -> Result<PluginCallResult, PoolError> {
    let target = target_of(params.session_id, params.cwd)?.ok_or_else(|| {
        PoolError::InvalidRequest("a call without `session_id` needs `cwd`".into())
    })?;
    let mut request = PluginCall::new(params.function, params.input, target);
    request.writes = params.writes;
    request.permission_rules = params.permission_rules;
    request.timeout = params.timeout_ms.map(std::time::Duration::from_millis);
    let cancel = CancellationToken::new();
    let _named = match &params.run_id {
        Some(run_id) => Some(cx.client.name_run(run_id, cancel.clone()).ok_or_else(|| {
            PoolError::InvalidRequest(format!(
                "a call of this connection is already running as `{run_id}`"
            ))
        })?),
        None => None,
    };
    let running = s.pool.call_plugin(request, cancel.clone());
    tokio::pin!(running);
    tokio::select! {
        result = &mut running => result,
        _ = cx.client.closed() => {
            cancel.cancel();
            running.await
        }
    }
}

async fn cancel(
    _: &DaemonServer,
    cx: &Call,
    params: PluginCancelParams,
) -> Result<PluginCancelled, PoolError> {
    Ok(PluginCancelled {
        cancelled: cx.client.cancel_run(&params.run_id),
    })
}

async fn list(s: &DaemonServer, _: &Call, _: NoParams) -> Result<PluginList, PoolError> {
    Ok(PluginList {
        plugins: s.pool.list_plugins().await?,
    })
}

async fn install(
    s: &DaemonServer,
    _: &Call,
    params: PluginInstallParams,
) -> Result<PluginInstalled, PoolError> {
    s.pool
        .install_plugin(
            &params.name,
            &params.version,
            &params.download_url,
            params.checksum.as_deref(),
            params.scope,
        )
        .await
}

async fn uninstall(
    s: &DaemonServer,
    _: &Call,
    params: PluginUninstallParams,
) -> Result<PluginRemoved, PoolError> {
    s.pool
        .uninstall_plugin(&params.name, params.version.as_deref(), params.scope)
        .await
}

async fn enable(
    s: &DaemonServer,
    _: &Call,
    params: PluginToggleParams,
) -> Result<PluginToggled, PoolError> {
    s.pool
        .set_plugin_enabled(&params.name, true, params.scope)
        .await
}

async fn disable(
    s: &DaemonServer,
    _: &Call,
    params: PluginToggleParams,
) -> Result<PluginToggled, PoolError> {
    s.pool
        .set_plugin_enabled(&params.name, false, params.scope)
        .await
}

async fn reload(s: &DaemonServer, _: &Call, _: NoParams) -> Result<PluginList, PoolError> {
    Ok(PluginList {
        plugins: s.pool.reload_plugins().await?,
    })
}
