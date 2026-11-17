//! MCP 授权：授权状态的变化转成守护进程事件，令牌失效的服务器下线、重新登录后重连，
//! 以及 `mcp.auth.*`。

use super::*;
use crate::session_pool::*;

impl SessionPool {
    /// Report this pool's MCP authorization transitions as `daemon.event`s,
    /// and take a server whose tokens stopped working off every connection
    /// that has it.
    pub(in crate::session_pool) fn watch_mcp_auth(self: &Arc<Self>) {
        use mcp::oauth::AuthEvent;
        let pool = Arc::downgrade(self);
        self.mcp.auth.set_listener(Arc::new(move |event| {
            let Some(pool) = pool.upgrade() else {
                return;
            };
            pool.emit_event(match &event {
                AuthEvent::Required { server, scopes, .. }
                | AuthEvent::StepUp { server, scopes, .. } => api::DaemonEvent::McpAuthRequired {
                    server: server.clone(),
                    scopes: scopes.clone(),
                },
                AuthEvent::Completed { server, .. } => api::DaemonEvent::McpAuthCompleted {
                    server: server.clone(),
                },
                AuthEvent::Failed { server, error, .. } => api::DaemonEvent::McpAuthFailed {
                    server: server.clone(),
                    error: error.clone(),
                },
            });
            if let AuthEvent::Required { url, .. } = event {
                tokio::spawn(async move { pool.demote_mcp_servers(&url).await });
            }
        }));
    }

    pub(in crate::session_pool) async fn invalidate_mcp_keys(&self, keys: &[ProjectSceneKey]) {
        use crate::watch_hub::Tier;
        for (root, scene) in keys {
            let tier = match root {
                Some(root) => Tier::Project(root.clone()),
                None => Tier::Scene(scene.clone()),
            };
            self.invalidate_bindings(Some(&tier)).await;
        }
    }

    /// Disconnect every server signed in under `key` until someone signs in again.
    pub(in crate::session_pool) async fn demote_mcp_servers(&self, record: &str) {
        let mut touched = Vec::new();
        for (key, slot) in self.mcp.cache.slots() {
            let update = slot.update().await;
            let Some(manager) = update.current() else {
                continue;
            };
            let names: Vec<String> = manager
                .servers_at(record)
                .into_iter()
                .filter(|n| manager.clients().iter().any(|c| c.server_name() == n))
                .collect();
            if names.is_empty() {
                continue;
            }
            let mut updated = manager.share();
            for name in &names {
                updated.demote(name);
            }
            updated.refresh_tools().await;
            update.set(Arc::new(updated));
            touched.push(key);
        }
        self.invalidate_mcp_keys(&touched).await;
    }

    /// Connect again every server signed in under `key` — and only those — wherever this
    /// pool has it, connected or waiting for a sign-in. Each outcome is a
    /// `mcp_connected` / `mcp_connect_failed` event.
    pub(in crate::session_pool) async fn reconnect_mcp_servers(&self, key: &str) -> Vec<String> {
        self.reconnect_mcp_where(|manager| manager.servers_at(key))
            .await
    }

    /// [`Self::reconnect_mcp_servers`] for the servers `pick` names in each
    /// cached manager, each through [`Self::reconcile_mcp_server`]: admitted
    /// again from the pair's settings and connected anew, its other servers
    /// left as they are. At most one event per server, however many pairs
    /// have it: the best outcome among them, as [`after_sign_in`] tells it.
    pub(in crate::session_pool) async fn reconnect_mcp_where(
        &self,
        pick: impl Fn(&McpManager) -> Vec<String>,
    ) -> Vec<String> {
        let mut outcomes: HashMap<String, Option<Reconnected>> = HashMap::new();
        for (key, slot) in self.mcp.cache.slots() {
            let Some(manager) = slot.current() else {
                continue;
            };
            for name in pick(&manager) {
                let Some((status, event)) = self.reconcile_unannounced(&key, &name, true).await
                else {
                    continue;
                };
                let outcome = event.map(|event| Reconnected {
                    state: status.map(|s| s.state),
                    event,
                });
                let telling = |o: &Option<Reconnected>| match o {
                    Some(Reconnected {
                        event: api::DaemonEvent::McpConnected { .. },
                        ..
                    }) => 2,
                    Some(_) => 1,
                    None => 0,
                };
                let seen = outcomes.entry(name).or_insert(None);
                if telling(&outcome) > telling(seen) {
                    *seen = outcome;
                }
            }
        }
        let mut names: Vec<String> = outcomes.keys().cloned().collect();
        names.sort();
        for outcome in outcomes.into_values() {
            if let Some(event) = after_sign_in(outcome) {
                self.emit_event(event);
            }
        }
        names
    }

