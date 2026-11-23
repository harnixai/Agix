//! How the global, scene and project tiers and their local overlays combine into one `LoadedSettings`.

use super::*;

#[test]
fn local_overlay_outranks_the_settings_json_beside_it() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    write_settings(&project, r#"{"model": {"max_tokens": 1000}}"#);
    write_local_settings(&project, r#"{"model": {"max_tokens": 2000}}"#);

    let settings = LoadedSettings::load(
        root.path().join("global"),
        root.path().join("scene"),
        project,
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(settings.values.model.max_tokens, 2000);
}

/// The local overlay's rules must *add to* the committed tier's, not
/// replace them the way the generic array merge would — both sets have to
/// reach the rule engine for `RuleSource::LocalSettings`'s higher priority
/// to decide anything.
#[test]
fn local_overlay_permission_rules_add_to_rather_than_replace_the_committed_ones() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    write_settings(&project, r#"{"permissions": {"deny": ["Bash(rm:*)"]}}"#);
    write_local_settings(&project, r#"{"permissions": {"allow": ["Bash(ls)"]}}"#);
    trust_project(&root.path().join("global"), &project);

    let settings = LoadedSettings::load(
        root.path().join("global"),
        root.path().join("scene"),
        project,
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(
        shared_rules(&settings)
            .into_iter()
            .map(|r| r.rule.tool.as_str())
            .collect::<Vec<_>>(),
        vec!["Bash(rm:*)"],
        "the committed tier's rules must survive the overlay"
    );
    assert_eq!(
        local_rules(&settings)
            .into_iter()
            .map(|r| r.rule.tool.as_str())
            .collect::<Vec<_>>(),
        vec!["Bash(ls)"]
    );
}

#[test]
fn local_overlay_is_read_from_every_tier() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("global");
    let project = root.path().join("project");
    write_local_settings(&global, r#"{"permissions": {"allow": ["Bash(id)"]}}"#);
    write_local_settings(&project, r#"{"permissions": {"allow": ["Bash(ls)"]}}"#);
    trust_project(&global, &project);

    let settings = LoadedSettings::load(
        global,
        root.path().join("scene"),
        project,
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(
        local_rules(&settings)
            .into_iter()
            .map(|r| r.rule.tool.as_str())
            .collect::<Vec<_>>(),
        vec!["Bash(id)", "Bash(ls)"]
    );
}

#[test]
fn committed_tiers_contribute_rules_side_by_side_each_tagged_with_its_tier() {
    use base::interface::config_source::Tier;
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("global");
    let scene = root.path().join("scene");
    let project = root.path().join("project");
    write_settings(&global, r#"{"permissions": {"deny": ["Bash(rm:*)"]}}"#);
    write_settings(&scene, r#"{"permissions": {"deny": ["Read(~/.ssh/**)"]}}"#);
    write_settings(&project, r#"{"permissions": {"allow": ["Bash(ls)"]}}"#);
    trust_project(&global, &project);

    let settings = LoadedSettings::load(
        global,
        scene,
        project,
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(
        shared_rules(&settings)
            .into_iter()
            .map(|r| (r.rule.tool.as_str(), r.tier))
            .collect::<Vec<_>>(),
        vec![
            ("Bash(rm:*)", Tier::Global),
            ("Read(~/.ssh/**)", Tier::Scene),
            ("Bash(ls)", Tier::Project),
        ],
        "a project tier that writes any rule must not erase the user's"
    );
}

#[test]
fn a_directory_serving_as_two_tiers_contributes_its_rules_once() {
    use base::interface::config_source::Tier;
    let root = tempfile::tempdir().unwrap();
    let home_agix = root.path().join("home");
    write_settings(&home_agix, r#"{"permissions": {"deny": ["Bash(rm:*)"]}}"#);

    let settings = LoadedSettings::load(
        home_agix.clone(),
        root.path().join("scene"),
        home_agix,
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(
        shared_rules(&settings)
            .into_iter()
            .map(|r| (r.rule.tool.as_str(), r.tier))
            .collect::<Vec<_>>(),
        vec![("Bash(rm:*)", Tier::Project)]
    );
}

#[test]
fn local_overlay_can_never_override_paths_either() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("global");
    write_local_settings(&global, r#"{"paths": {"scope": "hijacked"}}"#);

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
fn load_merges_three_tiers_low_to_high() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("global");
    let scene = root.path().join("scene");
    let project = root.path().join("project");
    write_settings(&global, r#"{"model": {"max_tokens": 1000}}"#);
    write_settings(&scene, r#"{"model": {"max_tokens": 2000}}"#);
    write_settings(&project, r#"{}"#);

    let settings = LoadedSettings::load(
        global,
        scene,
        project,
        "code",
        "cli-default",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(settings.values.model.max_tokens, 2000);
}
