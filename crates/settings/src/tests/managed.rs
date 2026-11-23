//! The managed layer: held apart, set over every tier, and its own trust decision.

use super::*;

#[test]
fn a_managed_layer_is_held_apart_and_takes_bypass_away() {
    use base::interface::config_source::{InMemoryLayers, Tier};

    let source = InMemoryLayers::new()
        .with(
            Tier::Global,
            false,
            "user",
            serde_json::json!({"permissions": {"mode": "bypassPermissions", "allow": ["Bash"]}}),
        )
        .with(
            Tier::Managed,
            false,
            "it",
            serde_json::json!({"permissions": {"disable_bypass": true, "deny": ["Bash(curl:*)"]}}),
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

    assert_eq!(settings.values.permissions.mode, PermissionMode::Default);
    assert!(settings.bypass_disabled());
    assert_eq!(shared_rules(&settings).len(), 1);
    let managed = settings.managed.expect("the managed layer");
    assert_eq!(managed.origin, "it");
    assert_eq!(managed.rules[0].tool, "Bash(curl:*)");
}

#[test]
fn a_managed_layer_sets_any_key_over_every_tier_and_a_broken_one_none() {
    use base::interface::config_source::{InMemoryLayers, Tier};

    let load = |managed: serde_json::Value| {
        let source = InMemoryLayers::new()
            .with(
                Tier::Global,
                false,
                "user",
                serde_json::json!({"model": {"max_tokens": 2000}, "app": {"session_cap": 7}}),
            )
            .with(
                Tier::Cli,
                false,
                "cli",
                serde_json::json!({"model": {"max_tokens": 3000}}),
            )
            .with(Tier::Managed, false, "it", managed);
        let root = tempfile::tempdir().unwrap();
        LoadedSettings::load_from(
            &source,
            root.path().join("global"),
            root.path().join("scene"),
            root.path().join("project"),
            "code",
            "m",
            &Sections::none(),
            Default::default(),
        )
    };

    let settings = load(serde_json::json!({"model": {"max_tokens": 5}, "app": {"session_cap": 2}}));
    assert_eq!(
        settings.managed.as_ref().and_then(|m| m.error.clone()),
        None
    );
    assert_eq!(settings.values.model.max_tokens, 5);
    assert_eq!(settings.values.app.session_cap, 2);

    let settings = load(serde_json::json!({"model": {"max_tokens": 5}, "modle": {}}));
    assert_eq!(settings.values.model.max_tokens, 3000);
    let error = settings.managed.and_then(|m| m.error).expect("an error");
    assert!(error.contains("modle"), "{error}");
}

#[test]
fn a_managed_trust_decision_stands_over_the_users() {
    use base::interface::config_source::{InMemoryLayers, Tier};
    use base::permission::ProjectTrust;

    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("work");
    let load = |managed: serde_json::Value| {
        let source = InMemoryLayers::new()
            .with(
                Tier::Global,
                false,
                "user",
                serde_json::json!({"app": {"trust_all_projects": true, "trusted_projects": [project]}}),
            )
            .with(Tier::Managed, false, "it", managed);
        LoadedSettings::load_from(
            &source,
            root.path().join("global"),
            root.path().join("scene"),
            project.join(".agix"),
            "code",
            "m",
            &Sections::none(),
            Default::default(),
        )
    };
    assert_eq!(load(serde_json::json!({})).trust(), ProjectTrust::Trusted);
    let decided =
        load(serde_json::json!({"app": {"trust_all_projects": false, "trusted_projects": []}}));
    assert_eq!(decided.trust(), ProjectTrust::Untrusted);
    assert!(!decided.values.app.trust_all_projects);
    assert!(decided.values.app.trusted_projects.is_empty());
}
