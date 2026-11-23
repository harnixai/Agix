//! Who may talk to the daemon: socket permissions and the `daemon.auth` handshake over Unix and TCP.

use std::sync::Arc;

use crate::support::*;
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::rpc::codes;
use daemon::DaemonServer;
use model::client::{AnthropicClient, AuthMode, HttpAnthropicClient};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
#[tokio::test]
async fn unix_socket_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("new").join("dir").join("agixd.sock");
    let _listener = DaemonServer::bind_unix(&sock).unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&sock), 0o600);
    assert_eq!(mode(sock.parent().unwrap()), 0o700);
    assert_eq!(mode(&dir.path().join("new")), 0o700);
}

/// Mirrors `start_server()` but over TCP via `serve_tcp_listener` (bound
/// separately so the test knows the OS-assigned ephemeral port before the
/// accept loop starts — `serve_tcp` itself only takes a fixed `SocketAddr`).
async fn start_tcp_server(
    token: &str,
) -> (
    Arc<DaemonServer>,
    std::net::SocketAddr,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();

    let client: Arc<dyn AnthropicClient> =
        Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("test-key".into())).unwrap());

    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        None,
        None,
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.settings.values.app.session_cap = 8;

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(dir.path().to_path_buf()),
            model_client: Some(client),
            transcripts: daemon::Transcripts::Nowhere,
            host_policy: Some(Arc::new(AllowAllPermission)),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    server.set_tcp_token(token.to_string()).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server2 = server.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_tcp_listener(listener).await;
    });

    (server, addr, dir, handle)
}

/// Connect, send a `daemon.auth` handshake with `token`, and return the open
/// stream plus the parsed handshake response (so callers can keep using the
/// same connection afterward — unlike `rpc_call`, which opens/closes fresh).
async fn tcp_handshake(addr: std::net::SocketAddr, token: &str) -> (TcpStream, serde_json::Value) {
    let mut client = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        r#"{{"jsonrpc":"2.0","method":"daemon.auth","params":{{"token":"{token}"}},"id":0}}"#
    );
    client.write_all(req.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    let mut buf = String::new();
    {
        let (r, _) = client.split();
        let mut br = BufReader::new(r);
        br.read_line(&mut buf).await.unwrap();
    }
    (client, serde_json::from_str(&buf).unwrap())
}