    /// `mcp.auth.setIdentity` — the user's enterprise identity, which every
    /// `"grant": "enterprise"` server waiting for one is then connected with.
    pub async fn mcp_auth_set_identity(
        &self,
        identity: api::McpIdentity,
    ) -> Result<api::McpReconnected, PoolError> {
        if self.mcp.auth.enterprise_idp().is_none() {
            return Err(PoolError::InvalidRequest(
                "no enterprise IdP is configured: the host's Assembly::mcp.enterprise_idp or \
                 the MCP policy's enterprise_idp names one"
                    .into(),
            ));
        }
        self.mcp.auth.set_identity(Some(match identity {
            api::McpIdentity::IdToken(token) => mcp::oauth::EnterpriseIdentity::IdToken(token),
            api::McpIdentity::RefreshToken(token) => {
                mcp::oauth::EnterpriseIdentity::RefreshToken(token)
            }
        }));
        let reconnected = self
            .reconnect_mcp_where(|manager| {
                manager
                    .pending()
                    .iter()
                    .filter(|p| {
                        manager
                            .config_of(&p.name)
                            .and_then(|c| c.oauth())
                            .is_some_and(|o| o.grant == mcp::config::OAuthGrant::Enterprise)
                    })
                    .map(|p| p.name.clone())
                    .collect()
            })
            .await;
        Ok(api::McpReconnected { reconnected })
    }

    /// The configuration of the MCP server `name` on a project, for a
    /// sign-in. A server the policy denies never gets one.
    pub(in crate::session_pool) async fn mcp_server_to_authorize(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
    ) -> Result<mcp::config::McpServerConfig, PoolError> {
        let root = project_root.unwrap_or(self.cwd());
        let manager = self.mcp_for_project(Some(root), scene_id).await;
        if let Some(cfg) = manager.config_of(name) {
            return Ok(cfg.clone());
        }
        if let Some(denied) = manager.denied().iter().find(|d| d.name == name) {
            return Err(PoolError::McpServerDenied(denied.reason.clone()));
        }
        Err(PoolError::InvalidRequest(format!(
            "no MCP server `{name}` is configured for this project"
        )))
    }

    /// `mcp.auth.start` — begin a sign-in to an MCP server. A loopback flow
    /// completes itself when the browser comes back; an external one waits
    /// for `mcp.auth.complete`.
    pub async fn mcp_auth_start(
        self: &Arc<Self>,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
        redirect: api::McpRedirect,
        scopes: Option<Vec<String>>,
    ) -> Result<api::McpAuthStarted, PoolError> {
        let redirect = match redirect {
            api::McpRedirect::Loopback => mcp::oauth::Redirect::Loopback,
            api::McpRedirect::External(uri) => mcp::oauth::Redirect::External(uri),
        };
        let cfg = self
            .mcp_server_to_authorize(project_root, scene_id, name)
            .await?;
        let mut start = self
            .mcp
            .auth
            .start(name, &cfg, redirect, scopes)
            .await
            .map_err(PoolError::from)?;
        if let Some(mut callbacks) = start.callbacks.take() {
            let pool = Arc::downgrade(self);
            let flow_id = start.flow_id.clone();
            tokio::spawn(async move {
                while let Some(callback) = callbacks.recv().await {
                    let Some(pool) = pool.upgrade() else {
                        return;
                    };
                    let outcome = pool.finish_mcp_auth(&flow_id, &callback.url).await;
                    let open = pool.mcp.auth.flow_server(&flow_id).await.is_some();
                    let _ = callback
                        .reply
                        .send(outcome.map(|done| done.server).map_err(|e| e.to_string()));
                    if !open {
                        return;
                    }
                }
            });
        }
        Ok(api::McpAuthStarted {
            flow_id: start.flow_id,
            authorize_url: start.authorize_url,
            redirect_uri: start.redirect_uri,
            expires_in: start.expires_in,
        })
    }

