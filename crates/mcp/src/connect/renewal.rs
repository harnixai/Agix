//! A server's token around each request: an unattended server's fetched
//! again when it is about to expire or is refused, and a refusal reported to
//! the pool's authorization state.

use super::StdioMcpClient;
use crate::error::McpError;
use std::sync::atomic::Ordering;

impl StdioMcpClient {
    pub(super) fn unattended(
        &self,
    ) -> Option<(
        &crate::oauth::McpAuth,
        String,
        &crate::config::McpOAuthConfig,
    )> {
        let oauth = self.config.oauth().filter(|o| o.grant.is_unattended())?;
        let auth = self
            .auth
            .as_deref()
            .filter(|_| self.config.oauth_capable())?;
        Some((auth, crate::oauth::server_url(&self.config)?, oauth))
    }

    /// Fetch an unattended server's token again when it is about to expire,
    /// or always when `renew`.
    pub(super) async fn keep_token(&self, renew: bool) -> Result<(), McpError> {
        match self.unattended() {
            Some((auth, url, oauth)) => {
                auth.renew_unattended(&self.server_name, &url, self.config.tier(), oauth, renew)
                    .await
            }
            None => Ok(()),
        }
    }

    /// Run `op` with a current token; an unattended server that refuses
    /// one gets a newly fetched one and `op` once more.
    pub(super) async fn authorized<T, F, Fut>(&self, op: F) -> Result<T, McpError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, McpError>>,
    {
        let result = self.renewing(op, || true).await;
        self.noted(result).await
    }

    /// One request with a current token, sent once more with a newly fetched
    /// one when an unattended server refuses it — a request the server never
    /// took, so sending it again does nothing twice. The refusal is left for
    /// the caller to note.
    pub(crate) async fn with_renewal<T, F, Fut>(&self, op: F) -> Result<T, McpError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, McpError>>,
    {
        self.renewing(op, || true).await
    }

    pub(super) async fn renewing<T, F, Fut>(
        &self,
        op: F,
        may_repeat: impl Fn() -> bool,
    ) -> Result<T, McpError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, McpError>>,
    {
        self.keep_token(false).await?;
        let first = op().await;
        if matches!(first, Err(McpError::AuthRequired { .. }))
            && self.unattended().is_some()
            && may_repeat()
        {
            self.keep_token(true).await?;
            return op().await;
        }
        first
    }

    /// Report an authorization refusal to the pool's authorization state —
    /// a 401 takes the server's tokens away, a 403 asks for more scope.
    pub(super) async fn noted<T>(&self, result: Result<T, McpError>) -> Result<T, McpError> {
        let Err(e) = result else {
            return result;
        };
        let auth = self.auth.as_ref().filter(|_| self.config.oauth_capable());
        if let (Some(auth), Some(url)) = (auth, crate::oauth::server_url(&self.config)) {
            match &e {
                McpError::AuthRequired { challenge, .. } => {
                    auth.note_unauthorized(
                        &self.server_name,
                        &url,
                        self.config.tier(),
                        challenge.as_deref(),
                        self.authorized.load(Ordering::SeqCst),
                    )
                    .await
                }
                McpError::InsufficientScope { challenge, .. } => auth.note_insufficient_scope(
                    &self.server_name,
                    &url,
                    self.config.tier(),
                    challenge,
                ),
                _ => {}
            }
        }
        Err(e)
    }
}
