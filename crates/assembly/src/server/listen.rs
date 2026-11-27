//! Listeners: binding and accept loops for the Unix socket and TCP, including
//! the refusal of connections from processes this daemon started.

use super::*;
use std::time::Duration;
use tracing::error;

/// A unix listener [`DaemonServer::bind_unix`] bound, and the sandbox fence
/// around its socket, which comes down when the listener does.
pub struct UnixBinding {
    listener: UnixListener,
    _fence: platform::sandbox::Fence,
}

/// The pid of the peer on `stream`, if this process started it.
pub(super) fn started_by_this_daemon(stream: &tokio::net::UnixStream) -> Option<u32> {
    let pid = u32::try_from(stream.peer_cred().ok()?.pid()?).ok()?;
    platform::lineage::started_here(pid).then_some(pid)
}

/// Whether the peer on `stream` runs as a different user than this daemon.
/// The socket is chmod 0600 only after `bind`, so for that window the file
/// mode is whatever the umask left; checking the peer on every accept closes
/// it without touching the process-wide umask, which other threads would
/// race with. A peer whose credentials cannot be read is treated as foreign.
pub(super) fn from_another_user(stream: &tokio::net::UnixStream) -> bool {
    match stream.peer_cred() {
        Ok(cred) => differs_from_daemon_user(cred.uid()),
        Err(_) => true,
    }
}

fn differs_from_daemon_user(peer_uid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        peer_uid != unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        let _ = peer_uid;
        false
    }
}

/// One error frame, so the refused process learns why, then the connection
/// is dropped unread.
pub(super) async fn refuse_own_descendant(mut stream: tokio::net::UnixStream, pid: u32) {
    warn!(
        pid,
        "refused a connection on the unix socket from a process this daemon started"
    );
    let refusal = RpcResponse::err(
        serde_json::Value::Null,
        codes::UNAUTHORIZED,
        "this daemon does not serve processes it started itself",
    );
    if let Ok(mut line) = serde_json::to_vec(&refusal) {
        line.push(b'\n');
        let _ = stream.write_all(&line).await;
    }
}

const PROBE_PAUSE: Duration = Duration::from_millis(10);
const PROBE_PAUSES: u32 = 100;

const ACCEPT_BACKOFF_FIRST: Duration = Duration::from_millis(10);
const ACCEPT_BACKOFF_CAP: Duration = Duration::from_secs(1);
const ACCEPT_FAILURES_BEFORE_ERROR: u32 = 10;

fn backoff_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    (ACCEPT_BACKOFF_FIRST * 2u32.pow(doublings)).min(ACCEPT_BACKOFF_CAP)
}

/// The next accepted connection, or `None` once `shutdown` fires. A failing
/// `accept` (file descriptors exhausted, say, fails every call at once) is
/// retried after a delay that doubles up to a second, not in a tight loop.
async fn accept_backing_off<S, E, Fut>(
    shutdown: &CancellationToken,
    mut accept: impl FnMut() -> Fut,
) -> Option<S>
where
    E: std::fmt::Display,
    Fut: std::future::Future<Output = Result<S, E>>,
{
    let mut failures = 0u32;
    loop {
        let result = tokio::select! {
            _ = shutdown.cancelled() => return None,
            result = accept() => result,
        };
        match result {
            Ok(accepted) => return Some(accepted),
            Err(e) => {
                failures = failures.saturating_add(1);
                if failures == ACCEPT_FAILURES_BEFORE_ERROR {
                    error!(error=%e, failures, "accept keeps failing; the listener is backing off");
                } else {
                    warn!(error=%e);
                }
                tokio::select! {
                    _ = shutdown.cancelled() => return None,
                    _ = tokio::time::sleep(backoff_delay(failures)) => {}
                }
            }
        }
    }
}

impl DaemonServer {
    /// Accept loop over an already-bound TCP listener — the caller binds
    /// the listener, so tests can use an ephemeral port (`127.0.0.1:0`).
    ///
    /// Fails without a configured token: forgetting `set_tcp_token` would
    /// otherwise not fail loudly — every connection would just be rejected
    /// by `authenticate_tcp` forever.
    pub async fn serve_tcp_listener(self: Arc<Self>, listener: TcpListener) -> anyhow::Result<()> {
        if self.tcp_token.read().await.is_none() {
            anyhow::bail!("TCP requires token");
        }
        let _fence = listener
            .local_addr()
            .ok()
            .map(platform::sandbox::fence_port);
        while let Some((stream, peer)) =
            accept_backing_off(&self.shutdown_token, || listener.accept()).await
        {
            debug!(peer=%peer);
            let (r, w) = stream.into_split();
            tokio::spawn(self.clone().serve_byte_stream(
                Box::new(r),
                Box::new(w),
                Peer::Accepted { needs_auth: true },
            ));
        }
        Ok(())
    }

    pub async fn serve_unix(self: Arc<Self>, socket_path: &Path) -> anyhow::Result<()> {
        let listener = Self::bind_unix(socket_path)?;
        self.serve_unix_listener(listener).await
    }

    /// Bind the unix socket, replacing a stale one, readable and writable by
    /// this user only; a directory it creates for it is this user's only
    /// too. One something still answers on for about a second is not
    /// replaced: that fails with [`std::io::ErrorKind::AddrInUse`]. The
    /// second is for a listener its server has already closed but a child
    /// forked a moment before still holds, as it holds every descriptor,
    /// until it execs: that one accepts connections nobody will serve.
    /// Split from [`Self::serve_unix_listener`] so a caller can bind every
    /// listener before announcing the daemon — a client that finds it must
    /// be able to connect. No command this process sandboxes can connect to
    /// it.
    pub fn bind_unix(socket_path: &Path) -> anyhow::Result<UnixBinding> {
        let mut pauses = 0;
        Self::bind_unix_probing(socket_path, || {
            pauses += 1;
            std::thread::sleep(PROBE_PAUSE);
            pauses <= PROBE_PAUSES
        })
    }