    pub(in crate::session_pool) async fn finish_mcp_auth(
        &self,
        flow_id: &str,
        callback_url: &str,
    ) -> Result<mcp::oauth::FlowDone, mcp::oauth::AuthFlowError> {
        let done = self.mcp.auth.complete(flow_id, callback_url).await?;
        self.reconnect_mcp_servers(&done.url).await;
        Ok(done)
    }

    /// `mcp.auth.complete` — finish a sign-in with the URL the browser was
    /// sent back to, then reconnect that server.
    pub async fn mcp_auth_complete(
        &self,
        flow_id: &str,
        callback_url: &str,
    ) -> Result<api::McpAuthDone, PoolError> {
        let done = self
            .finish_mcp_auth(flow_id, callback_url)
            .await
            .map_err(PoolError::from)?;
        Ok(api::McpAuthDone {
            server: done.server,
            scopes: done.scopes,
        })
    }

    pub async fn mcp_auth_cancel(&self, flow_id: &str) -> Result<api::McpServerRef, PoolError> {
        let server = self
            .mcp
            .auth
            .cancel(flow_id)
            .await
            .map_err(PoolError::from)?;
        Ok(api::McpServerRef { server })
    }

    /// `mcp.auth.logout` — revoke and forget a server's tokens, and
    /// disconnect it.
    pub async fn mcp_auth_logout(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        name: &str,
    ) -> Result<api::McpServerRef, PoolError> {
        let cfg = self
            .mcp_server_to_authorize(project_root, scene_id, name)
            .await?;
        self.mcp
            .auth
            .logout(name, &cfg)
            .await
            .map_err(PoolError::from)?;
        if let Some(key) = mcp::oauth::server_url(&cfg)
            .and_then(|u| mcp::oauth::record_key(name, cfg.tier(), &u).ok())
        {
            self.demote_mcp_servers(&key).await;
        }
        Ok(api::McpServerRef {
            server: name.to_string(),
        })
    }
}

/// How a server signed in to came out of being reconnected in one pair.
struct Reconnected {
    state: Option<api::McpServerState>,
    event: api::DaemonEvent,
}

/// What the host is told of a server once it is signed in and reconnected:
/// nothing when every pair has turned it off or dropped it since; a refusal
/// by the policy as it is; a connection that failed as one that failed after
/// authorizing.
fn after_sign_in(outcome: Option<Reconnected>) -> Option<api::DaemonEvent> {
    let Reconnected { state, event } = outcome?;
    match event {
        api::DaemonEvent::McpConnectFailed { server, error }
            if !matches!(
                state,
                Some(api::McpServerState::Denied | api::McpServerState::Untrusted)
            ) =>
        {
            Some(api::DaemonEvent::McpConnectFailed {
                server,
                error: format!("connect failed after authorizing: {error}"),
            })
        }
        other => Some(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(error: &str) -> api::DaemonEvent {
        api::DaemonEvent::McpConnectFailed {
            server: "docs".into(),
            error: error.into(),
        }
    }

    fn error_of(event: Option<api::DaemonEvent>) -> Option<String> {
        match event? {
            api::DaemonEvent::McpConnectFailed { error, .. } => Some(error),
            other => panic!("expected mcp_connect_failed, got {other:?}"),
        }
    }

    #[test]
    fn a_server_turned_off_while_signing_in_is_not_reported_as_failing() {
        assert!(after_sign_in(None).is_none());
        assert_eq!(
            error_of(after_sign_in(Some(Reconnected {
                state: Some(api::McpServerState::Failed),
                event: failed("refused"),
            }))),
            Some("connect failed after authorizing: refused".into())
        );
        assert_eq!(
            error_of(after_sign_in(Some(Reconnected {
                state: Some(api::McpServerState::Denied),
                event: failed("the MCP policy denies it"),
            }))),
            Some("the MCP policy denies it".into())
        );
    }
}
