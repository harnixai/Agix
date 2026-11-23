//! Default values, list-valued keys and the JSON merge rule between layers.

use super::*;

#[test]
fn default_turn_and_agents_settings() {
    let s = Settings::defaults_for("m");
    assert_eq!(s.turn.parallel_tools, 10);
    assert_eq!(s.turn.max_model_calls, 25);
    assert_eq!(s.agents.max_depth, 3);
    assert_eq!(s.agents.max_concurrent, 6);
    assert!(s.sandbox.enabled && s.skills.allow_shell && s.memory.enabled);
    assert!(s.app.system_proxy);
}

#[test]
fn listing_the_root_among_the_read_dirs_reads_anywhere() {
    let settings = load_global(r#"{"files": {"read_dirs": ["/", "/srv/docs"]}}"#);
    assert!(settings.values.files.reads_anywhere());
    assert_eq!(
        settings.values.files.read_dirs_listed().collect::<Vec<_>>(),
        [&PathBuf::from("/srv/docs")]
    );
    assert!(!load_global(r#"{"files": {"read_dirs": ["/srv"]}}"#)
        .values
        .files
        .reads_anywhere());
}

#[test]
fn instruction_files_are_a_list_a_higher_tier_replaces_whole() {
    let root = tempfile::tempdir().unwrap();
    let (global, scene) = (root.path().join("global"), root.path().join("scene"));
    write_settings(
        &global,
        r#"{"prompt": {"instruction_files": ["/a.md", "/b.md"]}}"#,
    );
    write_settings(&scene, r#"{"prompt": {"instruction_files": ["/c.md"]}}"#);
    let settings = LoadedSettings::load(
        global,
        scene,
        root.path().join("project"),
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    );
    assert_eq!(
        settings.values.prompt.instruction_files,
        [PathBuf::from("/c.md")]
    );
}

#[test]
fn merge_json_values_recurses_into_nested_objects() {
    let mut base = serde_json::json!({
        "mcp_servers": {
            "gh": { "command": "old", "args": ["serve"] }
        }
    });
    let over = serde_json::json!({
        "mcp_servers": {
            "gh": { "command": "new" }
        }
    });
    merge_json_values(&mut base, over);
    assert_eq!(base["mcp_servers"]["gh"]["command"], "new");
    // Untouched sibling field survives — this is the whole point of a
    // recursive merge over a whole-value replace.
    assert_eq!(base["mcp_servers"]["gh"]["args"][0], "serve");
}

#[test]
fn a_settings_file_replaces_one_cache_group_and_keeps_the_rest() {
    let mut merged = serde_json::to_value(Settings::defaults_for("m")).unwrap();
    merge_json_values(
        &mut merged,
        serde_json::json!({ "sandbox": { "cache_dirs": {
            "python": [], "zig": ["~/.cache/zig", "build-cache"]
        } } }),
    );
    let settings: Settings = serde_json::from_value(merged).unwrap();
    let groups = &settings.sandbox.cache_dirs.0;
    assert!(groups["python"].is_empty());
    assert_eq!(groups["node"], CacheDirectories::default().0["node"]);

    let dirs = settings.sandbox.cache_dirs.paths(Path::new("/work"));
    assert!(dirs.contains(&PathBuf::from("~/.cache/zig")));
    assert!(dirs.contains(&PathBuf::from("/work/build-cache")));
    assert!(dirs.contains(&PathBuf::from("~/.npm")));
    assert!(!dirs.iter().any(|d| d.ends_with("pip")));
}

#[test]
fn merge_json_values_non_object_values_fully_replace() {
    let mut base = serde_json::json!({ "servers": {"a": 1} });
    let over = serde_json::json!({ "servers": {"b": 2} });
    merge_json_values(&mut base, over);
    // Both are objects, so this recurses (keeps "a", adds "b") — objects
    // always merge key-by-key, never fully replace each other.
    assert_eq!(base["servers"]["a"], 1);
    assert_eq!(base["servers"]["b"], 2);

    let mut base2 = serde_json::json!({ "language": "en" });
    let over2 = serde_json::json!({ "language": "zh-CN" });
    merge_json_values(&mut base2, over2);
    assert_eq!(base2["language"], "zh-CN");
}
