//! `config.setProvider` / `config.getProvider` and the model router they rebuild.

use crate::support::*;
use daemon::rpc::codes;

#[tokio::test]
async fn set_provider_writes_project_settings_and_doctor_sees_it() {
    let (_server, sock, dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_type":"openai_compatible","base_url":"https://api.deepseek.com/v1","api_key":"k","default_model":"deepseek-pro"},"default_provider":"deepseek"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "expected success, got: {v}");
    assert_eq!(v["result"]["routing"]["ok"], true, "resp: {v}");
    assert_eq!(v["result"]["default_provider"], "deepseek");

    // Written to disk at the project tier, the key to the machine-local file.
    let agix = dir.path().join(".agix");
    let written = std::fs::read_to_string(agix.join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(
        written["providers"]["deepseek"]["default_model"],
        "deepseek-pro"
    );
    assert!(
        written["providers"]["deepseek"].get("api_key").is_none(),
        "the committed settings.json carries the key: {written}"
    );
    let local = std::fs::read_to_string(agix.join("settings.local.json")).unwrap();
    let local: serde_json::Value = serde_json::from_str(&local).unwrap();
    assert_eq!(local["providers"]["deepseek"]["api_key"], "k");
    assert_eq!(
        v["result"]["secrets_written_to"],
        agix.join("settings.local.json").display().to_string()
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
    let ignored = std::fs::read_to_string(agix.join(".gitignore")).unwrap_or_default();
    assert!(
        ignored.lines().any(|l| l == "settings.local.json"),
        "the key can be committed: .agix/.gitignore is {ignored:?}"
    );

    // Doctor now reflects the newly-written provider.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"daemon.doctor","id":2}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["providers"]["default_provider"], "deepseek");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_can_target_another_project_this_daemon_serves() {
    let (_server, sock, dir, handle) = start_server().await;

    let other = dir.path().join("other-project");
    std::fs::create_dir_all(&other).unwrap();

    let resp = rpc_call(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"config.setProvider","params":{{"project_root":"{}","provider_id":"deepseek","config":{{"api_type":"openai_compatible","base_url":"https://api.deepseek.com/v1","api_key":"k","default_model":"deepseek-pro"}},"default_provider":"deepseek"}},"id":1}}"#,
            other.display()
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "expected success, got: {v}");
    assert_eq!(
        v["result"]["written_to"],
        other
            .join(".agix")
            .join("settings.json")
            .display()
            .to_string(),
        "{v}"
    );

    let local = std::fs::read_to_string(other.join(".agix").join("settings.local.json")).unwrap();
    let local: serde_json::Value = serde_json::from_str(&local).unwrap();
    assert_eq!(local["providers"]["deepseek"]["api_key"], "k");
    assert!(
        !dir.path().join(".agix").join("settings.json").exists(),
        "the daemon's own project must not be written when another one was named"
    );

    // `null` is refused rather than silently writing the global tier: these
    // methods act on the project tier, and a no-project session has none.
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"project_root":null,"provider_id":"x","config":{"api_type":"anthropic"}},"id":2}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], -32602, "{v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_partial_patch_does_not_clobber_untouched_fields() {
    let (_server, sock, dir, handle) = start_server().await;

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_type":"openai_compatible","base_url":"https://api.deepseek.com/v1","default_model":"deepseek-pro"}},"id":1}"#,
    )
    .await;

    // Second call only touches api_key — base_url/default_model must survive.
    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_key":"new-key"}},"id":2}"#,
    )
    .await;

    let written = std::fs::read_to_string(dir.path().join(".agix").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    let local =
        std::fs::read_to_string(dir.path().join(".agix").join("settings.local.json")).unwrap();
    let local: serde_json::Value = serde_json::from_str(&local).unwrap();
    assert_eq!(local["providers"]["deepseek"]["api_key"], "new-key");
    assert_eq!(
        written["providers"]["deepseek"]["base_url"],
        "https://api.deepseek.com/v1"
    );
    assert_eq!(
        written["providers"]["deepseek"]["default_model"],
        "deepseek-pro"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A key that an earlier write left in the committed file moves out the next
/// time that provider is written, and a delete clears it from both files.
#[tokio::test]
async fn set_provider_moves_a_committed_key_to_the_local_file() {
    let (_server, sock, dir, handle) = start_server().await;
    let agix = dir.path().join(".agix");
    std::fs::create_dir_all(&agix).unwrap();
    std::fs::write(
        agix.join("settings.json"),
        r#"{"providers":{"deepseek":{"api_key":"old-key","base_url":"https://x/v1"}},"prompt_append":"kept"}"#,
    )
    .unwrap();

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"default_model":"m"}},"id":1}"#,
    )
    .await;
    let read = |name: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(agix.join(name)).unwrap()).unwrap()
    };
    let committed = read("settings.json");
    assert!(
        committed["providers"]["deepseek"].get("api_key").is_none(),
        "{committed}"
    );
    assert_eq!(
        committed["providers"]["deepseek"]["base_url"],
        "https://x/v1"
    );
    assert_eq!(committed["prompt_append"], "kept");
    assert_eq!(
        read("settings.local.json")["providers"]["deepseek"]["api_key"],
        "old-key"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.getProvider","params":{"include_secrets":true},"id":2}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["result"]["providers"]["deepseek"]["api_key"], "old-key",
        "{v}"
    );

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","delete":true},"id":3}"#,
    )
    .await;
    assert!(read("settings.json")["providers"].get("deepseek").is_none());
    assert!(read("settings.local.json")["providers"]
        .get("deepseek")
        .is_none());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_delete_removes_entry() {
    let (_server, sock, dir, handle) = start_server().await;

    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"default_model":"deepseek-pro"}},"id":1}"#,
    )
    .await;
    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","delete":true},"id":2}"#,
    )
    .await;

    let written = std::fs::read_to_string(dir.path().join(".agix").join("settings.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert!(written["providers"].get("deepseek").is_none());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_rejects_missing_provider_id() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_refuses_to_clobber_a_malformed_existing_settings_json() {
    let (_server, sock, dir, handle) = start_server().await;

    let agix_dir = dir.path().join(".agix");
    std::fs::create_dir_all(&agix_dir).unwrap();
    let settings_path = agix_dir.join("settings.json");
    std::fs::write(&settings_path, "{not valid json").unwrap();

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"default_model":"deepseek-pro"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS, "resp: {v}");

    // The original (malformed) content must survive untouched — not get
    // silently replaced with `{}` + the patch.
    let still_there = std::fs::read_to_string(&settings_path).unwrap();
    assert_eq!(still_there, "{not valid json");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn get_provider_redacts_api_key_by_default_and_reveals_with_flag() {
    let (_server, sock, _dir, handle) = start_server().await;
    rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_key":"sk-secret-1234"}},"id":1}"#,
    )
    .await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.getProvider","id":2}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["result"]["providers"]["deepseek"]["api_key"], "***1234",
        "resp: {v}"
    );

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.getProvider","params":{"include_secrets":true},"id":3}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["result"]["providers"]["deepseek"]["api_key"], "sk-secret-1234",
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn config_reload_with_no_providers_reports_ok_and_no_router() {
    let (_server, sock, _dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.reload","id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "expected success, got: {v}");
    assert_eq!(v["result"]["routing"]["ok"], true, "resp: {v}");
    assert_eq!(v["result"]["routing"]["router_rebuilt"], false, "resp: {v}");
    assert!(v["result"]["providers"].as_array().unwrap().is_empty());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_with_openai_compatible_rebuilds_the_router() {
    // `openai_compatible` has a runtime `Model` impl
    // (`model::OpenAICompatibleModel`), so configuring a DeepSeek-shaped
    // provider builds and swaps the live router.
    let (_server, sock, _dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_type":"openai_compatible","base_url":"https://api.deepseek.com/v1","api_key":"k","default_model":"deepseek-pro"},"default_provider":"deepseek"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["routing"]["ok"], true, "resp: {v}");
    assert_eq!(v["result"]["routing"]["router_rebuilt"], true, "resp: {v}");
    assert!(
        v["result"]["routing"]["router_error"].is_null(),
        "resp: {v}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_with_openai_compatible_missing_base_url_reports_a_router_error() {
    // The one thing an `openai_compatible` provider cannot do without: there
    // is no default host to fall back to, and guessing one would surface as a
    // confusing auth failure on the first call instead of a config error.
    let (_server, sock, _dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_type":"openai_compatible","api_key":"k","default_model":"deepseek-pro"},"default_provider":"deepseek"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["routing"]["ok"], true, "resp: {v}");
    assert_eq!(v["result"]["routing"]["router_rebuilt"], false, "resp: {v}");
    let router_error = v["result"]["routing"]["router_error"].as_str().unwrap();
    assert!(
        router_error.contains("base_url"),
        "router_error: {router_error}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_with_unresolvable_config_reports_routing_not_ok() {
    // Distinguishes the other failure layer: `default_provider` pointing at
    // a provider that doesn't exist is a `resolve_task_models` failure, not
    // a router-build failure — `routing.ok` must be `false` here (unlike
    // the openai_compatible case above).
    let (_server, sock, _dir, handle) = start_server().await;

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"anthropic","config":{"api_type":"anthropic","api_key":"k","default_model":"claude-sonnet-4-6"},"default_provider":"ghost-provider"},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["routing"]["ok"], false, "resp: {v}");
    assert_eq!(v["result"]["routing"]["router_rebuilt"], false, "resp: {v}");
    let error = v["result"]["routing"]["error"].as_str().unwrap();
    assert!(error.contains("ghost-provider"), "error: {error}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn get_provider_reads_back_api_key_env_thinking_toggle_and_reasoning_passback() {
    let (_server, sock, _dir, handle) = start_server().await;
    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.setProvider","params":{"provider_id":"deepseek","config":{"api_type":"openai_compatible","base_url":"https://api.deepseek.com/v1","api_key_env":"AGIX_TEST_GETPROVIDER_KEY","reasoning_passback":true,"thinking_toggle":"deepseek"}},"id":1}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(v["result"].is_object(), "setProvider: {v}");

    let resp = rpc_call(
        &sock,
        r#"{"jsonrpc":"2.0","method":"config.getProvider","id":2}"#,
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let p = &v["result"]["providers"]["deepseek"];
    assert_eq!(p["api_key_env"], "AGIX_TEST_GETPROVIDER_KEY", "resp: {v}");
    assert_eq!(p["reasoning_passback"], true, "resp: {v}");
    assert_eq!(p["thinking_toggle"], "deepseek", "resp: {v}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn get_provider_reports_whether_the_credential_has_a_value_and_never_the_value() {
    const SET: &str = "AGIX_TEST_CRED_PRESENT_SET_7f3a";
    const UNSET: &str = "AGIX_TEST_CRED_PRESENT_UNSET_7f3a";
    const SECRET: &str = "sk-env-secret-value-91c4";
    // SAFETY: test-only unique names, no other test reads them.
    unsafe {
        std::env::set_var(SET, SECRET);
        std::env::remove_var(UNSET);
    }
    let (_server, sock, _dir, handle) = start_server().await;
    for (id, var) in [("has_key", SET), ("no_key", UNSET)] {
        let resp = rpc_call(
            &sock,
            &format!(
                r#"{{"jsonrpc":"2.0","method":"config.setProvider","params":{{"provider_id":"{id}","config":{{"api_type":"openai_compatible","base_url":"https://example.invalid/v1","api_key_env":"{var}"}}}},"id":1}}"#
            ),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert!(v["result"].is_object(), "setProvider: {v}");
    }

    for params in [r#""#, r#","params":{"include_secrets":true}"#] {
        let resp = rpc_call(
            &sock,
            &format!(r#"{{"jsonrpc":"2.0","method":"config.getProvider"{params},"id":2}}"#),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(
            v["result"]["providers"]["has_key"]["credential_present"], true,
            "resp: {v}"
        );
        assert_eq!(
            v["result"]["providers"]["no_key"]["credential_present"], false,
            "resp: {v}"
        );
        assert!(
            !resp.contains(SECRET),
            "the variable's value is in the reply: {resp}"
        );
    }

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// `tier: "scene"` writes the named scene's own settings, which that scene's
/// pairs read and no other scene's do.
#[tokio::test]
async fn set_provider_writes_the_scene_tier_of_the_scene_it_names() {
    let (_server, sock, dir, handle) = start_server().await;
    let saved = call(
        &sock,
        "config.setProvider",
        serde_json::json!({
            "tier": "scene", "scene": "harnix", "provider_id": "scened",
            "config": {"api_type": "anthropic", "api_key": "k"},
            "default_provider": "scened"
        }),
    )
    .await;
    let scene_dir = dir.path().join("scenes").join("harnix");
    assert_eq!(saved["tier"], "scene", "{saved}");
    assert_eq!(
        saved["written_to"],
        serde_json::json!(scene_dir.join("settings.json").display().to_string())
    );
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(scene_dir.join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(written["default_provider"], "scened");
    assert!(written["providers"]["scened"].get("api_key").is_none());

    let get = |scene: &str| {
        call(
            &sock,
            "config.getProvider",
            serde_json::json!({"scene": scene}),
        )
    };
    assert_eq!(get("harnix").await["default_provider"], "scened");
    assert!(get("code").await["default_provider"].is_null());

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A password in a provider's `base_url` is not the `api_key` the writer
/// moves to `settings.local.json`: the answer warns, naming the provider and
/// not the password, as `config.set` does.
#[tokio::test]
async fn a_password_in_a_providers_base_url_is_written_with_a_warning() {
    let (server, sock, _dir, handle) = start_server().await;
    let saved = call(
        &sock,
        "config.setProvider",
        serde_json::json!({"tier": "global", "provider_id": "gw",
            "config": {"api_type": "anthropic", "api_key": "k-in-local",
                       "base_url": "https://agent:hunter2-pass@gw.invalid/",
                       "default_model": "m"}}),
    )
    .await;
    let warnings = saved["warnings"].as_array().cloned().unwrap_or_default();
    assert_eq!(warnings.len(), 1, "{saved}");
    assert!(
        warnings[0].as_str().unwrap().contains("providers.gw"),
        "{saved}"
    );
    assert!(!saved.to_string().contains("hunter2-pass"), "{saved}");
    let shown = call(&sock, "config.getProvider", serde_json::json!({})).await;
    assert_eq!(
        shown["providers"]["gw"]["base_url"], "https://agent:***@gw.invalid/",
        "{shown}"
    );

    let clean = call(
        &sock,
        "config.setProvider",
        serde_json::json!({"tier": "global", "provider_id": "plain",
            "config": {"api_type": "anthropic", "api_key": "k-in-local", "default_model": "m"}}),
    )
    .await;
    assert!(clean.get("warnings").is_none(), "{clean}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}
