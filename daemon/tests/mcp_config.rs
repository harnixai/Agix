//! `mcp.*` methods: status, add and update server, secrets kept out of committed settings, connect failures.

use std::path::PathBuf;
use std::time::Duration;

use crate::support::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[tokio::test]
async fn mcp_status_empty_when_nothing_connected() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"mcp.status","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"]["servers"].as_array().unwrap().is_empty(),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn mcp_add_server_writes_settings_and_reports_connect_failure() {
    let (_server, sock, dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.addServer","params":{"name":"bogus","config":{"type":"stdio","command":"definitely-not-a-real-binary-xyz"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "resp: {v}");

    let written = std::fs::read_to_string(dir.path().join(".agix").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        written["mcp_servers"]["bogus"]["command"],
        "definitely-not-a-real-binary-xyz"
    );

    // The binary doesn't exist, so the connect attempt fails — status lists it
    // as failed, with why.
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"mcp.status","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let servers = v["result"]["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1, "resp: {v}");
    assert_eq!(servers[0]["name"], "bogus", "resp: {v}");
    assert_eq!(servers[0]["state"], "failed", "resp: {v}");
    assert!(
        servers[0]["error"].as_str().is_some_and(|e| !e.is_empty()),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// `mcp.updateServer` changes a server that is there and refuses one that is
/// not: a misspelt name must not quietly become a second server.
#[tokio::test]
async fn mcp_update_server_replaces_a_declared_server_and_refuses_an_unknown_one() {
    let (_server, sock, dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.updateServer","params":{"name":"bogus","config":{"type":"stdio","command":"x"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], -32602, "resp: {v}");

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.addServer","params":{"name":"bogus","config":{"type":"stdio","command":"first-missing-binary"}},"id":2}"#,
    )
    .await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.updateServer","params":{"name":"bogus","config":{"type":"stdio","command":"second-missing-binary"}},"id":3}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "resp: {v}");

    let written = std::fs::read_to_string(dir.path().join(".agix").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        written["mcp_servers"]["bogus"]["command"],
        "second-missing-binary"
    );
    assert_eq!(written["mcp_servers"].as_object().unwrap().len(), 1);

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A server's `headers` and `env` are credentials: they go to the
/// uncommitted `settings.local.json` (0600), the rest to `settings.json`. A
/// later write moves what an earlier version left in `settings.json`, and
/// drops what the replacement no longer has.
#[tokio::test]
async fn mcp_add_server_keeps_headers_and_env_out_of_the_committed_settings() {
    let (_server, sock, dir, handle) = start_server().await;
    let agix = dir.path().join(".agix");
    std::fs::create_dir_all(&agix).unwrap();
    std::fs::write(
        agix.join("settings.json"),
        r#"{"mcp_servers":{"svc":{"type":"stdio","command":"old-binary","env":{"TOKEN":"old-secret"}}}}"#,
    )
    .unwrap();
    let read = |file: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(agix.join(file)).unwrap()).unwrap()
    };

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.updateServer","params":{"name":"svc","config":{"type":"stdio","command":"missing-binary-xyz","env":{"TOKEN":"new-secret"}}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["result"]["secrets_written_to"],
        agix.join("settings.local.json").display().to_string(),
        "{v}"
    );
    let committed = read("settings.json");
    assert_eq!(
        committed["mcp_servers"]["svc"]["command"],
        "missing-binary-xyz"
    );
    assert!(
        committed["mcp_servers"]["svc"].get("env").is_none(),
        "the committed settings.json carries the env: {committed}"
    );
    assert_eq!(
        read("settings.local.json")["mcp_servers"]["svc"]["env"]["TOKEN"],
        "new-secret"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(agix.join("settings.local.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "settings.local.json is {mode:o}");
    }

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.addServer","params":{"name":"svc","config":{"type":"streamable_http","url":"http://127.0.0.1:9/mcp","headers":{"Authorization":"Bearer t"}}},"id":3}"#,
    )
    .await;
    let committed = read("settings.json");
    assert_eq!(
        committed["mcp_servers"]["svc"]["url"],
        "http://127.0.0.1:9/mcp"
    );
    assert!(
        committed["mcp_servers"]["svc"].get("headers").is_none(),
        "{committed}"
    );
    let local = read("settings.local.json");
    assert_eq!(
        local["mcp_servers"]["svc"],
        serde_json::json!({"headers": {"Authorization": "Bearer t"}}),
        "the replaced server's env stayed behind: {local}"
    );

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.addServer","params":{"name":"svc","config":{"type":"stdio","command":"plain-binary-xyz"}},"id":4}"#,
    )
    .await;
    let local = read("settings.local.json");
    assert!(local["mcp_servers"].get("svc").is_none(), "{local}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Where `cargo` put the toy MCP server, having built it.
///
/// Asked rather than reconstructed: a path built from `CARGO_MANIFEST_DIR`
/// guesses the profile and misses a `CARGO_TARGET_DIR` override, and this
/// test is meant to survive both.
fn toy_mcp_server() -> PathBuf {
    let out = std::process::Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "mcp-toy-server",
            "--message-format=json-render-diagnostics",
        ])
        .output()
        .expect("cargo build -p mcp-toy-server runs");
    assert!(
        out.status.success(),
        "cargo build -p mcp-toy-server failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v["reason"] == "compiler-artifact" && v["target"]["name"] == "mcp-toy-server")
        .and_then(|v| v["executable"].as_str().map(PathBuf::from))
        .expect("cargo reported an executable for mcp-toy-server")
}

/// The other half of `mcp.addServer`: a server that actually starts.
///
/// Every other MCP case here points the daemon at a binary that does not
/// exist. That covers the failure path twice over and leaves the one a user
/// takes — a server connects, reports its tools, and turns up in
/// `mcp.status` — resting on nothing. The `mcp_connected` notification is
/// checked here for the same reason: it is the half of
/// `daemon.subscribeEvents` a failing connect cannot show.
#[tokio::test]
#[ignore = "builds and spawns the toy MCP server"]
async fn mcp_add_server_connects_a_real_server_and_lists_its_tools() {
    let binary = toy_mcp_server();
    let (_server, sock, dir, handle) = start_server().await;

    let mut sub_client = UnixStream::connect(&sock).await.unwrap();
    sub_client
        .write_all(br#"{"jsonrpc":"2.0","method":"daemon.subscribeEvents","id":1}"#)
        .await
        .unwrap();
    sub_client.write_all(b"\n").await.unwrap();
    let (r, _w) = sub_client.split();
    let mut sub_reader = BufReader::new(r);
    let mut ack_line = String::new();
    sub_reader.read_line(&mut ack_line).await.unwrap();
    let ack: serde_json::Value = serde_json::from_str(&ack_line).unwrap();
    assert_eq!(ack["result"]["subscribed"], true, "ack: {ack}");

    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "mcp.addServer",
        "params": {
            "name": "toy",
            "config": { "type": "stdio", "command": binary.to_string_lossy() }
        }
    });
    let resp = rpc_call(&sock, &request.to_string()).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();

    let connected = |servers: &serde_json::Value| -> bool {
        servers.as_array().is_some_and(|list| {
            list.iter()
                .any(|s| s["name"] == "toy" && s["tool_count"].as_u64().unwrap_or(0) > 0)
        })
    };

    assert!(
        v["result"]["written_to"].is_string(),
        "addServer should report where it wrote: {v}"
    );
    assert!(
        connected(&v["result"]["servers"]),
        "addServer should report the server it just connected, with its tools: {v}"
    );

    let written = std::fs::read_to_string(dir.path().join(".agix").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        written["mcp_servers"]["toy"]["command"],
        binary.to_string_lossy().as_ref()
    );

    let mut event_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        sub_reader.read_line(&mut event_line),
    )
    .await
    .expect("timed out waiting for the mcp_connected notification")
    .unwrap();
    let event: serde_json::Value = serde_json::from_str(&event_line).unwrap();
    assert_eq!(event["method"], "daemon.event", "event: {event}");
    assert_eq!(event["params"]["kind"], "mcp_connected", "event: {event}");
    assert_eq!(event["params"]["server"], "toy", "event: {event}");

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"mcp.status","id":3}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        connected(&v["result"]["servers"]),
        "a connected server must still be there when asked separately: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn subscribe_events_receives_mcp_connect_failed_notification() {
    let (_server, sock, _dir, handle) = start_server().await;

    let mut sub_client = UnixStream::connect(&sock).await.unwrap();
    sub_client
        .write_all(br#"{"jsonrpc":"2.0","method":"daemon.subscribeEvents","id":1}"#)
        .await
        .unwrap();
    sub_client.write_all(b"\n").await.unwrap();
    let (r, _w) = sub_client.split();
    let mut sub_reader = BufReader::new(r);

    let mut ack_line = String::new();
    sub_reader.read_line(&mut ack_line).await.unwrap();
    let ack: serde_json::Value = serde_json::from_str(&ack_line).unwrap();
    assert_eq!(ack["result"]["subscribed"], true, "ack: {ack}");

    // Trigger a connect attempt (guaranteed to fail — nonexistent binary)
    // from a second, independent connection.
    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"mcp.addServer","params":{"name":"bogus","config":{"type":"stdio","command":"definitely-not-a-real-binary-xyz"}},"id":2}"#,
    )
    .await;

    let mut event_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        sub_reader.read_line(&mut event_line),
    )
    .await
    .expect("timed out waiting for daemon.event notification")
    .unwrap();
    let event: serde_json::Value = serde_json::from_str(&event_line).unwrap();
    assert_eq!(event["method"], "daemon.event", "event: {event}");
    assert_eq!(event["params"]["kind"], "mcp_connect_failed");
    assert_eq!(event["params"]["server"], "bogus");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn config_reload_attempts_to_connect_a_hand_edited_mcp_server() {
    // Uses the same "definitely not a real binary" pattern as
    // `mcp_add_server_writes_settings_and_reports_connect_failure` — the
    // point isn't a successful connection (that needs a real MCP server
    // process), it's proving `config.reload` attempts one and reports the
    // outcome in `mcp_failed`.
    let (_server, sock, dir, handle) = start_server().await;

    let settings_path = dir.path().join(".agix").join("settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "mcp_servers": {
                "hand-edited": {
                    "type": "stdio",
                    "command": "definitely-not-a-real-binary-xyz"
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.reload","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "expected success, got: {v}");
    let failed = v["result"]["routing"]["mcp_failed"]
        .as_array()
        .expect("mcp_failed should be an array");
    assert!(
        failed.iter().any(|s| s == "hand-edited"),
        "expected 'hand-edited' in mcp_failed, got: {v}"
    );
    assert!(v["result"]["routing"]["mcp_connected"]
        .as_array()
        .unwrap()
        .is_empty());

    // And `mcp.status` — which reads the same `self.mcp` this reconcile call
    // writes to — agrees: still nothing connected (the binary doesn't
    // exist), but the attempt happened, which is what actually matters here.
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"mcp.status","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let servers = v["result"]["servers"].as_array().unwrap();
    assert!(
        !servers.is_empty()
            && servers
                .iter()
                .all(|s| s["name"] == "hand-edited" && s["state"] == "failed"),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}
