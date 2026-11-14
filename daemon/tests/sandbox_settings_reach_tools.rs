//! What `settings.json`'s `sandbox` section says reaches the tools that act
//! on it: the hosts `WebFetch` may reach, what a sandboxed `Bash` command may
//! not read, and the one proxy it may connect through.

#![cfg(feature = "web")]

use crate::support::call;
use crate::support::{text_round, tool_round};
use std::sync::Arc;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// An HTTP server on 127.0.0.1 answering every request with `body`.
async fn stub_server(body: &'static str) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(reply.as_bytes()).await;
            });
        }
    });
    port
}

/// The `tool_result` blocks the model was sent for `id`.
fn answers(model: &MockAnthropicClient, id: &str) -> Vec<Value> {
    (0..model.calls())
        .filter_map(|n| model.nth_request(n))
        .flat_map(|r| {
            serde_json::to_value(&r).unwrap()["messages"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["type"] == "tool_result" && b["tool_use_id"] == id)
        .collect()
}

/// A forward proxy on 127.0.0.1 that takes an absolute-form `GET`, fetches
/// it and hands the answer back marked `X-Via: agix-test`.
async fn marking_proxy() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && conn.read(&mut byte).await.unwrap_or(0) == 1 {
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head);
                let Some(target) = head
                    .split_whitespace()
                    .nth(1)
                    .and_then(|t| t.strip_prefix("http://"))
                else {
                    return;
                };
                let (host, path) = target.split_once('/').unwrap_or((target, ""));
                let Ok(mut upstream) = tokio::net::TcpStream::connect(host).await else {
                    return;
                };
                let request =
                    format!("GET /{path} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\r\n");
                let _ = upstream.write_all(request.as_bytes()).await;
                let mut reply = Vec::new();
                let _ = upstream.read_to_end(&mut reply).await;
                let reply = String::from_utf8_lossy(&reply);
                let marked = reply.replacen("\r\n", "\r\nX-Via: agix-test\r\n", 1);
                let _ = conn.write_all(marked.as_bytes()).await;
            });
        }
    });
    port
}

struct Daemon {
    _dir: tempfile::TempDir,
    sock: std::path::PathBuf,
    model: Arc<MockAnthropicClient>,
}

/// A daemon over a global settings.json of `sandbox` with `Bash` allowed.
async fn daemon_with_sandbox(sandbox: Value) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let global = root.join("agix");
    let work = root.join("work");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        global.join("settings.json"),
        json!({
            "permission_rules": [{"tool": "Bash", "action": "allow"}],
            "sandbox": sandbox,
        })
        .to_string(),
    )
    .unwrap();
    let sock = root.join("agix.sock");
    let paths = StaticDaemonPaths::with_project(global.clone(), work.clone()).with_global(global);
    let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
    config.settings.values.memory_enabled = false;
    let model = Arc::new(MockAnthropicClient::new());
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(work),
            model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
            transcripts: daemon::Transcripts::Nowhere,
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let listener = DaemonServer::bind_unix(&sock).unwrap();
    tokio::spawn(async move {
        let _ = server.serve_unix_listener(listener).await;
    });
    Daemon {
        _dir: dir,
        sock,
        model,
    }
}

async fn doctor_check(sock: &std::path::Path, name: &str) -> Value {
    let doctor = call(sock, "daemon.doctor", json!({})).await;
    doctor["health"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("no {name} check: {doctor}"))
}

/// Under `network_mode: proxy` a command reaches a server on this machine
/// through the proxy its variables name, and not around it.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn egress_goes_only_through_the_proxy() {
    let direct = stub_server("DIRECT-BODY").await;
    let proxy = marking_proxy().await;
    let daemon = daemon_with_sandbox(json!({
        "network_mode": "proxy",
        "egress_proxy": format!("127.0.0.1:{proxy}"),
    }))
    .await;
    let url = format!("http://127.0.0.1:{direct}/");
    let rounds = [
        (
            "toolu_around",
            format!("curl -s --noproxy '*' -m 3 {url}; echo rc=$?"),
        ),
        (
            "toolu_through",
            format!("curl -s -i -m 3 {url}; echo rc=$?"),
        ),
        (
            "toolu_env",
            "echo \"[$http_proxy][$HTTPS_PROXY][${NO_PROXY-unset}]\"".to_string(),
        ),
    ];
    for (id, command) in &rounds {
        daemon
            .model
            .push_turn(tool_round(id, "Bash", json!({ "command": command })));
    }
    daemon.model.push_turn(text_round("done"));
    call(
        &daemon.sock,
        "session.run_turn",
        json!({"message": "fetch"}),
    )
    .await;
    let shown = |id: &str| {
        answers(&daemon.model, id)
            .first()
            .cloned()
            .unwrap_or_default()
            .to_string()
    };

    let check = doctor_check(&daemon.sock, "sandbox.egress_proxy").await;
    if !tools::bash::sandbox_status().starts_with("available") {
        assert_eq!(check["details"]["enforcement"], "none", "{check}");
        return;
    }
    assert_eq!(check["status"], "ok", "{check}");
    assert_eq!(check["details"]["enforcement"], "full", "{check}");

    let around = shown("toolu_around");
    assert!(around.contains("rc="), "the command did not run: {around}");
    assert!(
        !around.contains("DIRECT-BODY") && !around.contains("rc=0"),
        "the command reached 127.0.0.1:{direct} around the proxy: {around}"
    );
    let through = shown("toolu_through");
    assert!(
        through.contains("DIRECT-BODY") && through.contains("X-Via: agix-test"),
        "the command did not reach the server through the proxy: {through}"
    );
    let env = shown("toolu_env");
    let expected = format!("[http://127.0.0.1:{proxy}][http://127.0.0.1:{proxy}][]");
    assert!(env.contains(&expected), "{expected} not in {env}");
}

