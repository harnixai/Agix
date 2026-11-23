use super::{ignored_keys, load_global, write_local_settings, write_settings};
use crate::section::Sections;
use crate::LoadedSettings;

fn load(global: &str, global_local: &str, scene: &str) -> LoadedSettings {
    let root = tempfile::tempdir().unwrap();
    let (global_dir, scene_dir) = (root.path().join("global"), root.path().join("scene"));
    write_settings(&global_dir, global);
    write_local_settings(&global_dir, global_local);
    write_settings(&scene_dir, scene);
    LoadedSettings::load(
        global_dir,
        scene_dir,
        root.path().join("project"),
        "code",
        "cli-model",
        &Sections::none(),
        Default::default(),
    )
}

#[test]
fn providers_add_up_across_tiers_and_a_repeated_id_is_ignored() {
    let settings = load(
        r#"{"providers": [{"id": "ds", "tiers": {"strong": "global-pro"}}]}"#,
        r#"{"providers": [{"id": "claude", "tiers": {"strong": "opus"}}]}"#,
        r#"{"providers": [{"id": "ds", "tiers": {"strong": "scene-pro"}},
                          {"id": "local", "tiers": {"strong": "qwen"}}],
            "model": {"order": ["claude", "ds"]}}"#,
    );
    let ids: Vec<&str> = settings
        .values
        .providers
        .iter()
        .map(|p| p.id.as_str())
        .collect();
    assert_eq!(ids, ["ds", "claude", "local"]);
    assert_eq!(
        settings
            .values
            .provider("ds")
            .unwrap()
            .tiers
            .strong
            .as_deref(),
        Some("global-pro"),
        "the first definition stands"
    );
    assert_eq!(ignored_keys(&settings), ["providers[0]"]);
    assert!(settings.ignored[0].error.contains("already defined"));
    assert!(settings.provider_origins["claude"].machine_local);
    assert_eq!(settings.values.model.model_name, "opus");
}

#[test]
fn a_provider_whose_id_is_not_letters_digits_dashes_and_underscores_is_ignored() {
    let settings = load_global(
        r#"{"providers": [{"id": "a.b", "tiers": {"strong": "dotted"}},
                          {"id": "a_b", "tiers": {"strong": "underscored"}},
                          {"tiers": {"strong": "anonymous"}}]}"#,
    );
    let ids: Vec<&str> = settings
        .values
        .providers
        .iter()
        .map(|p| p.id.as_str())
        .collect();
    assert_eq!(ids, ["a_b"]);
    assert_eq!(ignored_keys(&settings), ["providers[0]", "providers[2]"]);
    assert!(
        settings.ignored[0].error.contains("`a.b`"),
        "{:?}",
        settings.ignored
    );
}

#[test]
fn missing_tiers_are_filled_from_the_tier_above() {
    let settings = load_global(
        r#"{"providers": [{"id": "ds", "models": ["pro", "flash"],
                           "tiers": {"strong": "pro", "balanced": "flash"}}],
            "model": {"tasks": {"main": "fast"}}}"#,
    );
    let routing = settings.values.routing().unwrap();
    let (provider, tiers) = &routing[0];
    assert_eq!(provider.id, "ds");
    assert_eq!(
        (
            tiers.strong.as_str(),
            tiers.balanced.as_str(),
            tiers.fast.as_str()
        ),
        ("pro", "flash", "flash")
    );
    assert_eq!(settings.values.model.model_name, "flash");
}

#[test]
fn a_tier_outside_the_models_or_an_order_naming_no_provider_does_not_route() {
    let settings = load_global(
        r#"{"providers": [{"id": "ds", "models": ["pro"], "tiers": {"strong": "max"}}]}"#,
    );
    let err = settings.values.routing().unwrap_err();
    assert!(err.contains("tiers.strong") && err.contains("max"), "{err}");
    assert_eq!(settings.values.model.model_name, "m");

    let settings = load_global(
        r#"{"providers": [{"id": "ds", "tiers": {"strong": "pro"}}],
            "model": {"order": ["ds", "claude"]}}"#,
    );
    let err = settings.values.routing().unwrap_err();
    assert!(err.contains("`claude`"), "{err}");
}

#[test]
fn without_providers_the_engine_supplies_anthropic_on_the_cli_model() {
    let settings = load("{}", "{}", "{}");
    let routing = settings.values.routing().unwrap();
    assert_eq!(routing.len(), 1);
    assert_eq!(routing[0].0.id, "anthropic");
    assert!(routing[0].0.builtin);
    assert_eq!(routing[0].1.fast, "cli-model");
    assert_eq!(settings.values.model.model_name, "cli-model");
    assert!(settings.provider_origins.is_empty());
}

#[test]
fn an_old_key_is_reported_with_the_key_that_replaced_it() {
    let settings = load_global(
        r#"{"model": {"model_name": "x", "thinking_mode": "off", "max_tokens": 100},
            "providers": [{"id": "ds", "api_type": "openai_compatible", "tiers": {"strong": "p"}},
                          {"id": "ok", "tiers": {"strong": "q"}}]}"#,
    );
    let mut ignored = ignored_keys(&settings);
    ignored.sort();
    assert_eq!(
        ignored,
        [
            "model.model_name",
            "model.thinking_mode",
            "providers[0].api_type"
        ]
    );
    let error = |key: &str| {
        settings
            .ignored
            .iter()
            .find(|i| i.key == key)
            .map(|i| i.error.clone())
            .unwrap()
    };
    assert!(error("providers[0].api_type").contains("renamed to `providers[].api`"));
    assert!(error("model.thinking_mode").contains("renamed to `model.thinking`"));
    assert!(error("model.model_name").contains("model.tasks.main"));
    assert_eq!(settings.values.model.max_tokens, 100);
    let ids: Vec<&str> = settings
        .values
        .providers
        .iter()
        .map(|p| p.id.as_str())
        .collect();
    assert_eq!(ids, ["ok"], "an entry with an old key is not used");

    let keyed = load_global(r#"{"providers": {"ds": {"api": "openai_chat"}}}"#);
    assert_eq!(ignored_keys(&keyed), ["providers"]);
    assert!(keyed.ignored[0].error.contains("list"));
}
