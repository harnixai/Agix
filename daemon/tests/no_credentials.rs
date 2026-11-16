//! A real `agixd` with no key anywhere: a turn on the default provider ends
//! with `no_credentials`, and nothing — the prompt included — reaches the
//! model endpoint.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};

async fn counting_endpoint() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buf = vec![0u8; 65536];
            let _ = sock.read(&mut buf).await;
            let body = r#"{"type":"error","error":{"type":"authentication_error","message":"x-api-key header is required"}}"#;
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (port, accepted)
}

async fn spawn_keyless_agixd(
    root: &Path,
    port: u16,
) -> (tokio::process::Child, std::path::PathBuf) {
    let (home, project) = (root.join("home"), root.join("project"));
    std::fs::create_dir_all(home.join(".agix")).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let sock = root.join("d.sock");
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_agixd"))
        .arg("--socket")
        .arg(&sock)
        .arg("--instance")
        .arg("no-credentials")
        .current_dir(&project)
        .env("HOME", &home)
        .env("AGIX_CONFIG_HOME", home.join(".agix"))
        .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("AGIX_DAEMON_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("agixd starts");
    (child, sock)
}

async fn call(sock: &Path, id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    let mut conn = None;
    for _ in 0..2400 {
        if let Ok(c) = UnixStream::connect(sock).await {
            conn = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut conn = conn.unwrap_or_else(|| panic!("agixd never listened on {}", sock.display()));
    let request = json!({"jsonrpc":"2.0","method":method,"params":params,"id":id});
    conn.write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut reader = BufReader::new(conn);
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await.unwrap();
        assert!(read > 0, "connection closed before {method} answered");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v["id"] == id {
            return v;
        }
    }
}

#[tokio::test]
async fn a_turn_without_any_key_sends_nothing_and_fails_as_no_credentials() {
    let (port, accepted) = counting_endpoint().await;
    let dir = tempfile::tempdir().unwrap();
    let (_child, sock) = spawn_keyless_agixd(dir.path(), port).await;

    let created = call(&sock, 1, "session.create", json!({})).await;
    let session = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();
    let failed = tokio::time::timeout(
        Duration::from_secs(60),
        call(
            &sock,
            2,
            "session.run_turn",
            json!({"session_id": session, "message": "a prompt that must stay here"}),
        ),
    )
    .await
    .expect("the turn never ended");

    assert_eq!(
        failed["error"]["data"]["kind"], "no_credentials",
        "{failed}"
    );
    assert_eq!(failed["error"]["data"]["retryable"], false, "{failed}");
    let message = failed["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("ANTHROPIC_API_KEY") && message.contains("default"),
        "the message should say which provider lacks what: {failed}"
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        0,
        "a request reached the model endpoint without credentials"
    );
}
