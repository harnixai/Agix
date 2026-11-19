//! `config.setProvider` / `config.getProvider` and the model router they rebuild.

use crate::support::*;
use daemon::rpc::codes;
use serde_json::{json, Value};

fn read(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The entry with `id` in a settings file's `providers` list.
fn entry<'a>(settings: &'a Value, id: &str) -> Option<&'a Value> {
    settings["providers"]
        .as_array()?
        .iter()
        .find(|e| e["id"] == id)
}

const DEEPSEEK: &str = r#"{"vendor":"deepseek","api":"openai_chat","base_url":"https://api.deepseek.com","api_key_env":"DEEPSEEK_API_KEY","tiers":{"strong":"deepseek-pro"}}"#;

#[tokio::test]
async fn set_provider_writes_a_list_entry_and_doctor_sees_it() {
    let (_server, sock, dir, handle) = start_server().await;

    let saved = call(
        &sock,
        "config.setProvider",
        json!({"provider_id": "deepseek",
               "config": serde_json::from_str::<Value>(DEEPSEEK).unwrap()}),
    )
    .await;
    assert_eq!(saved["routing"]["ok"], true, "{saved}");
    assert_eq!(saved["providers"], json!(["deepseek"]), "{saved}");
    assert_eq!(saved["order"], json!(["deepseek"]), "{saved}");
    assert!(saved.get("secrets_written_to").is_none(), "{saved}");

    let written = read(&dir.path().join(".agix").join("settings.json"));
    assert_eq!(
        entry(&written, "deepseek").unwrap()["tiers"]["strong"],
        "deepseek-pro"
    );

    let doctor = call(&sock, "daemon.doctor", json!({})).await;
    assert_eq!(doctor["providers"]["configured"], json!(["deepseek"]));
    assert_eq!(doctor["model"]["provider"], "deepseek");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_can_target_another_project_this_daemon_serves() {
    let (_server, sock, dir, handle) = start_server().await;

    let other = dir.path().join("other-project");
    std::fs::create_dir_all(&other).unwrap();
    let saved = call(
        &sock,
        "config.setProvider",
        json!({"project_root": other, "provider_id": "deepseek",
               "config": serde_json::from_str::<Value>(DEEPSEEK).unwrap()}),
    )
    .await;
    assert_eq!(
        saved["written_to"],
        other
            .join(".agix")
            .join("settings.json")
            .display()
            .to_string(),
        "{saved}"
    );
    assert!(entry(
        &read(&other.join(".agix").join("settings.json")),
        "deepseek"
    )
    .is_some());
    assert!(
        !dir.path().join(".agix").join("settings.json").exists(),
        "the daemon's own project must not be written when another one was named"
    );

    // `null` is refused rather than silently writing the global tier: these
    // methods act on the project tier, and a no-project session has none.
    let refused = call_frame(
        &sock,
        "config.setProvider",
        json!({"project_root": null, "provider_id": "x", "config": {"api": "anthropic"}}),
    )
    .await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_partial_patch_does_not_clobber_untouched_fields() {
    let (_server, sock, dir, handle) = start_server().await;
    call(
        &sock,
        "config.setProvider",
        json!({"provider_id": "deepseek",
               "config": serde_json::from_str::<Value>(DEEPSEEK).unwrap()}),
    )
    .await;
    call(
        &sock,
        "config.setProvider",
        json!({"provider_id": "deepseek", "config": {"tiers": {"fast": "deepseek-flash"}}}),
    )
    .await;

    let written = read(&dir.path().join(".agix").join("settings.json"));
    let deepseek = entry(&written, "deepseek").unwrap();
    assert_eq!(deepseek["base_url"], "https://api.deepseek.com");
    assert_eq!(
        deepseek["tiers"],
        json!({"strong": "deepseek-pro", "fast": "deepseek-flash"})
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A provider is written where it is defined — here the global tier's
/// `settings.local.json` — whatever tier the call names; a new one goes to
/// the tier named.
#[tokio::test]
async fn set_provider_writes_a_provider_back_where_it_is_defined() {
    let (_server, sock, dir, handle) = start_server().await;
    let global_local = dir.path().join("settings.local.json");
    std::fs::write(
        &global_local,
        r#"{"providers": [{"id": "mine", "tiers": {"strong": "m"}}]}"#,
    )
    .unwrap();
    call(&sock, "config.reload", json!({})).await;

    let saved = call(
        &sock,
        "config.setProvider",
        json!({"tier": "project", "provider_id": "mine", "config": {"base_url": "https://gw.example"}}),
    )
    .await;
    assert_ne!(saved["tier"], "project", "{saved}");
    assert_eq!(
        entry(&read(&global_local), "mine").unwrap()["base_url"],
        "https://gw.example"
    );
    assert!(!dir.path().join(".agix").join("settings.json").exists());

    let shown = call(&sock, "config.getProvider", json!({"provider_id": "mine"})).await;
    assert_eq!(
        shown["providers"][0]["defined_in"],
        global_local.display().to_string(),
        "{shown}"
    );

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_delete_removes_entry() {
    let (_server, sock, dir, handle) = start_server().await;
    call(
        &sock,
        "config.setProvider",
        json!({"provider_id": "deepseek", "config": {"tiers": {"strong": "deepseek-pro"}}}),
    )
    .await;
    let deleted = call(
        &sock,
        "config.setProvider",
        json!({"provider_id": "deepseek", "delete": true}),
    )
    .await;
    assert_eq!(deleted["providers"], json!(["anthropic"]), "{deleted}");

    let written = read(&dir.path().join(".agix").join("settings.json"));
    assert!(entry(&written, "deepseek").is_none(), "{written}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_rejects_a_missing_id_and_an_old_key() {
    let (_server, sock, _dir, handle) = start_server().await;
    let missing = call_frame(&sock, "config.setProvider", json!({})).await;
    assert_eq!(missing["error"]["code"], codes::INVALID_PARAMS, "{missing}");

    let old = call_frame(
        &sock,
        "config.setProvider",
        json!({"provider_id": "deepseek", "config": {"api_type": "openai_compatible"}}),
    )
    .await;
    assert_eq!(old["error"]["code"], codes::INVALID_PARAMS, "{old}");
    assert!(
        old["error"]["message"]
            .as_str()
            .unwrap()
            .contains("providers[].api"),
        "{old}"
    );

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

    let refused = call_frame(
        &sock,
        "config.setProvider",
        json!({"provider_id": "deepseek", "config": {"tiers": {"strong": "deepseek-pro"}}}),
    )
    .await;
    assert_eq!(refused["error"]["code"], codes::INVALID_PARAMS, "{refused}");

    // The original (malformed) content must survive untouched — not get
    // silently replaced with `{}` + the patch.
    let still_there = std::fs::read_to_string(&settings_path).unwrap();
    assert_eq!(still_there, "{not valid json");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn config_reload_with_no_providers_routes_to_the_built_in_one() {
    let (_server, sock, _dir, handle) = start_server().await;

    let reloaded = call(&sock, "config.reload", json!({})).await;
    assert_eq!(reloaded["routing"]["ok"], true, "{reloaded}");
    assert_eq!(reloaded["routing"]["router_rebuilt"], true, "{reloaded}");
    assert_eq!(reloaded["providers"], json!(["anthropic"]), "{reloaded}");
    assert_eq!(reloaded["order"], json!(["anthropic"]), "{reloaded}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_with_openai_chat_rebuilds_the_router() {
    let (_server, sock, _dir, handle) = start_server().await;
    let saved = call(
        &sock,
        "config.setProvider",
        json!({"provider_id": "deepseek",
               "config": serde_json::from_str::<Value>(DEEPSEEK).unwrap()}),
    )
    .await;
    assert_eq!(saved["routing"]["ok"], true, "{saved}");
    assert_eq!(saved["routing"]["router_rebuilt"], true, "{saved}");
    assert!(saved["routing"]["router_error"].is_null(), "{saved}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn set_provider_with_openai_chat_missing_base_url_reports_a_router_error() {
    // The one thing an `openai_chat` provider cannot do without: there is no
    // default host to fall back to, and guessing one would surface as a
    // confusing auth failure on the first call instead of a config error.
    let (_server, sock, _dir, handle) = start_server().await;
    let saved = call(
        &sock,
        "config.setProvider",
        json!({"provider_id": "deepseek",
               "config": {"api": "openai_chat", "tiers": {"strong": "deepseek-pro"}}}),
    )
    .await;
    assert_eq!(saved["routing"]["ok"], true, "{saved}");
    assert_eq!(saved["routing"]["router_rebuilt"], false, "{saved}");
    let router_error = saved["routing"]["router_error"].as_str().unwrap();
    assert!(router_error.contains("base_url"), "{router_error}");

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// The other failure layer: settings that do not route — a tier outside the
/// provider's models, an order naming a provider nobody defines — are
/// `routing.ok: false`, and the previous router is kept.
#[tokio::test]
async fn providers_that_do_not_route_report_routing_not_ok() {
    let (_server, sock, _dir, handle) = start_server().await;
    let saved = call(
        &sock,
        "config.setProvider",
        json!({"provider_id": "deepseek",
               "config": {"models": ["deepseek-pro"], "tiers": {"strong": "deepseek-max"}}}),
    )
    .await;
    assert_eq!(saved["routing"]["ok"], false, "{saved}");
    let error = saved["routing"]["error"].as_str().unwrap();
    assert!(error.contains("deepseek-max"), "{error}");

    call(
        &sock,
        "config.setProvider",
        json!({"provider_id": "deepseek", "delete": true}),
    )
    .await;
    let set = call(
        &sock,
        "config.set",
        json!({"tier": "project", "changes": {"model.order": ["ghost-provider"]}}),
    )
    .await;
    assert_eq!(set["routing"]["ok"], false, "{set}");
    assert!(
        set["routing"]["error"]
            .as_str()
            .unwrap()
            .contains("ghost-provider"),
        "{set}"
    );

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
        call(
            &sock,
            "config.setProvider",
            json!({"provider_id": id, "config": {
                "vendor": "deepseek", "api": "openai_chat",
                "base_url": "https://example.invalid/v1", "api_key_env": var,
                "tiers": {"strong": "m"}}}),
        )
        .await;
    }

    for params in [json!({}), json!({"include_secrets": true})] {
        let shown = call(&sock, "config.getProvider", params).await;
        let of = |id: &str| {
            shown["providers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["id"] == id)
                .cloned()
                .unwrap()
        };
        assert_eq!(of("has_key")["credential_present"], true, "{shown}");
        assert_eq!(of("no_key")["credential_present"], false, "{shown}");
        assert_eq!(of("has_key")["vendor"], "deepseek", "{shown}");
        assert_eq!(of("has_key")["api_key_env"], SET, "{shown}");
        assert!(
            !shown.to_string().contains(SECRET),
            "the variable's value is in the reply: {shown}"
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
        json!({
            "tier": "scene", "scene": "harnix", "provider_id": "scened",
            "config": {"api": "anthropic", "tiers": {"strong": "m"}}
        }),
    )
    .await;
    let scene_dir = dir.path().join("scenes").join("harnix");
    assert_eq!(saved["tier"], "scene", "{saved}");
    assert_eq!(
        saved["written_to"],
        json!(scene_dir.join("settings.json").display().to_string())
    );
    assert!(entry(&read(&scene_dir.join("settings.json")), "scened").is_some());

    let order = |scene: &str| {
        let sock = sock.clone();
        let scene = scene.to_string();
        async move { call(&sock, "config.getProvider", json!({"scene": scene})).await["order"].clone() }
    };
    assert_eq!(order("harnix").await, json!(["scened"]));
    assert_eq!(order("code").await, json!(["anthropic"]));

    _server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A password in a provider's `base_url` is written as it is, with a
/// warning that names the providers and not the password, as `config.set`
/// does; reading it back scrubs it.
#[tokio::test]
async fn a_password_in_a_providers_base_url_is_written_with_a_warning() {
    let (server, sock, _dir, handle) = start_server().await;
    let saved = call(
        &sock,
        "config.setProvider",
        json!({"tier": "global", "provider_id": "gw",
            "config": {"api": "anthropic",
                       "base_url": "https://agent:hunter2-pass@gw.invalid/",
                       "tiers": {"strong": "m"}}}),
    )
    .await;
    let warnings = saved["warnings"].as_array().cloned().unwrap_or_default();
    assert_eq!(warnings.len(), 1, "{saved}");
    assert!(
        warnings[0].as_str().unwrap().contains("providers.gw"),
        "{saved}"
    );
    assert!(!saved.to_string().contains("hunter2-pass"), "{saved}");
    let shown = call(&sock, "config.getProvider", json!({"provider_id": "gw"})).await;
    assert_eq!(
        shown["providers"][0]["base_url"], "https://agent:***@gw.invalid/",
        "{shown}"
    );

    let clean = call(
        &sock,
        "config.setProvider",
        json!({"tier": "global", "provider_id": "plain",
            "config": {"api": "anthropic", "tiers": {"strong": "m"}}}),
    )
    .await;
    assert!(clean.get("warnings").is_none(), "{clean}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}
