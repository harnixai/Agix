//! `agixd` 在嵌入入口之上加的传输：TCP 与 WebSocket 监听、它们共用的 token、unix
//! socket 的 `daemon.auth` 握手与它的 token 文件。不在 `embed` 承诺稳定的面里，随
//! `agixd` 的命令行变。

use std::net::SocketAddr;
use std::sync::Arc;

use tracing::info;

use crate::discovery::{self, UnixSocketTokenFile};
use crate::embed::{Announced, Embedded};
use crate::server::DaemonServer;

/// The transports beyond the unix socket, already validated.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Listeners {
    pub tcp: Option<SocketAddr>,
    /// Loopback only.
    pub ws: Option<SocketAddr>,
    /// The `daemon.auth` secret for TCP and WebSocket, and for the unix
    /// socket under `unix_socket_auth`.
    pub token: Option<String>,
    /// The unix socket, too, wants `daemon.auth`; with no `token`, a fresh
    /// one is written to `<data_root>/daemon/auth/<instance>.token`.
    pub unix_socket_auth: bool,
}

impl Listeners {
    pub fn new(
        tcp: Option<SocketAddr>,
        ws: Option<SocketAddr>,
        token: Option<String>,
        unix_socket_auth: bool,
    ) -> Self {
        Self {
            tcp,
            ws,
            token,
            unix_socket_auth,
        }
    }
}

/// Bind every listener, then serve the unix socket and announce the
/// instance until the engine begins shutting down. A port already in use
/// fails here, before the instance is announced, so a client that finds the
/// `instances.d` entry can connect at once.
pub async fn serve(embedded: &Embedded, listeners: Listeners) -> anyhow::Result<()> {
    let server = embedded.server();
    if (listeners.tcp.is_some() || listeners.ws.is_some()) && listeners.token.is_none() {
        anyhow::bail!("--listen and --listen-ws require --token or env AGIX_DAEMON_TOKEN");
    }
    if let Some(token) = listeners.token.clone() {
        server.set_tcp_token(token).await;
    }
    let token_file = if listeners.unix_socket_auth {
        unix_socket_auth(embedded, listeners.token.is_some()).await?
    } else {
        None
    };
    bind_network_listeners(server, &listeners).await?;
    embedded
        .serve_unix_announcing(Announced {
            tcp: listeners.tcp,
            ws: listeners.ws,
            unix_socket_auth: listeners.unix_socket_auth,
            token_file: token_file.as_ref().map(|f| f.path().to_path_buf()),
        })
        .await?;
    drop(token_file);
    Ok(())
}

/// Require `daemon.auth` on the unix socket: against the given token, else
/// a fresh one written beside the instance's other files.
async fn unix_socket_auth(
    embedded: &Embedded,
    has_token: bool,
) -> anyhow::Result<Option<UnixSocketTokenFile>> {
    let server = embedded.server();
    let file = if has_token {
        None
    } else {
        let (Some(root), Some(name)) = (embedded.daemon_root(), embedded.instance_name()) else {
            anyhow::bail!("--unix-socket-auth without a token needs a named instance");
        };
        let path = discovery::unix_socket_token_path(root, name);
        let (token, file) = UnixSocketTokenFile::create(&path).map_err(|e| {
            anyhow::anyhow!(
                "--unix-socket-auth: could not write {}: {e}",
                path.display()
            )
        })?;
        server.set_tcp_token(token).await;
        Some(file)
    };
    server.require_unix_socket_auth();
    Ok(file)
}

async fn bind_network_listeners(
    server: &Arc<DaemonServer>,
    listeners: &Listeners,
) -> anyhow::Result<()> {
    if let Some(addr) = listeners.tcp {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("could not bind --listen `{addr}`: {e}"))?;
        info!(%addr, "TCP listener bound (token-auth required)");
        let s = server.clone();
        tokio::spawn(async move { s.serve_tcp_listener(listener).await });
    }
    if let Some(addr) = listeners.ws {
        #[cfg(not(feature = "ws"))]
        anyhow::bail!("--listen-ws `{addr}`: this build has no WebSocket listener (feature `ws`)");
        #[cfg(feature = "ws")]
        {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| anyhow::anyhow!("could not bind --listen-ws `{addr}`: {e}"))?;
            info!(%addr, "WebSocket listener bound (token-auth required)");
            let s = server.clone();
            tokio::spawn(async move { crate::ws::serve_ws_listener(s, listener).await });
        }
    }
    Ok(())
}