/// A proxy the sandbox can hold only in part says so: `daemon.doctor`
/// names what is not held.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn a_proxy_off_this_machine_is_reported_as_held_only_by_port() {
    let daemon = daemon_with_sandbox(json!({
        "network_mode": "proxy",
        "egress_proxy": {"http": "192.0.2.10:3128"},
    }))
    .await;
    let check = doctor_check(&daemon.sock, "sandbox.egress_proxy").await;
    if !tools::bash::sandbox_status().starts_with("available") {
        return;
    }
    assert_eq!(check["status"], "degraded", "{check}");
    let unmet = check["details"]["unmet"].to_string();
    assert!(
        unmet.contains("192.0.2.10:3128") && unmet.contains("only by port"),
        "{check}"
    );

    let missing = daemon_with_sandbox(json!({
        "network_mode": "proxy",
        "egress_proxy": "proxy.example:3128",
    }))
    .await;
    let check = doctor_check(&missing.sock, "sandbox.egress_proxy").await;
    assert_eq!(check["status"], "failing", "{check}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sandbox_section_reaches_the_tools() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let global = dir.join("agix");
    let work = dir.join("work");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let secret = work.join("secret.txt");
    std::fs::write(&secret, "SANDBOX-SECRET-7c2").unwrap();
    std::fs::write(
        global.join("settings.json"),
        json!({
            "permission_rules": [
                {"tool": "WebFetch", "action": "allow"},
                {"tool": "Bash", "action": "allow"},
            ],
            "sandbox": {
                "network_mode": "allowlist",
                "allowed_domains": ["127.0.0.1"],
                "deny_read": [secret],
            },
        })
        .to_string(),
    )
    .unwrap();
    let port = stub_server("STUB-BODY-reached").await;

    let sock = dir.join("agix.sock");
    let paths = StaticDaemonPaths::with_project(global.clone(), work.clone()).with_global(global);
    let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
    config.settings.values.memory_enabled = false;
    let model = Arc::new(MockAnthropicClient::new());
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(work.clone()),
            model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
            transcripts: daemon::Transcripts::Nowhere,
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let listener = DaemonServer::bind_unix(&sock).unwrap();
    tokio::spawn(async move {
        let _ = server.serve_unix_listener(listener).await;
    });

    model.push_turn(tool_round(
        "WebFetch",
        "toolu_listed",
        json!({"url": format!("http://127.0.0.1:{port}/")}),
    ));
    model.push_turn(tool_round(
        "WebFetch",
        "toolu_unlisted",
        json!({"url": format!("http://localhost:{port}/")}),
    ));
    model.push_turn(tool_round(
        "Bash",
        "toolu_cat",
        json!({"command": "cat secret.txt; echo end"}),
    ));
    model.push_turn(text_round("done"));
    call(&sock, "session.run_turn", json!({"message": "fetch"})).await;

    let listed = answers(&model, "toolu_listed")
        .first()
        .cloned()
        .unwrap_or_default();
    assert!(
        listed.to_string().contains("STUB-BODY-reached"),
        "the listed host was not reached: {listed}"
    );
    let unlisted = answers(&model, "toolu_unlisted")
        .first()
        .cloned()
        .unwrap_or_default();
    assert!(
        !unlisted.to_string().contains("STUB-BODY-reached"),
        "a host the allowlist does not name was reached: {unlisted}"
    );
    // A path `deny_read` names is a credential to the whole tool surface: the
    // permission layer refuses the command before it runs, so the sandbox is
    // the second line, not the first.
    let cat = answers(&model, "toolu_cat")
        .first()
        .cloned()
        .unwrap_or_default()
        .to_string();
    assert!(
        !cat.contains("SANDBOX-SECRET-7c2"),
        "a path `deny_read` names was readable to a command: {cat}"
    );
    assert!(
        cat.contains("end") || cat.to_lowercase().contains("denied"),
        "the command neither ran nor was refused: {cat}"
    );
}