#[tokio::test]
async fn tcp_rejects_request_before_handshake_and_closes_connection() {
    let (_server, addr, _dir, handle) = start_tcp_server("s3cr3t").await;

    let mut client = TcpStream::connect(addr).await.unwrap();
    // Skip the handshake — send a normal method call first.
    client
        .write_all(br#"{"jsonrpc":"2.0","method":"daemon.status","id":1}"#)
        .await
        .unwrap();
    client.write_all(b"\n").await.unwrap();

    let mut buf = String::new();
    {
        let (r, _) = client.split();
        let mut br = BufReader::new(r);
        br.read_line(&mut buf).await.unwrap();
    }
    let v: serde_json::Value = serde_json::from_str(&buf).unwrap();
    assert_eq!(v["error"]["code"], codes::UNAUTHORIZED, "resp: {v}");

    // The server closes the connection after rejecting the handshake — the
    // next read should see EOF (0 bytes), not a second response, proving
    // `daemon.status` itself was never dispatched.
    let mut trailing = String::new();
    let (r, _) = client.split();
    let mut br = BufReader::new(r);
    let n = br.read_line(&mut trailing).await.unwrap();
    assert_eq!(n, 0, "connection should be closed after auth rejection");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn tcp_rejects_wrong_token() {
    let (_server, addr, _dir, handle) = start_tcp_server("s3cr3t").await;
    let (_client, v) = tcp_handshake(addr, "wrong-token").await;
    assert_eq!(v["error"]["code"], codes::UNAUTHORIZED, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn tcp_accepts_correct_token_then_dispatches_normally() {
    let (_server, addr, _dir, handle) = start_tcp_server("s3cr3t").await;
    let (mut client, v) = tcp_handshake(addr, "s3cr3t").await;
    assert_eq!(v["result"]["authenticated"], true, "resp: {v}");

    // Same connection, now dispatches like any other request.
    client
        .write_all(br#"{"jsonrpc":"2.0","method":"daemon.status","id":2}"#)
        .await
        .unwrap();
    client.write_all(b"\n").await.unwrap();
    let mut buf = String::new();
    let (r, _) = client.split();
    let mut br = BufReader::new(r);
    br.read_line(&mut buf).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&buf).unwrap();
    assert!(v["result"]["version"].is_string(), "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Under `require_unix_socket_auth` the unix socket takes the TCP handshake:
/// nothing is served before `daemon.auth`, a wrong token is refused, and the
/// right one opens the connection for everything else.
#[tokio::test]
async fn the_unix_socket_can_be_made_to_require_the_handshake() {
    let (server, sock, _dir, handle) = start_server().await;
    server.set_tcp_token("s3cr3t".into()).await;
    server.require_unix_socket_auth();

    async fn exchange(sock: &std::path::Path, lines: &[&str]) -> Vec<serde_json::Value> {
        let mut client = UnixStream::connect(sock).await.unwrap();
        let (r, mut w) = client.split();
        let mut br = BufReader::new(r);
        let mut answers = Vec::new();
        for line in lines {
            if w.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                break;
            }
            let mut buf = String::new();
            if br.read_line(&mut buf).await.unwrap_or(0) == 0 {
                break;
            }
            answers.push(serde_json::from_str(&buf).unwrap());
        }
        answers
    }
    let status = r#"{"jsonrpc":"2.0","method":"daemon.status","id":2}"#;

    let refused = exchange(&sock, &[status, status]).await;
    assert_eq!(refused.len(), 1, "served after the refusal: {refused:?}");
    assert_eq!(refused[0]["error"]["code"], codes::UNAUTHORIZED);

    let wrong = exchange(
        &sock,
        &[
            r#"{"jsonrpc":"2.0","method":"daemon.auth","params":{"token":"nope"},"id":1}"#,
            status,
        ],
    )
    .await;
    assert_eq!(wrong.len(), 1, "{wrong:?}");
    assert_eq!(wrong[0]["error"]["code"], codes::UNAUTHORIZED);

    let admitted = exchange(
        &sock,
        &[
            r#"{"jsonrpc":"2.0","method":"daemon.auth","params":{"token":"s3cr3t"},"id":1}"#,
            status,
        ],
    )
    .await;
    assert_eq!(admitted[0]["result"]["authenticated"], true, "{admitted:?}");
    assert!(admitted[1]["result"]["version"].is_string(), "{admitted:?}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn serve_tcp_listener_bails_without_a_token_even_when_called_directly() {
    // `serve_tcp_listener` is `pub` specifically so tests (and any other
    // caller who already has a bound `TcpListener`, e.g. socket activation)
    // can drive it without going through `serve_tcp`'s addr-binding — but
    // that means it can't just trust `serve_tcp`'s "token is set" check to
    // have already run. This calls it directly, skipping `set_tcp_token`
    // entirely, and asserts it fails loudly instead of silently rejecting
    // every future connection forever.
    let dir = tempfile::tempdir().unwrap();
    let client: Arc<dyn AnthropicClient> =
        Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("test-key".into())).unwrap());
    let config = load_daemon_config(
        "claude-sonnet-4-6",
        None,
        None,
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(dir.path().to_path_buf()),
            model_client: Some(client),
            transcripts: daemon::Transcripts::Nowhere,
            host_policy: Some(Arc::new(AllowAllPermission)),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    // Deliberately NOT calling server.set_tcp_token(...).

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err = server.serve_tcp_listener(listener).await.unwrap_err();
    assert!(err.to_string().contains("token"), "err: {err}");
}
