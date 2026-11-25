//! `mcp.*`：属于 `mcp` 模块，没编进这个构建时答 `METHOD_NOT_FOUND` 加
//! `data.module`。`mcp.callTool` 的 `run_id` 与 `plugin.call` 的同一张按连接的登记表，
//! `mcp.cancel` 与 `plugin.cancel` 谁都取消得了哪一种。

use super::*;
use crate::rpc::methods::mcp::{
    McpAuthCancelParams, McpAuthCompleteParams, McpAuthLogoutParams, McpAuthStartParams,
    McpCallToolParams, McpCancelParams, McpCancelled, McpPutServerParams, McpReconnectParams,
    McpRedirectKind, McpRemoveServerParams, McpSetEnabledParams, McpSetIdentityParams,
    McpToolsParams,
};
use crate::rpc::methods::ConfigTarget;
use crate::session_pool::api::{
    McpAuthDone, McpAuthStarted, McpIdentity, McpReconnected, McpRedirect, McpServerReconnected,
    McpServerRef, McpServerWritten, McpStatusList, McpToolCall, McpToolCallResult,
    McpToolCallTarget, McpToolList,
};
use crate::session_pool::MCP_COMPILED;

rpc_methods! {
    "mcp.status" => status(ConfigTarget) -> McpStatusList ["mcp", MCP_COMPILED];
    "mcp.addServer" => add_server(McpPutServerParams) -> McpServerWritten ["mcp", MCP_COMPILED];
    "mcp.updateServer" => update_server(McpPutServerParams) -> McpServerWritten ["mcp", MCP_COMPILED];
    "mcp.removeServer" => remove_server(McpRemoveServerParams) -> McpServerWritten ["mcp", MCP_COMPILED];
    "mcp.setEnabled" => set_enabled(McpSetEnabledParams) -> McpServerWritten ["mcp", MCP_COMPILED];
    "mcp.reconnect" => reconnect(McpReconnectParams) -> McpServerReconnected ["mcp", MCP_COMPILED];
    "mcp.auth.start" => auth_start(McpAuthStartParams) -> McpAuthStarted ["mcp", MCP_COMPILED];
    "mcp.auth.complete" => auth_complete(McpAuthCompleteParams) -> McpAuthDone ["mcp", MCP_COMPILED];
    "mcp.auth.cancel" => auth_cancel(McpAuthCancelParams) -> McpServerRef ["mcp", MCP_COMPILED];
    "mcp.auth.logout" => auth_logout(McpAuthLogoutParams) -> McpServerRef ["mcp", MCP_COMPILED];
    "mcp.auth.setIdentity" => auth_set_identity(McpSetIdentityParams) -> McpReconnected ["mcp", MCP_COMPILED];
    "mcp.tools" => tools(McpToolsParams) -> McpToolList ["mcp", MCP_COMPILED];
    "mcp.callTool" => call_tool(McpCallToolParams) -> McpToolCallResult ["mcp", MCP_COMPILED];
    "mcp.cancel" => cancel(McpCancelParams) -> McpCancelled ["mcp", MCP_COMPILED];
}

fn invalid(message: impl Into<String>) -> PoolError {
    PoolError::InvalidRequest(message.into())
}

/// The pair `target` names, or the session `session_id` names; not both.
fn call_target_of(
    s: &DaemonServer,
    session_id: Option<String>,
    target: ConfigTarget,
) -> Result<McpToolCallTarget, PoolError> {
    match session_id {
        Some(_) if target.scene.is_some() || target.project_root.is_some() => Err(invalid(
            "give `session_id` or `scene` / `project_root`, not both",
        )),
        Some(id) => Ok(McpToolCallTarget::Session(id)),
        None => {
            let (project_root, scene) = s.target_of(target);
            Ok(McpToolCallTarget::Pair {
                project_root,
                scene,
            })
        }
    }
}

async fn tools(
    s: &DaemonServer,
    _: &Call,
    params: McpToolsParams,
) -> Result<McpToolList, PoolError> {
    let target = call_target_of(s, params.session_id, params.target)?;
    s.pool.mcp_tools(params.server.as_deref(), &target).await
}

async fn call_tool(
    s: &DaemonServer,
    cx: &Call,
    params: McpCallToolParams,
) -> Result<McpToolCallResult, PoolError> {
    let target = call_target_of(s, params.session_id, params.target)?;
    if params.server.is_empty() || params.tool.is_empty() {
        return Err(invalid("`server` and `tool` are not empty"));
    }
    let mut call = McpToolCall::new(params.server, params.tool, target);
    call.arguments = params.arguments.unwrap_or_default();
    call.timeout = params.timeout_ms.map(std::time::Duration::from_millis);
    call.run_id = params.run_id.clone();
    as_named_run(cx, params.run_id.as_deref(), |cancel| {
        s.pool.call_mcp_tool(call, cancel)
    })
    .await
}

