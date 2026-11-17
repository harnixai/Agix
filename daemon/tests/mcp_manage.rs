//! MCP management over the wire: `mcp.addServer` / `updateServer` into the
//! tier named, `mcp.removeServer`, `mcp.setEnabled` across tiers and trust,
//! `mcp.reconnect` touching one server, and `mcp.status` of any pair — one
//! that has not connected is listed from its settings, nothing started.

use crate::support::*;
use daemon::rpc::codes;
use serde_json::{json, Value};
use std::path::Path;

fn read(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

fn bogus() -> Value {
    json!({"type": "stdio", "command": "definitely-not-a-real-binary-xyz"})
}

fn state_of(status: &Value, name: &str) -> Option<String> {
    status["servers"]
        .as_array()
        .unwrap_or_else(|| panic!("no servers: {status}"))
        .iter()
        .find(|s| s["name"] == name)
        .map(|s| s["state"].as_str().unwrap().to_string())
}

#[tokio::test]
async fn a_server_is_added_to_and_removed_from_the_global_tier() {
    let (server, sock, dir, handle) = start_server().await;
    let mut config = bogus();
    config["env"] = json!({"TOKEN": "secret-1234"});
    let added = call(
        &sock,
        "mcp.addServer",
        json!({"name": "fs", "tier": "global", "config": config}),
    )
    .await;
    let (shared, local) = (
        dir.path().join("settings.json"),
        dir.path().join("settings.local.json"),
    );
    assert_eq!(
        added["written_to"],
        json!(shared.display().to_string()),
        "{added}"
    );
    assert_eq!(
        added["secrets_written_to"],
        json!(local.display().to_string())
    );
    assert!(
        !dir.path().join(".agix/settings.json").exists(),
        "a global write landed in the project tier"
    );
    let tier = call(
        &sock,
        "config.get",
        json!({"scene": "code", "tier": "global"}),
    )
    .await;
    assert_eq!(
        tier["settings"]["mcp_servers"]["fs"]["command"], "definitely-not-a-real-binary-xyz",
        "{tier}"
    );
    assert!(tier["settings"]["mcp_servers"]["fs"].get("env").is_none());
    assert_eq!(
        read(&local)["mcp_servers"]["fs"]["env"]["TOKEN"],
        "secret-1234"
    );
    assert_eq!(state_of(&added, "fs").as_deref(), Some("failed"), "{added}");

    let removed = call(
        &sock,
        "mcp.removeServer",
        json!({"name": "fs", "tier": "global"}),
    )
    .await;
    assert!(
        read(&shared).get("mcp_servers").is_none(),
        "{}",
        read(&shared)
    );
    assert!(
        read(&local).get("mcp_servers").is_none(),
        "{}",
        read(&local)
    );
    assert_eq!(state_of(&removed, "fs"), None, "{removed}");
    let status = call(&sock, "mcp.status", json!({})).await;
    assert_eq!(state_of(&status, "fs"), None, "{status}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A token in a server's arguments or its URL's query is not in `headers`
/// or `env`, so it is written to `settings.json`: the answer warns, naming
/// the server and not the token.
#[tokio::test]
async fn a_token_in_a_servers_arguments_or_url_is_written_with_a_warning() {
    let (server, sock, _dir, handle) = start_server().await;
    let mut in_args = bogus();
    in_args["args"] = json!(["--token", "s3cr3t-in-args"]);
    let in_url = json!({"type": "streamable_http",
                        "url": "https://mcp.invalid/mcp?api_key=s3cr3t-in-url"});
    for (name, config) in [("argued", in_args), ("linked", in_url)] {
        let added = call(
            &sock,
            "mcp.addServer",
            json!({"name": name, "tier": "global", "config": config}),
        )
        .await;
        let warnings = added["warnings"].as_array().cloned().unwrap_or_default();
        assert_eq!(warnings.len(), 1, "{name}: {added}");
        assert!(
            warnings[0]
                .as_str()
                .unwrap()
                .contains(&format!("mcp_servers.{name}")),
            "{added}"
        );
        assert!(!added.to_string().contains("s3cr3t"), "{added}");
    }
    let mut clean = bogus();
    clean["env"] = json!({"TOKEN": "s3cr3t-in-env"});
    let added = call(
        &sock,
        "mcp.addServer",
        json!({"name": "clean", "tier": "global", "config": clean}),
    )
    .await;
    assert!(added.get("warnings").is_none(), "{added}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn a_server_turned_off_is_reported_disabled_until_turned_back_on() {
    let (server, sock, dir, handle) = start_server().await;
    call(
        &sock,
        "mcp.addServer",
        json!({"name": "gh", "tier": "global", "config": bogus()}),
    )
    .await;
    let off = call(
        &sock,
        "mcp.setEnabled",
        json!({"name": "gh", "tier": "global", "enabled": false}),
    )
    .await;
    assert_eq!(state_of(&off, "gh").as_deref(), Some("disabled"), "{off}");
    assert_eq!(
        read(&dir.path().join("settings.json"))["mcp_enabled"]["gh"],
        false
    );
    let status = call(&sock, "mcp.status", json!({})).await;
    assert_eq!(
        state_of(&status, "gh").as_deref(),
        Some("disabled"),
        "{status}"
    );
    assert_eq!(status["loaded"], true, "{status}");

    let updated = call(
        &sock,
        "mcp.updateServer",
        json!({"name": "gh", "tier": "global", "config": {
            "type": "stdio", "command": "another-missing-binary", "env": {"A": "b"}
        }}),
    )
    .await;
    assert_eq!(
        state_of(&updated, "gh").as_deref(),
        Some("disabled"),
        "an update turned the server back on: {updated}"
    );

    let on = call(
        &sock,
        "mcp.setEnabled",
        json!({"name": "gh", "tier": "global", "enabled": true}),
    )
    .await;
    assert_eq!(state_of(&on, "gh").as_deref(), Some("failed"), "{on}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn a_project_turns_off_a_global_server_for_itself_and_an_untrusted_one_only_off() {
    let (server, sock, dir, handle) = start_server().await;
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let untrusted = elsewhere.path().join("repo");
    std::fs::create_dir_all(&untrusted).unwrap();
    call(
        &sock,
        "mcp.addServer",
        json!({"name": "gh", "tier": "global", "config": bogus()}),
    )
    .await;

    call(
        &sock,
        "mcp.setEnabled",
        json!({"name": "gh", "tier": "project", "enabled": false}),
    )
    .await;
    let own = call(&sock, "mcp.status", json!({"project_root": dir.path()})).await;
    assert_eq!(state_of(&own, "gh").as_deref(), Some("disabled"), "{own}");
    let elsewhere_status = call(&sock, "mcp.status", json!({"project_root": other})).await;
    assert_eq!(elsewhere_status["loaded"], false, "{elsewhere_status}");
    assert_eq!(
        state_of(&elsewhere_status, "gh").as_deref(),
        Some("not_started"),
        "{elsewhere_status}"
    );

    let turned_off = call(
        &sock,
        "mcp.setEnabled",
        json!({"name": "gh", "tier": "project", "project_root": untrusted, "enabled": false}),
    )
    .await;
    assert_eq!(
        state_of(&turned_off, "gh").as_deref(),
        Some("disabled"),
        "{turned_off}"
    );
    let turned_on = call_frame(
        &sock,
        "mcp.setEnabled",
        json!({"name": "gh", "tier": "project", "project_root": untrusted, "enabled": true}),
    )
    .await;
    assert_eq!(
        turned_on["error"]["code"],
        codes::PROJECT_UNTRUSTED,
        "{turned_on}"
    );
    assert_eq!(
        read(&untrusted.join(".agix/settings.json"))["mcp_enabled"]["gh"],
        false
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn reconnect_needs_a_connected_pair_and_a_server_that_is_on() {
    let (server, sock, dir, handle) = start_server().await;
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    call(
        &sock,
        "mcp.addServer",
        json!({"name": "gh", "tier": "global", "config": bogus()}),
    )
    .await;

    let unconnected = call_frame(
        &sock,
        "mcp.reconnect",
        json!({"name": "gh", "project_root": other}),
    )
    .await;
    assert_eq!(
        unconnected["error"]["code"],
        codes::INVALID_PARAMS,
        "{unconnected}"
    );

    let again = call(&sock, "mcp.reconnect", json!({"name": "gh"})).await;
    assert_eq!(again["server"]["name"], "gh", "{again}");
    assert_eq!(again["server"]["state"], "failed", "{again}");

    call(
        &sock,
        "mcp.setEnabled",
        json!({"name": "gh", "tier": "global", "enabled": false}),
    )
    .await;
    let off = call_frame(&sock, "mcp.reconnect", json!({"name": "gh"})).await;
    assert_eq!(off["error"]["code"], codes::INVALID_PARAMS, "{off}");
    let unknown = call_frame(&sock, "mcp.reconnect", json!({"name": "nope"})).await;
    assert_eq!(unknown["error"]["code"], codes::INVALID_PARAMS, "{unknown}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// D1: asking about a project's servers runs none of them — not for a
/// project that is not trusted, nor for a trusted one that has not
/// connected.
#[tokio::test]
async fn the_status_of_a_pair_that_never_connected_starts_nothing() {
    let (server, sock, dir, handle) = start_server().await;
    let marker = dir.path().join("started.log");
    let declare = |root: &Path| {
        std::fs::create_dir_all(root.join(".agix")).unwrap();
        std::fs::write(
            root.join(".agix/settings.json"),
            json!({"mcp_servers": {"probe": {
                "type": "stdio", "command": "/bin/sh",
                "args": ["-c", format!("echo started >> '{}'", marker.display())]
            }}})
            .to_string(),
        )
        .unwrap();
    };
    let elsewhere = tempfile::tempdir().unwrap();
    let untrusted = elsewhere.path().join("repo");
    let trusted = dir.path().join("trusted");
    declare(&untrusted);
    declare(&trusted);

    let status = call(&sock, "mcp.status", json!({"project_root": untrusted})).await;
    assert_eq!(status["loaded"], false, "{status}");
    assert_eq!(
        state_of(&status, "probe").as_deref(),
        Some("untrusted"),
        "{status}"
    );
    let status = call(&sock, "mcp.status", json!({"project_root": trusted})).await;
    assert_eq!(status["loaded"], false, "{status}");
    assert_eq!(
        state_of(&status, "probe").as_deref(),
        Some("not_started"),
        "{status}"
    );
    assert!(!marker.exists(), "a status query started a server");

    let missing = call_frame(
        &sock,
        "mcp.status",
        json!({"project_root": "/nonexistent/agix-x"}),
    )
    .await;
    assert_eq!(
        missing["error"]["code"],
        codes::PROJECT_NOT_FOUND,
        "{missing}"
    );
    let scene = call_frame(&sock, "mcp.status", json!({"scene": "no-such-scene"})).await;
    assert_eq!(scene["error"]["code"], codes::SCENE_NOT_FOUND, "{scene}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Turning a server off, removing one or replacing one's configuration for
/// a pair that has not connected writes the file and starts nothing: the
/// pair's other servers wait for its first session, as `mcp.status` and
/// `mcp.reconnect` leave them.
#[tokio::test]
async fn writing_a_server_for_a_pair_that_never_connected_starts_nothing() {
    let (server, sock, dir, handle) = start_server().await;
    let marker = dir.path().join("started.log");
    let other = dir.path().join("other");
    std::fs::create_dir_all(other.join(".agix")).unwrap();
    std::fs::write(
        other.join(".agix/settings.json"),
        json!({"mcp_servers": {
            "probe": {
                "type": "stdio", "command": "/bin/sh",
                "args": ["-c", format!("echo started >> '{}'", marker.display())]
            },
            "old": bogus(),
            "spare": bogus()
        }})
        .to_string(),
    )
    .unwrap();
    call(
        &sock,
        "mcp.addServer",
        json!({"name": "gh", "tier": "global", "config": bogus()}),
    )
    .await;

    let off = call(
        &sock,
        "mcp.setEnabled",
        json!({"name": "gh", "project_root": other, "enabled": false}),
    )
    .await;
    assert_eq!(state_of(&off, "gh").as_deref(), Some("disabled"), "{off}");
    assert_eq!(
        state_of(&off, "probe").as_deref(),
        Some("not_started"),
        "{off}"
    );
    let removed = call(
        &sock,
        "mcp.removeServer",
        json!({"name": "old", "project_root": other}),
    )
    .await;
    assert_eq!(state_of(&removed, "old"), None, "{removed}");
    let updated = call(
        &sock,
        "mcp.updateServer",
        json!({"name": "spare", "project_root": other, "config": bogus()}),
    )
    .await;
    assert_eq!(
        state_of(&updated, "spare").as_deref(),
        Some("not_started"),
        "{updated}"
    );
    let status = call(&sock, "mcp.status", json!({"project_root": other})).await;
    assert_eq!(status["loaded"], false, "{status}");
    assert!(
        !marker.exists(),
        "a write for an unconnected pair started its servers"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn removing_a_server_the_tier_does_not_declare_is_refused_with_the_file() {
    let (server, sock, dir, handle) = start_server().await;
    call(
        &sock,
        "mcp.addServer",
        json!({"name": "gh", "tier": "global", "config": bogus()}),
    )
    .await;
    let refused = call_frame(
        &sock,
        "mcp.removeServer",
        json!({"name": "gh", "tier": "project"}),
    )
    .await;
    assert_eq!(refused["error"]["code"], codes::INVALID_PARAMS, "{refused}");
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&dir.path().join(".agix/settings.json").display().to_string()),
        "{message}"
    );
    assert!(read(&dir.path().join("settings.json"))["mcp_servers"]["gh"].is_object());

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// `mcp.reconnect` restarts the one server named: each wrapper counts its
/// own starts.
#[tokio::test]
#[ignore = "builds and spawns the toy MCP server"]
async fn reconnect_restarts_the_one_server_and_leaves_the_others() {
    let binary = crate::mcp_config::toy_mcp_server();
    let (server, sock, dir, handle) = start_server().await;
    let counted = |name: &str| {
        let marker = dir.path().join(format!("{name}.log"));
        let wrapper = dir.path().join(format!("{name}.sh"));
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\necho start >> '{}'\nexec '{}'\n",
                marker.display(),
                binary.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (wrapper, marker)
    };
    let (a, a_marker) = counted("a");
    let (b, b_marker) = counted("b");
    for (name, wrapper) in [("a", &a), ("b", &b)] {
        call(
            &sock,
            "mcp.addServer",
            json!({"name": name, "tier": "global", "config": {"type": "stdio", "command": wrapper}}),
        )
        .await;
    }
    let starts = |marker: &Path| {
        std::fs::read_to_string(marker)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let (a_before, b_before) = (starts(&a_marker), starts(&b_marker));
    assert!(a_before >= 1 && b_before >= 1, "{a_before} {b_before}");

    let again = call(&sock, "mcp.reconnect", json!({"name": "a"})).await;
    assert_eq!(again["server"]["state"], "connected", "{again}");
    assert_eq!(starts(&a_marker), a_before + 1);
    assert_eq!(
        starts(&b_marker),
        b_before,
        "reconnecting `a` restarted `b`"
    );
    let status = call(&sock, "mcp.status", json!({})).await;
    assert_eq!(
        state_of(&status, "b").as_deref(),
        Some("connected"),
        "{status}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}
