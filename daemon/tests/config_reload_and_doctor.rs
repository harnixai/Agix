//! `daemon.doctor` checks and `config.reload`: what a hand-edited settings file changes without a restart.

use std::sync::Arc;

use crate::support::*;
use daemon::rpc::codes;

#[tokio::test]
async fn doctor_lists_the_safety_posture_checks() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let names: Vec<&str> = v["result"]["health"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    for expected in [
        "settings.permissions.mode",
        "settings.hooks_fail_open",
        "build.panic",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// An unattended scene with the network open and the network policy not
/// required: doctor says so, and says the default is planned to change.
#[tokio::test]
async fn doctor_warns_when_unattended_commands_may_reach_an_open_network() {
    use scene::code::{spec, Interaction};

    async fn check(
        scene: base::interface::scene::SceneSpec,
        settings: Option<&str>,
    ) -> serde_json::Value {
        let dir = tempfile::tempdir().unwrap();
        if let Some(settings) = settings {
            std::fs::write(dir.path().join("settings.json"), settings).unwrap();
        }
        let (server, sock, _dir, handle) = start_server_serving(dir, Arc::new(scene)).await;
        let resp = rpc_call(
            &sock,
            r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":1}"#,
        )
        .await;
        server.shutdown_token().cancel();
        let _ = handle.await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        v["result"]["health"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "sandbox.unattended_network")
            .cloned()
            .unwrap_or_else(|| panic!("no sandbox.unattended_network check: {v}"))
    }

    let warned = check(spec(Interaction::Unattended), None).await;
    assert_eq!(warned["status"], "degraded", "{warned}");
    let summary = warned["summary"].as_str().unwrap();
    assert!(summary.contains("later 0.3.x"), "{summary}");
    assert_eq!(
        warned["details"]["unattended_scenes"][0], "code",
        "{warned}"
    );

    let attended = check(spec(Interaction::Interactive), None).await;
    assert_eq!(attended["status"], "ok", "{attended}");
    for settings in [
        r#"{"sandbox": {"network": {"mode": "deny_all"}}}"#,
        r#"{"sandbox": {"unattended_needs_network_policy": true}}"#,
    ] {
        let held = check(spec(Interaction::Unattended), Some(settings)).await;
        assert_eq!(held["status"], "ok", "{settings}: {held}");
    }
}

#[tokio::test]
async fn doctor_reports_no_providers_configured_as_ok() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["providers"]["ok"], true, "resp: {v}");
    assert_eq!(
        v["result"]["session_persistence"]["history_store_wired"],
        false
    );
    assert!(v["result"]["settings_tiers"].as_array().unwrap().len() == 3);

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// `enabled.json` is the only off switch a package has. Damaged, it keeps
/// everything in its tier off, refuses to be overwritten (which would turn
/// back on whatever it had turned off), and `daemon.doctor` says why.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn a_damaged_enable_state_keeps_plugins_off_and_doctor_says_so() {
    let (_server, sock, dir, handle) = start_server().await;
    let (archive_path, checksum) = build_demo_plugin_zip(dir.path(), "demo-plugin");
    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"demo-plugin","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
            archive_path.display(),
            checksum,
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["success"], true, "install resp: {v}");

    let enabled_json = dir.path().join("plugins").join("enabled.json");
    std::fs::write(&enabled_json, r#"{"demo-plugin": fal"#).unwrap();

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let demo = v["result"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "demo-plugin")
        .cloned()
        .unwrap();
    assert_eq!(demo["enabled"], false, "{demo}");

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.enable","id":3,"params":{"name":"demo-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["error"].is_object(), "resp: {v}");
    assert_eq!(
        std::fs::read_to_string(&enabled_json).unwrap(),
        r#"{"demo-plugin": fal"#
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":4}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let check = v["result"]["health"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "plugins.enable_state")
        .cloned()
        .unwrap_or_else(|| panic!("doctor has no enable-state check: {v}"));
    assert_eq!(check["status"], "degraded", "{check}");
    assert_eq!(check["details"]["tiers"][0]["tier"], "global", "{check}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn config_reload_picks_up_a_hand_edited_settings_file() {
    // The whole point of `config.reload` (vs `config.setProvider`) is
    // supporting a human editing settings.json directly, outside any RPC —
    // this writes the project-tier file by hand (no `config.setProvider`
    // call at all) and confirms `config.reload` alone picks it up.
    let (_server, sock, dir, handle) = start_server().await;

    let settings_path = dir.path().join(".agix").join("settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    std::fs::write(
        &settings_path,
        serde_json::json!({
            "providers": {
                "anthropic": {
                    "api_type": "anthropic",
                    "api_key": "sk-ant-hand-edited",
                    "default_model": "claude-sonnet-4-6"
                }
            },
            "default_provider": "anthropic"
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
    assert_eq!(v["result"]["default_provider"], "anthropic", "resp: {v}");
    assert_eq!(v["result"]["routing"]["ok"], true, "resp: {v}");
    assert_eq!(v["result"]["routing"]["router_rebuilt"], true, "resp: {v}");
    assert_eq!(
        v["result"]["routing"]["router_error"],
        serde_json::Value::Null,
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn existing_session_survives_a_config_reload_between_turns() {
    // Smoke test for the lazy-recreate path in `run_turn`: a session created
    // before a reload must still be usable (same session_id, no
    // SESSION_NOT_FOUND / crash) for a turn sent after the reload bumped
    // its binding going stale — proving the stale-binding branch in
    // `run_turn` (recreate via `resume_or_create`) doesn't break the normal
    // "session already exists" path. Uses `start_server_with_history()`
    // because the recreate path only engages for HistoryStore-backed
    // sessions (see `run_turn`'s doc comment).
    let (_server, sock, _dir, handle) = start_server_with_history(8).await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"hello"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"].is_object() || v["error"].is_object(),
        "resp: {v}"
    );
    let session_id = {
        let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"session.list","id":2}"#).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        let sessions = v["result"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "resp: {v}");
        sessions[0]["session_id"].as_str().unwrap().to_string()
    };

    // Invalidate every binding without touching this session directly.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.reload","id":3}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "reload should succeed: {v}");

    // Same session_id, second turn — must not come back SESSION_NOT_FOUND.
    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{session_id}","message":"still there?"}},"id":4}}"#
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_ne!(v["error"]["code"], codes::SESSION_NOT_FOUND, "resp: {v}");
    assert!(
        v["result"].is_object() || v["error"].is_object(),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn recreating_a_stale_session_at_capacity_does_not_evict_an_unrelated_session() {
    // The config-generation recreate path replaces an *existing* map entry
    // under the same sid rather than adding one, so at capacity it must not
    // evict an unrelated session.
    let (_server, sock, _dir, handle) = start_server_with_history(2).await;

    async fn session_ids(sock: &std::path::Path) -> std::collections::HashSet<String> {
        let resp = rpc_call(sock, r#"{"jsonrpc":"2.0","method":"session.list","id":99}"#).await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        v["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_id"].as_str().unwrap().to_string())
            .collect()
    }

    // Create the two sessions one at a time, diffing `session.list` after
    // each — `self.sessions` is a `HashMap`, so its iteration order carries
    // no creation-order information; this is the only reliable way to know
    // which of the two is objectively older (`id_older`, created first,
    // never touched again) vs the one we're about to recreate (`id_newer`).
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"hi 0"},"id":0}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"].is_object() || v["error"].is_object(),
        "resp: {v}"
    );
    let after_first = session_ids(&sock).await;
    assert_eq!(after_first.len(), 1, "ids: {after_first:?}");
    let id_older = after_first.into_iter().next().unwrap();

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"hi 1"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"].is_object() || v["error"].is_object(),
        "resp: {v}"
    );
    let after_second = session_ids(&sock).await;
    assert_eq!(after_second.len(), 2, "ids: {after_second:?}");
    let id_newer = after_second
        .into_iter()
        .find(|id| id != &id_older)
        .expect("second session must have a different id than the first");

    // Invalidate every binding, then send a second turn to the *newer* session
    // — this triggers the stale-binding recreate path for it while the
    // pool is already at capacity (2/2). If eviction incorrectly treats this
    // replace-in-place as "adding a new session", it'll evict whatever's
    // globally least-recently-active — `id_older`, which this call never
    // touches.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.reload","id":11}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "reload should succeed: {v}");

    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{id_newer}","message":"again"}},"id":12}}"#
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"].is_object() || v["error"].is_object(),
        "resp: {v}"
    );

    let final_ids = session_ids(&sock).await;
    assert_eq!(final_ids.len(), 2, "ids: {final_ids:?}");
    assert!(
        final_ids.contains(&id_older),
        "unrelated older session was evicted as a side effect of recreating the newer one: {final_ids:?}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// `daemon.doctor` names the key a settings value was left out at, even when
/// it is the only one in its file.
#[tokio::test]
async fn doctor_names_the_key_of_a_value_left_out_of_the_settings() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".agix")).unwrap();
    std::fs::write(
        dir.path().join(".agix/settings.json"),
        r#"{"sandbox": {"network": {"mode": "open"}}}"#,
    )
    .unwrap();
    let (server, sock, _dir, handle) = start_server_in(dir).await;
    let doctor = call(&sock, "daemon.doctor", serde_json::json!({})).await;
    let tiers = doctor["health"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "settings.tiers")
        .unwrap_or_else(|| panic!("no settings.tiers check: {doctor}"))["details"]
        .clone();
    let project = tiers
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tier"] == "project")
        .unwrap()
        .clone();
    let keys: Vec<&str> = project["ignored_values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["sandbox.network.mode"], "{project}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}
