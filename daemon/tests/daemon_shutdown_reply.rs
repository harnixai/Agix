//! `daemon.shutdown` against the real `agixd` process: the caller gets its
//! answer before the process exits, not just an EOF.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn spawn_agixd(root: &Path) -> (tokio::process::Child, std::path::PathBuf) {
    let (home, project) = (root.join("home"), root.join("project"));
    std::fs::create_dir_all(home.join(".agix")).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let sock = root.join("d.sock");
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_agixd"))
        .arg("--socket")
        .arg(&sock)
        .arg("--instance")
        .arg("shutdown-reply")
        .current_dir(&project)
        .env("HOME", &home)
        .env("AGIX_CONFIG_HOME", home.join(".agix"))
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .env("ANTHROPIC_AUTH_TOKEN", "unused")
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

async fn connect(sock: &Path) -> UnixStream {
    for _ in 0..400 {
        if let Ok(conn) = UnixStream::connect(sock).await {
            return conn;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("agixd never listened on {}", sock.display());
}

#[tokio::test]
async fn daemon_shutdown_answers_before_the_process_exits() {
    for round in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let (mut child, sock) = spawn_agixd(dir.path()).await;
        let mut conn = connect(&sock).await;
        let request = json!({"jsonrpc":"2.0","method":"daemon.shutdown","params":{},"id":7});
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut reader = BufReader::new(conn);
        let mut answer = None;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                    if v["id"] == 7 {
                        answer = Some(v);
                        break;
                    }
                }
            }
        }
        let answer = answer.unwrap_or_else(|| {
            panic!("round {round}: the connection closed without an answer to daemon.shutdown")
        });
        assert_eq!(answer["result"]["shutting_down"], true, "{answer}");
        let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .expect("agixd exits after daemon.shutdown")
            .unwrap();
        assert!(status.success(), "agixd exited with {status}");
    }
}
