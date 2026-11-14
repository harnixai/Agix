//! Package list, enable, disable, uninstall and reload, and scenes a package owns.

use crate::support::*;

#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_lifecycle_install_list_disable_enable_uninstall() {
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

    let d = &v["result"]["disclosure"];
    assert_eq!(d["plugin"], "demo-plugin", "install must disclose: {v}");
    assert_eq!(d["version"], "1.0.0");
    assert_eq!(
        d["capabilities"].as_array().unwrap().len(),
        0,
        "this fixture asks for nothing: {d}"
    );

    // The plugin's own description reaches the model, so it is listed with
    // its provenance rather than left for the reader to infer.
    let visible = d["model_visible"].as_array().unwrap();
    assert!(
        visible
            .iter()
            .any(|v| v["origin"] == "plugin description" && v["text"] == "demo plugin"),
        "{d}"
    );

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let plugins = v["result"]["plugins"].as_array().unwrap();
    let demo = plugins.iter().find(|p| p["name"] == "demo-plugin").unwrap();
    assert_eq!(demo["enabled"], true);

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.disable","id":4,"params":{"name":"demo-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["enabled"], false, "resp: {v}");

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":6}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let plugins = v["result"]["plugins"].as_array().unwrap();
    let demo = plugins.iter().find(|p| p["name"] == "demo-plugin").unwrap();
    assert_eq!(
        demo["enabled"], false,
        "plugin.list still shows enabled: {demo:?}"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.enable","id":7,"params":{"name":"demo-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["enabled"], true, "resp: {v}");

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.uninstall","id":9,"params":{"name":"demo-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["success"], true, "resp: {v}");

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":10}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let plugins = v["result"]["plugins"].as_array().unwrap();
    assert!(
        !plugins.iter().any(|p| p["name"] == "demo-plugin"),
        "plugins: {plugins:?}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A build without the package layer must say so, not pretend. An empty
/// `plugin.list` would read as "nothing installed", which is a different fact
/// from "this binary cannot load plugins at all".
#[cfg(not(feature = "plugin-packages"))]
#[tokio::test]
async fn plugin_rpcs_report_plugins_disabled_when_compiled_out() {
    let (_server, sock, _dir, handle) = start_server().await;

    for (method, params) in [
        ("plugin.list", "{}"),
        (
            "plugin.install",
            r#"{"name":"x","version":"1.0.0","download_url":"file:///dev/null"}"#,
        ),
        ("plugin.uninstall", r#"{"name":"x"}"#),
        ("plugin.enable", r#"{"name":"x"}"#),
        ("plugin.disable", r#"{"name":"x"}"#),
        ("plugin.reload", "{}"),
    ] {
        let resp = rpc_call(
            &sock,
            &format!(r#"{{"jsonrpc":"2.0","method":"{method}","id":1,"params":{params}}}"#),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(
            v["error"]["code"],
            codes::PLUGINS_DISABLED,
            "{method} should report the subsystem as unavailable: {v}"
        );
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("compiled-out"),
            "{method} should say why: {v}"
        );
    }

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Install a plugin that owns a scene directly into the global tier's
/// versioned cache — the layout `packages::discovery` reads.
#[cfg(feature = "plugin-packages")]
fn install_scene_plugin(root: &std::path::Path, name: &str) {
    let dir = root.join("plugins").join("cache").join(name).join("1.0.0");
    std::fs::create_dir_all(dir.join("scene")).unwrap();
    std::fs::write(
        dir.join("scene/prompt.md"),
        "You are the demo workflow agent.",
    )
    .unwrap();
    std::fs::write(
        dir.join("scene/scene.json"),
        format!(
            r#"{{"id": "plugin:{name}", "name": "Demo workflow", "description": "A plugin-owned scene",
                "tools": ["Read", "Grep"], "disallowed_tools": ["Bash"]}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            r#"
[plugin]
name = "{name}"
version = "1.0.0"
api_version = "0.1"
description = "owns a scene"

[scene]
dir = "scene"
"#
        ),
    )
    .unwrap();
}

/// A plugin that owns a scene needs no `scene.activate`: installing and
/// enabling it is the consent, and entering the scene is still an explicit
/// `session.create`.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn a_plugin_owned_scene_is_listed_and_can_host_a_session() {
    let dir = tempfile::tempdir().unwrap();
    install_scene_plugin(dir.path(), "demo-scene-plugin");

    let (server, sock, _dir, handle) = start_server_in(dir).await;

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"scene.list","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let scenes = v["result"]["scenes"].as_array().expect("scenes array");
    let demo = scenes
        .iter()
        .find(|s| s["scene"] == "plugin:demo-scene-plugin")
        .unwrap_or_else(|| panic!("plugin scene missing from {scenes:?}"));
    assert_eq!(demo["name"], "Demo workflow");
    assert_eq!(
        demo["active"], true,
        "an installed, enabled plugin's scene is servable without a second step"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":2,"params":{"scene":"plugin:demo-scene-plugin"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"]["session_id"].is_string(),
        "a session should be creatable in a plugin scene: {v}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A scene id no plugin owns must still be refused, so a typo does not
/// silently land the user in the default scene.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn an_unknown_plugin_scene_is_refused() {
    let (server, sock, _dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":1,"params":{"scene":"plugin:not-installed"}}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["error"].is_object(), "expected a refusal, got {v}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A plugin that appeared on disk without going through `plugin.install` —
/// dropped in by hand, or rebuilt in place during development — is picked up
/// by `plugin.reload`.
///
/// Asserted through the plugin's *scene*, not through `plugin.list`:
/// `plugin.list` rescans the directories on every call, so it would report a
/// hand-placed plugin with or without a reload and prove nothing. What needs
/// the reload is everything derived from the installed set — scenes, agent
/// types, commands.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_reload_picks_up_a_plugin_that_appeared_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (_server, sock, _dir, handle) = start_server_in(dir).await;

    let scene_named = |v: &serde_json::Value| -> bool {
        v["result"]["scenes"].as_array().is_some_and(|scenes| {
            scenes
                .iter()
                .any(|s| s["scene"] == "plugin:hand-placed-plugin")
        })
    };

    // Straight onto disk, behind the daemon's back.
    install_scene_plugin(&root, "hand-placed-plugin");

    let before = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"scene.list","id":1}"#).await;
    let before: serde_json::Value = serde_json::from_str(&before).unwrap();
    assert!(
        !scene_named(&before),
        "the daemon adopted a plugin nobody told it about — this test cannot show the reload \
         does anything: {before}"
    );

    let reloaded = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"plugin.reload","id":2}"#,
    )
    .await;
    let reloaded: serde_json::Value = serde_json::from_str(&reloaded).unwrap();
    assert!(
        reloaded["result"]["plugins"]
            .as_array()
            .is_some_and(|p| p.iter().any(|x| x["name"] == "hand-placed-plugin")),
        "reload should report the installed set it found: {reloaded}"
    );

    let after = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"scene.list","id":3}"#).await;
    let after: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert!(
        scene_named(&after),
        "the rescan did not take effect — the plugin's scene is still missing: {after}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}
