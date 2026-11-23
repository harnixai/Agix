//! What loading reports instead of failing: ignored keys, bad rules, malformed files, missing directories.

use super::*;

#[test]
fn load_never_lets_settings_json_override_paths() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("global");
    write_settings(&global, r#"{"paths": {"scope": "hijacked"}}"#);

    let settings = LoadedSettings::load(
        global,
        root.path().join("scene"),
        root.path().join("project"),
        "real-scope",
        "m",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(settings.paths.scope, "real-scope");
}

#[test]
fn load_malformed_layer_warns_and_is_skipped_not_fatal() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("global");
    write_settings(&global, "not valid json {{{");
    // Must not panic — malformed layer is skipped, defaults are used.
    let settings = LoadedSettings::load(
        global,
        root.path().join("scene"),
        root.path().join("project"),
        "code",
        "fallback-model",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(settings.values.model.model_name, "fallback-model");
}

#[test]
fn a_key_no_setting_reads_is_reported() {
    let layer = serde_json::json!({
        "$schema": "../schemas/settings.schema.json",
        "model": {},
        "permissions": {"mode": "plan"},
        "permisions": {"mode": "plan"},
        "hooks": {},
        "hook": {},
    });
    let mut unknown = unknown_settings_keys(&layer, &Sections::none());
    unknown.sort();
    assert_eq!(unknown, ["hook", "permisions"]);
    assert!(unknown_settings_keys(&serde_json::json!([1]), &Sections::none()).is_empty());
}

/// Without a project there is none: not the state root's parent, which
/// for `~/.agix` is the user's home.
#[test]
fn settings_without_a_project_have_no_project_root() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join(".agix");
    let settings = LoadedSettings::load_without_project(
        state.clone(),
        state.join("scenes/harnix"),
        "harnix",
        "m",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(settings.paths.project_root(), None);
    assert!(settings.paths.working_dir().starts_with(&state));
    assert_eq!(settings.paths.local_data_dir, state);

    let project = LoadedSettings::load(
        state.clone(),
        state.join("scenes/code"),
        root.path().join("repo/.agix"),
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(
        project.paths.project_root(),
        Some(root.path().join("repo").as_path())
    );
    assert_eq!(project.paths.working_dir(), root.path().join("repo"));
}

#[test]
fn session_dir_is_an_unknown_key_and_the_rest_of_its_file_loads() {
    let layer = serde_json::json!({"session_dir": "/tmp/x", "prompt": {"language": "en"}});
    assert_eq!(
        unknown_settings_keys(&layer, &Sections::none()),
        ["session_dir"]
    );

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.json"), layer.to_string()).unwrap();
    let settings = LoadedSettings::load(
        dir.path().to_path_buf(),
        dir.path().join("scene"),
        dir.path().join("project"),
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(settings.values.prompt.language.as_deref(), Some("en"));
    assert!(settings.ignored.is_empty());
}

#[test]
fn a_bad_permission_rule_is_left_out_alone_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.json"),
        r#"{"permissions": {"allow": [5], "deny": ["Bash"], "ask": "Read"}}"#,
    )
    .unwrap();
    let settings = LoadedSettings::load(
        dir.path().to_path_buf(),
        dir.path().join("scene"),
        dir.path().join("project"),
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    let rules: Vec<_> = shared_rules(&settings)
        .into_iter()
        .map(|r| (r.rule.tool.as_str(), r.rule.action))
        .collect();
    assert_eq!(rules, [("Bash", RuleBehavior::Deny)]);
    assert_eq!(
        ignored_keys(&settings),
        ["permissions.allow[0]", "permissions.ask"]
    );
}

#[test]
fn a_deny_rule_that_does_not_parse_is_reported_rather_than_skipped_silently() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.json"),
        r#"{"permissions": {"deny": ["Bash(rm:*", "(rm:*)", "Bash(ls:*)"]}}"#,
    )
    .unwrap();
    let settings = LoadedSettings::load(
        dir.path().to_path_buf(),
        dir.path().join("scene"),
        dir.path().join("project"),
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    let keys: Vec<_> = settings.ignored.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, ["permissions.deny[0]", "permissions.deny[1]"]);
    assert!(settings.ignored[0].error.contains("Bash(rm:*"));
    assert_eq!(shared_rules(&settings).len(), 1);
}

#[test]
fn load_missing_directories_returns_plain_defaults() {
    let root = tempfile::tempdir().unwrap();
    let settings = LoadedSettings::load(
        root.path().join("nonexistent-global"),
        root.path().join("nonexistent-scene"),
        root.path().join("nonexistent-project"),
        "code",
        "fallback-model",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(settings.values.model.model_name, "fallback-model");
    assert_eq!(settings.values.providers.len(), 1);
    assert!(settings.values.providers[0].builtin);
}

#[test]
fn a_value_left_out_is_reported_at_its_own_key_even_alone_in_its_file() {
    let alone = load_global(r#"{"sandbox": {"network": {"mode": "open"}}}"#);
    assert_eq!(ignored_keys(&alone), ["sandbox.network.mode"]);

    let both = load_global(r#"{"sandbox": {"network": {"mode": "open"}, "require": "yes"}}"#);
    assert_eq!(
        ignored_keys(&both),
        ["sandbox.network.mode", "sandbox.require"]
    );
}

#[test]
fn an_object_where_a_value_belongs_is_reported_as_that_value() {
    let settings =
        load_global(r#"{"web": {"allowed_domains": {"a": 1}}, "prompt": {"language": "x"}}"#);
    assert_eq!(ignored_keys(&settings), ["web.allowed_domains"]);
    assert_eq!(settings.values.prompt.language.as_deref(), Some("x"));
}

#[test]
fn a_program_only_permission_mode_is_left_out_and_reported_with_its_file() {
    let settings =
        load_global(r#"{"permissions": {"mode": "bubble"}, "prompt": {"language": "x"}}"#);
    assert_eq!(ignored_keys(&settings), ["permissions.mode"]);
    assert!(settings.ignored[0].origin.ends_with("settings.json"));
    assert_eq!(settings.values.permissions.mode, PermissionMode::Default);
    assert_eq!(settings.values.prompt.language.as_deref(), Some("x"));
}

#[test]
fn what_an_untrusted_project_holds_back_names_the_file_it_is_in() {
    let root = tempfile::tempdir().unwrap();
    let (state, project) = (root.path().join("state"), root.path().join("repo/.agix"));
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("settings.json"),
        r#"{"permissions": {"mode": "bypassPermissions"}}"#,
    )
    .unwrap();
    std::fs::write(
        project.join("settings.local.json"),
        r#"{"permissions": {"allow": ["Bash"]}}"#,
    )
    .unwrap();
    let settings = LoadedSettings::load(
        state.clone(),
        state.join("scenes/code"),
        project.clone(),
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    let withheld: Vec<(&str, String)> = settings
        .withheld()
        .iter()
        .map(|w| (w.key.as_str(), w.origin.clone()))
        .collect();
    assert_eq!(
        withheld,
        [
            (
                "permissions.mode",
                project.join("settings.json").display().to_string()
            ),
            (
                "permissions.allow(Bash)",
                project.join("settings.local.json").display().to_string()
            ),
        ]
    );
    assert_eq!(
        settings.requires_trust(),
        ["permissions.mode", "permissions.allow(Bash)"]
    );
}
