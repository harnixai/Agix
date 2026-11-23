//! Where layers come from: the CLI layer, in-memory sources, fallbacks and the child-process environment.

use super::*;

#[test]
fn a_cli_layer_is_above_the_project_and_read_as_the_global_tier() {
    use base::interface::config_source::{InMemoryLayers, Tier};

    let source = InMemoryLayers::new()
        .with(
            Tier::Project,
            false,
            "project",
            serde_json::json!({"turn": {"max_model_calls": 9}, "permissions": {"deny": ["Read"]}}),
        )
        .with(
            Tier::Cli,
            false,
            "cli",
            serde_json::json!({
                "turn": {"max_model_calls": 2},
                "app": {"history": "project"},
                "permissions": {"deny": ["Write"]},
                "memory": {"enabled": 5},
            }),
        );
    let root = tempfile::tempdir().unwrap();
    let settings = LoadedSettings::load_from(
        &source,
        root.path().join("global"),
        root.path().join("scene"),
        root.path().join("project"),
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );

    assert_eq!(settings.values.turn.max_model_calls, 2);
    assert_eq!(settings.values.app.history, crate::HistoryLocation::Project);
    let rules: Vec<_> = shared_rules(&settings)
        .iter()
        .map(|r| (r.rule.tool.clone(), r.tier))
        .collect();
    assert_eq!(
        rules,
        [
            ("Read".to_string(), Tier::Project),
            ("Write".to_string(), Tier::Cli)
        ]
    );
    assert_eq!(settings.ignored.len(), 1, "{:?}", settings.ignored);
    assert_eq!(
        (
            settings.ignored[0].tier,
            settings.ignored[0].origin.as_str()
        ),
        ("cli", "cli")
    );
    assert_eq!(settings.ignored[0].key, "memory.enabled");
    assert_eq!(
        settings
            .cli_layer
            .as_ref()
            .map(|l| &l["turn"]["max_model_calls"]),
        Some(&serde_json::json!(2))
    );
}

/// The same configuration, once from six files and
/// once from a source that never touches a disk, has to produce the same
/// `Settings` — including the two rules that are not part of the generic
/// merge (an overlay's `permissions` rule lists held apart, `paths` ignored).
#[test]
fn an_in_memory_source_replaces_the_files_entirely() {
    use base::interface::config_source::{InMemoryLayers, Tier};

    let global = r#"{"model": {"effort": "low", "max_tokens": 111},
                     "app": {"trusted_projects": ["/**"]}}"#;
    let scene = r#"{"model": {"effort": "max"}}"#;
    let project = r#"{"paths": {"scope": "hijacked"},
                      "permissions": {"deny": ["Bash(rm:*)"]}}"#;
    let project_local = r#"{"permissions": {"allow": ["Bash(ls)"]}}"#;

    let root = tempfile::tempdir().unwrap();
    let global_dir = root.path().join("global");
    let scene_dir = root.path().join("scene");
    let project_dir = root.path().join("project");
    write_settings(&global_dir, global);
    write_settings(&scene_dir, scene);
    write_settings(&project_dir, project);
    write_local_settings(&project_dir, project_local);
    let from_files = LoadedSettings::load(
        global_dir,
        scene_dir,
        project_dir,
        "code",
        "cli-default",
        &Sections::none(),
        Default::default(),
    );

    let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
    let source = InMemoryLayers::new()
        .with(Tier::Global, false, "config-service:global", parse(global))
        .with(Tier::Scene, false, "config-service:scene", parse(scene))
        .with(
            Tier::Project,
            false,
            "config-service:project",
            parse(project),
        )
        .with(
            Tier::Project,
            true,
            "config-service:machine",
            parse(project_local),
        );
    // Directories that do not exist, so a fall-back to reading files
    // could not produce this result.
    let elsewhere = root.path().join("no-such-tree");
    let from_memory = LoadedSettings::load_from(
        &source,
        elsewhere.join("global"),
        elsewhere.join("scene"),
        elsewhere.join("project"),
        "code",
        "cli-default",
        &Sections::none(),
        Default::default(),
    );

    assert_eq!(
        from_memory.values.model.effort,
        from_files.values.model.effort
    );
    assert_eq!(
        from_memory.values.model.effort,
        Some(base::interface::model::Effort::Max)
    );
    assert_eq!(
        from_memory.values.model.max_tokens,
        from_files.values.model.max_tokens
    );
    assert_eq!(
        shared_rules(&from_memory)
            .into_iter()
            .map(|r| r.rule.tool.as_str())
            .collect::<Vec<_>>(),
        vec!["Bash(rm:*)"]
    );
    assert_eq!(
        local_rules(&from_memory)
            .into_iter()
            .map(|r| r.rule.tool.as_str())
            .collect::<Vec<_>>(),
        vec!["Bash(ls)"]
    );
    assert_eq!(from_memory.paths.scope, "code");
    assert_eq!(from_memory.paths.local_data_dir, elsewhere.join("project"));
}

#[test]
fn load_falls_back_to_cli_default_model_when_no_layer_sets_it() {
    let root = tempfile::tempdir().unwrap();
    let settings = LoadedSettings::load(
        root.path().join("global"),
        root.path().join("scene"),
        root.path().join("project"),
        "code",
        "cli-default-model",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(settings.values.model.model_name, "cli-default-model");
}

#[test]
fn a_provider_key_variable_is_held_back_from_child_processes_once_loaded() {
    use base::interface::exec::env::is_engine_credential;
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    write_settings(
        &project,
        r#"{"providers": [{"id": "deepseek", "tiers": {"strong": "pro"},
                           "api_key_env": "AGIX_SETTINGS_TEST_DEEPSEEK_KEY"}]}"#,
    );
    trust_project(&root.path().join("global"), &project);
    let name = std::ffi::OsStr::new("AGIX_SETTINGS_TEST_DEEPSEEK_KEY");
    assert!(!is_engine_credential(name));
    LoadedSettings::load(
        root.path().join("global"),
        root.path().join("scene"),
        project,
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    assert!(is_engine_credential(name));
}
