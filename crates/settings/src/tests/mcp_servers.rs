//! What an untrusted project may do to `mcp.servers`, and what a global server holds back.

use super::*;

#[test]
fn an_untrusted_project_cannot_turn_a_disabled_server_back_on_with_null() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("global");
    let project = root.path().join("repo").join(".agix");
    write_settings(
        &global,
        r#"{"mcp": {"servers": {"prod-db": {"command": "prod-db-mcp", "enabled": false}}}}"#,
    );
    write_settings(
        &project,
        r#"{"mcp": {"servers": {"prod-db": {"enabled": null}}}}"#,
    );
    let loaded = LoadedSettings::load(
        global,
        root.path().join("scene"),
        project,
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    assert!(loaded.trust().is_untrusted());
    assert!(
        !loaded.values.mcp.is_enabled("prod-db"),
        "withheld: {:?}",
        loaded.requires_trust()
    );
}

#[test]
fn a_server_switch_that_is_not_true_or_false_is_ignored() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("global");
    let scene = root.path().join("scene");
    write_settings(
        &global,
        r#"{"mcp": {"servers": {"prod-db": {"command": "prod-db-mcp", "enabled": false}}}}"#,
    );
    write_settings(
        &scene,
        r#"{"mcp": {"servers": {"prod-db": {"enabled": null}}}}"#,
    );
    let loaded = LoadedSettings::load_without_project(
        global,
        scene,
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    assert!(!loaded.values.mcp.is_enabled("prod-db"));
    let keys: Vec<_> = loaded.ignored.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, ["mcp.servers.prod-db.enabled"]);
}

#[test]
fn what_a_global_mcp_server_reads_is_held_back_and_a_projects_own_is_not() {
    use base::interface::exec::env::is_engine_credential;
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("global");
    let project = root.path().join("project");
    write_settings(
        &global,
        r#"{"mcp": {"servers": {"docs": {"type": "streamable_http", "url": "https://h/mcp",
            "headers": {"X-Key": "$AGIX_SETTINGS_TEST_MCP_HEADER"},
            "oauth": {"client_id": "c", "client_secret_env": "AGIX_SETTINGS_TEST_MCP_SECRET",
                      "issuer": "https://login.example"}}}}}"#,
    );
    write_settings(
        &project,
        r#"{"mcp": {"servers": {"own": {"type": "stdio", "command": "x",
            "env": {"T": "$AGIX_SETTINGS_TEST_PROJECT_VAR"}}}}}"#,
    );
    LoadedSettings::load(
        global,
        root.path().join("scene"),
        project,
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    for name in [
        "AGIX_SETTINGS_TEST_MCP_HEADER",
        "AGIX_SETTINGS_TEST_MCP_SECRET",
    ] {
        assert!(is_engine_credential(std::ffi::OsStr::new(name)), "{name}");
    }
    assert!(!is_engine_credential(std::ffi::OsStr::new(
        "AGIX_SETTINGS_TEST_PROJECT_VAR"
    )));
}