    /// [`Self::bind_unix`], running `pause` each time the socket still
    /// answers; `false` gives up.
    fn bind_unix_probing(
        socket_path: &Path,
        mut pause: impl FnMut() -> bool,
    ) -> anyhow::Result<UnixBinding> {
        if let Some(parent) = socket_path.parent() {
            crate::discovery::create_private_dir(parent)?;
        }
        if socket_path.exists() {
            while std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
                if !pause() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        format!("{} is being served already", socket_path.display()),
                    )
                    .into());
                }
            }
            std::fs::remove_file(socket_path)?;
        }
        let listener = UnixListener::bind(socket_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
        }
        let fence = platform::sandbox::fence_socket(socket_path);
        info!(path=%socket_path.display());
        Ok(UnixBinding {
            listener,
            _fence: fence,
        })
    }

    /// Accept loop over an already-bound unix listener.
    ///
    /// A connection from a process this daemon started — a command, a hook,
    /// an MCP server, or what one of them left running — is refused: file
    /// permissions cannot tell it from the host, and as a client it could
    /// trust its own project or answer its own permission prompts. The
    /// lineage check misses a process that left its process group; under
    /// [`Self::require_unix_socket_auth`] such a process also needs the token.
    pub async fn serve_unix_listener(self: Arc<Self>, bound: UnixBinding) -> anyhow::Result<()> {
        let UnixBinding { listener, _fence } = bound;
        while let Some((stream, _addr)) =
            accept_backing_off(&self.shutdown_token, || listener.accept()).await
        {
            let this = self.clone();
            tokio::spawn(async move {
                if from_another_user(&stream) {
                    warn!("refused a unix socket connection from another user");
                    return;
                }
                if let Some(pid) = started_by_this_daemon(&stream) {
                    refuse_own_descendant(stream, pid).await;
                    return;
                }
                let needs_auth = this
                    .unix_socket_auth
                    .load(std::sync::atomic::Ordering::SeqCst);
                let (r, w) = stream.into_split();
                this.serve_byte_stream(Box::new(r), Box::new(w), Peer::Accepted { needs_auth })
                    .await;
            });
        }
        Ok(())
    }

    /// Serve one connection over a byte stream: newline-delimited frames, the
    /// frame cap, the in-flight bound and the outbound queue, the same on
    /// every transport that is a stream of bytes.
    pub(crate) async fn serve_byte_stream(
        self: Arc<Self>,
        reader: Reader,
        writer: Box<dyn AsyncWrite + Send + Unpin + 'static>,
        peer: Peer,
    ) {
        let writer: ByteWriter = Arc::new(AsyncMutex::new(writer));
        let sink: Sink = Arc::new(LineSink(writer));
        if let Err(e) = self.handle_connection(reader, sink, peer).await {
            warn!(error=%e);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn only_the_daemons_own_uid_is_not_foreign() {
        let mine = unsafe { libc::geteuid() };
        assert!(!differs_from_daemon_user(mine));
        assert!(differs_from_daemon_user(mine.wrapping_add(1)));
    }

    #[tokio::test]
    async fn a_peer_in_this_process_is_not_another_user() {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        assert!(!from_another_user(&a));
        assert!(!from_another_user(&b));
    }

    #[tokio::test]
    async fn a_failing_accept_backs_off_instead_of_spinning() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let stopper = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            stopper.cancel();
        });
        let accepted: Option<()> = accept_backing_off(&shutdown, || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Err::<(), _>(std::io::Error::other("too many open files")) }
        })
        .await;
        assert!(accepted.is_none());
        let n = attempts.load(std::sync::atomic::Ordering::SeqCst);
        assert!((2..=8).contains(&n), "{n} attempts in 300ms");
    }

    #[tokio::test]
    async fn a_listener_only_a_forked_child_still_holds_is_waited_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let closed = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut holder = std::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::os::fd::OwnedFd::from(closed))
            .spawn()
            .unwrap();
        assert!(
            std::os::unix::net::UnixStream::connect(&path).is_ok(),
            "the child's copy of the listener accepts"
        );
        let mut release = holder.stdin.take();
        let mut pauses = 0;
        let bound = DaemonServer::bind_unix_probing(&path, || {
            pauses += 1;
            if release.take().is_some() {
                holder.wait().unwrap();
            } else {
                std::thread::sleep(PROBE_PAUSE);
            }
            pauses <= PROBE_PAUSES
        });
        holder.wait().unwrap();
        assert!(bound.is_ok(), "{:?}", bound.err());
        assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());
    }

    #[tokio::test]
    async fn a_listener_that_keeps_answering_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let _served = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut pauses = 0;
        let bound = DaemonServer::bind_unix_probing(&path, || {
            pauses += 1;
            pauses <= 3
        });
        let err = bound.err().expect("refused");
        assert_eq!(
            err.downcast_ref::<std::io::Error>().map(|e| e.kind()),
            Some(std::io::ErrorKind::AddrInUse)
        );
        assert_eq!(pauses, 4);
    }

    #[test]
    fn the_backoff_doubles_up_to_a_second() {
        let ms = |n| backoff_delay(n).as_millis();
        assert_eq!((ms(1), ms(2), ms(3)), (10, 20, 40));
        assert_eq!((ms(8), ms(30), ms(u32::MAX)), (1000, 1000, 1000));
    }
}
