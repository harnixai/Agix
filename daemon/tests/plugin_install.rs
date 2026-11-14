//! Installing packages: checksum, disclosure limits, refused names, and what install leaves enabled.

use std::path::PathBuf;

use crate::support::*;
use daemon::rpc::codes;
#[cfg(feature = "plugin-packages")]
use sha2::{Digest, Sha256};

/// A package whose whole content is a script.
#[cfg(feature = "plugin-packages")]
fn build_script_plugin_zip(dir: &std::path::Path, plugin_name: &str) -> (PathBuf, String) {
    let mut buf = Vec::new();
    {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        writer.start_file("plugin.toml", opts).unwrap();
        write!(
            writer,
            "[plugin]\nname = \"{plugin_name}\"\nversion = \"1.0.0\"\n\
             api_version = \"0.1\"\ndescription = \"annotates tool results\"\n\n\
             [[script]]\npoint = \"tool.result\"\nentry = \"annotate.js:onResult\"\n"
        )
        .unwrap();
        writer.start_file("annotate.js", opts).unwrap();
        write!(writer, "function onResult(r) {{ return r.text }}").unwrap();
        writer.finish().unwrap();
    }
    let archive_path = dir.join(format!("{plugin_name}.zip"));
    std::fs::write(&archive_path, &buf).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&buf);
    (archive_path, hex::encode(hasher.finalize()))
}

/// A package that only ships a script installs in the default build.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn a_script_only_package_installs_and_discloses_what_it_will_run() {
    let (_server, sock, dir, handle) = start_server().await;
    let (archive_path, checksum) = build_script_plugin_zip(dir.path(), "annotator");

    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"annotator","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
            archive_path.display(),
            checksum,
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["success"], true, "install resp: {v}");

    // The script is a capability, not an implementation detail: it runs in
    // this process at a point that rewrites what the model reads, and
    // nothing isolates it from the host. Whoever installs it has to see
    // it before they rely on it.
    let d = &v["result"]["disclosure"];
    let caps = d["capabilities"].as_array().unwrap();
    assert!(
        caps.iter()
            .any(|c| c.as_str().unwrap().contains("`tool.result`")),
        "the script binding must be disclosed: {d}"
    );

    // `root` so a host can read what it cares about out of the package
    // without rebuilding the daemon's disk layout for itself.
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let plugins = v["result"]["plugins"].as_array().unwrap();
    let it = plugins.iter().find(|p| p["name"] == "annotator").unwrap();
    assert_eq!(it["enabled"], true);
    let root = std::path::Path::new(it["root"].as_str().expect("list carries the root: {it}"));
    assert!(
        root.join("annotate.js").is_file(),
        "the root must be the unpacked directory: {root:?}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_install_rejects_bad_checksum() {
    let (_server, sock, dir, handle) = start_server().await;
    let (archive_path, _correct_checksum) = build_demo_plugin_zip(dir.path(), "demo-plugin");

    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"demo-plugin","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
            archive_path.display(),
            "0".repeat(64),
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("checksum"),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Installing and being found are two different pieces of code, and only one
/// of them reads the manifest. A package that unpacks but will not load is
/// installed in no sense the caller cares about: it never appears in
/// `plugin.list` and contributes nothing, so install has to report the
/// failure on the way in.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_install_refuses_a_package_it_cannot_load_back() {
    let (_server, sock, dir, handle) = start_server().await;
    // No `api_version`, which is a parse error rather than a default.
    let (archive_path, checksum) = build_plugin_zip(
        dir.path(),
        "unloadable",
        "[plugin]\nname = \"unloadable\"\nversion = \"1.0.0\"\ndescription = \"no api_version\"\n",
    );

    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"unloadable","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
            archive_path.display(),
            checksum,
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("api_version"),
        "the refusal does not say what was wrong with the package: {v}"
    );

    // And it is gone, not left unpacked where a later `plugin.list` would
    // keep skipping it in silence.
    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let names: Vec<&str> = v["result"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"unloadable"),
        "the refused package was left behind: {names:?}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_rpcs_refuse_names_that_leave_the_cache() {
    let (_server, sock, dir, handle) = start_server().await;
    std::fs::create_dir_all(dir.path().join("keep").join("1.0.0")).unwrap();
    std::fs::create_dir_all(dir.path().join("plugins").join("keep")).unwrap();
    let (archive_path, checksum) = build_demo_plugin_zip(dir.path(), "demo-plugin");

    let calls = [
        r#"{"name":"../..","scope":"global"}"#.to_string(),
        r#"{"name":"..","version":"keep"}"#.to_string(),
        r#"{"name":"demo-plugin","version":"../../../keep"}"#.to_string(),
        format!(r#"{{"name":"{}"}}"#, dir.path().join("keep").display()),
    ];
    for params in &calls {
        let resp = rpc_call(
            &sock,
            &format!(r#"{{"jsonrpc":"2.0","method":"plugin.uninstall","id":1,"params":{params}}}"#),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "{params}: {v}");
    }
    for (name, version) in [("../../escaped", "1.0.0"), ("demo-plugin", "../../escaped")] {
        let resp = rpc_call(
            &sock,
            &format!(
                r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"{name}","version":"{version}","download_url":"file://{}","checksum":"{checksum}"}}}}"#,
                archive_path.display(),
            ),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(
            v["error"]["code"],
            codes::INVALID_PARAMS,
            "{name} {version}: {v}"
        );
    }
    assert!(dir.path().join("keep").join("1.0.0").is_dir());
    assert!(dir.path().join("plugins").join("keep").is_dir());
    assert!(!dir.path().join("escaped").exists());

    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"other-name","version":"1.0.0","download_url":"file://{}","checksum":"{checksum}"}}}}"#,
            archive_path.display(),
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("demo-plugin"),
        "a package installed under another name is refused: {v}"
    );
    assert!(!dir
        .path()
        .join("plugins")
        .join("cache")
        .join("other-name")
        .exists());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// The disclosure limits are a refusal: a package whose description is over