async fn cancel(
    _: &DaemonServer,
    cx: &Call,
    params: McpCancelParams,
) -> Result<McpCancelled, PoolError> {
    Ok(McpCancelled {
        cancelled: cx.client.cancel_run(&params.run_id),
    })
}

async fn status(
    s: &DaemonServer,
    _: &Call,
    target: ConfigTarget,
) -> Result<McpStatusList, PoolError> {
    let (project_root, scene) = s.target_of(target);
    s.pool.mcp_status(project_root.as_deref(), &scene).await
}

async fn add_server(
    s: &DaemonServer,
    _: &Call,
    params: McpPutServerParams,
) -> Result<McpServerWritten, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    s.pool
        .add_mcp_server(
            project_root.as_deref(),
            &scene,
            params.tier,
            &params.name,
            params.config,
        )
        .await
}

async fn update_server(
    s: &DaemonServer,
    _: &Call,
    params: McpPutServerParams,
) -> Result<McpServerWritten, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    s.pool
        .update_mcp_server(
            project_root.as_deref(),
            &scene,
            params.tier,
            &params.name,
            params.config,
        )
        .await
}

async fn remove_server(
    s: &DaemonServer,
    _: &Call,
    params: McpRemoveServerParams,
) -> Result<McpServerWritten, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    s.pool
        .remove_mcp_server(project_root.as_deref(), &scene, params.tier, &params.name)
        .await
}

async fn set_enabled(
    s: &DaemonServer,
    _: &Call,
    params: McpSetEnabledParams,
) -> Result<McpServerWritten, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    s.pool
        .set_mcp_enabled(
            project_root.as_deref(),
            &scene,
            params.tier,
            &params.name,
            params.enabled,
        )
        .await
}

async fn reconnect(
    s: &DaemonServer,
    _: &Call,
    params: McpReconnectParams,
) -> Result<McpServerReconnected, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    s.pool
        .mcp_reconnect(project_root.as_deref(), &scene, &params.name)
        .await
}

async fn auth_start(
    s: &DaemonServer,
    _: &Call,
    params: McpAuthStartParams,
) -> Result<McpAuthStarted, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    let redirect = match (params.redirect, params.redirect_uri) {
        (McpRedirectKind::Loopback, None) => McpRedirect::Loopback,
        (McpRedirectKind::Loopback, Some(_)) => {
            return Err(invalid(
                "redirect_uri is only for \"redirect\": \"external\"",
            ))
        }
        (McpRedirectKind::External, Some(uri)) => McpRedirect::External(uri),
        (McpRedirectKind::External, None) => {
            return Err(invalid("\"redirect\": \"external\" needs redirect_uri"))
        }
    };
    s.pool
        .mcp_auth_start(
            project_root.as_deref(),
            &scene,
            &params.server,
            redirect,
            params.scopes,
        )
        .await
}

async fn auth_complete(
    s: &DaemonServer,
    _: &Call,
    params: McpAuthCompleteParams,
) -> Result<McpAuthDone, PoolError> {
    s.pool
        .mcp_auth_complete(&params.flow_id, &params.callback_url)
        .await
}

async fn auth_cancel(
    s: &DaemonServer,
    _: &Call,
    params: McpAuthCancelParams,
) -> Result<McpServerRef, PoolError> {
    s.pool.mcp_auth_cancel(&params.flow_id).await
}

async fn auth_logout(
    s: &DaemonServer,
    _: &Call,
    params: McpAuthLogoutParams,
) -> Result<McpServerRef, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    s.pool
        .mcp_auth_logout(project_root.as_deref(), &scene, &params.server)
        .await
}

async fn auth_set_identity(
    s: &DaemonServer,
    _: &Call,
    params: McpSetIdentityParams,
) -> Result<McpReconnected, PoolError> {
    let text = |value: Option<String>| value.filter(|token| !token.is_empty());
    let identity = match (text(params.id_token), text(params.refresh_token)) {
        (Some(token), None) => McpIdentity::IdToken(token),
        (None, Some(token)) => McpIdentity::RefreshToken(token),
        _ => return Err(invalid("give exactly one of id_token / refresh_token")),
    };
    s.pool.mcp_auth_set_identity(identity).await
}