/// them does not install, and nothing of it is left in the cache.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn an_install_whose_disclosure_is_over_the_limit_is_refused_and_removed() {
    let (_server, sock, dir, handle) = start_server().await;
    let (archive_path, checksum) = build_plugin_zip(
        dir.path(),
        "wordy",
        &format!(
            "[plugin]\nname = \"wordy\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\
             description = \"{}\"\n",
            "x".repeat(501)
        ),
    );
    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"wordy","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
            archive_path.display(),
            checksum,
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("reaches the model"),
        "the refusal names the limit: {v}"
    );
    assert!(!dir
        .path()
        .join("plugins")
        .join("cache")
        .join("wordy")
        .join("1.0.0")
        .exists());

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        !v["result"]["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "wordy"),
        "{v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[cfg(feature = "plugin-packages")]
fn build_scene_plugin_zip(dir: &std::path::Path, name: &str) -> (PathBuf, String) {
    use std::io::Write;
    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        let files = [
            (
                "plugin.toml",
                format!(
                    "[plugin]\nname = \"{name}\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\n[scene]\ndir = \"scene\"\n"
                ),
            ),
            ("scene/prompt.md", "You are the demo workflow agent.".into()),
            (
                "scene/scene.json",
                format!(
                    r#"{{"id": "plugin:{name}", "name": "Demo", "description": "d", "tools": ["Read"]}}"#
                ),
            ),
        ];
        for (path, content) in files {
            writer.start_file(path, opts).unwrap();
            writer.write_all(content.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }
    let archive_path = dir.join(format!("{name}.zip"));
    std::fs::write(&archive_path, &buf).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&buf);
    (archive_path, hex::encode(hasher.finalize()))
}

/// With `plugins.enable_on_install` off, `plugin.install` reports
/// `enabled: false` and the package contributes nothing (here: its scene
/// cannot host a session) until the host calls `plugin.enable`.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_install_leaves_a_package_off_when_enable_on_install_is_false() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.json"),
        r#"{"plugins": {"enable_on_install": false}}"#,
    )
    .unwrap();
    let (archive_path, checksum) = build_scene_plugin_zip(dir.path(), "held-scene");
    let (server, sock, _dir, handle) = start_server_in(dir).await;

    let call = |body: String| {
        let sock = sock.clone();
        async move {
            let resp = rpc_call(&sock, &body).await;
            serde_json::from_str::<serde_json::Value>(&resp).unwrap()
        }
    };
    let session_in_scene = || {
        call(
            r#"{"jsonrpc":"2.0","method":"session.create","id":9,"params":{"scene":"plugin:held-scene"}}"#
                .to_string(),
        )
    };

    let v = call(format!(
        r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"held-scene","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
        archive_path.display(),
        checksum,
    ))
    .await;
    assert_eq!(v["result"]["success"], true, "{v}");
    assert_eq!(v["result"]["enabled"], false, "{v}");
    assert!(
        v["result"]["disclosure"]["scene"].is_string(),
        "the disclosure is still returned: {v}"
    );
    assert!(
        session_in_scene().await["error"].is_object(),
        "a package that is not enabled served a session"
    );

    let listed = call(r#"{"jsonrpc":"2.0","method":"plugin.list","id":2}"#.to_string()).await;
    let entry = listed["result"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "held-scene")
        .unwrap_or_else(|| panic!("installed but not listed: {listed}"))
        .clone();
    assert_eq!(entry["enabled"], false);

    let v = call(
        r#"{"jsonrpc":"2.0","method":"plugin.enable","id":3,"params":{"name":"held-scene"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(v["result"]["enabled"], true, "{v}");
    assert!(
        session_in_scene().await["result"]["session_id"].is_string(),
        "enabling the package did not make its scene servable"
    );

    let doctor = call(r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":4}"#.to_string()).await;
    assert!(
        doctor["result"]["health"]
            .to_string()
            .contains("plugins.enable_on_install"),
        "doctor does not report the setting: {doctor}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// Installing is the default and still enables: the result says so, and the
/// doctor announces that the default is going to change.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn plugin_install_enables_by_default_and_doctor_announces_the_coming_change() {
    let (server, sock, dir, handle) = start_server().await;
    let (archive_path, checksum) = build_scene_plugin_zip(dir.path(), "eager-scene");
    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"plugin.install","id":1,"params":{{"name":"eager-scene","version":"1.0.0","download_url":"file://{}","checksum":"{}"}}}}"#,
            archive_path.display(),
            checksum,
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["enabled"], true, "{v}");

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":2}"#,
    )
    .await;
    assert!(
        resp.contains("planned to default to false"),
        "doctor is silent about the coming default: {resp}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A package that reached the cache without `plugin.install` and is over the
/// disclosure limits is not loaded, and `daemon.doctor` names it.
#[cfg(feature = "plugin-packages")]
#[tokio::test]
async fn an_oversized_hand_placed_package_is_not_loaded_and_doctor_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("plugins/cache/wordy/1.0.0");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("plugin.toml"),
        format!(
            "[plugin]\nname = \"wordy\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\ndescription = \"{}\"\n",
            "x".repeat(600)
        ),
    )
    .unwrap();
    let (server, sock, _dir, handle) = start_server_in(dir).await;

    let resp = rpc_call(&sock, r#"{"jsonrpc":"2.0","method":"plugin.list","id":1}"#).await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["result"]["plugins"].as_array().unwrap().is_empty(),
        "an over-limit package was listed as loadable: {v}"
    );
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":2}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let health = v["result"]["health"].to_string();
    assert!(
        health.contains("plugins.refused") && health.contains("wordy"),
        "doctor does not name the refused package: {health}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}
