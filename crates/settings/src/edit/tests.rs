use super::*;
use crate::section::Sections;
use serde_json::json;

fn files(shared: Value, local: Value) -> TierFiles {
    let object = |v: Value| match v {
        Value::Object(fields) => fields,
        _ => Map::new(),
    };
    TierFiles {
        shared: object(shared),
        local: object(local),
    }
}

fn key(dotted: &str) -> KeyPath {
    KeyPath::parse(dotted).unwrap()
}

#[test]
fn a_key_is_dotted_segments_without_wildcards_or_indexes() {
    assert_eq!(key("files.read_dirs").segments(), ["files", "read_dirs"]);
    for bad in [
        "",
        ".",
        "a..b",
        "a.",
        "providers.*.api_key",
        "permissions.allow[0]",
    ] {
        assert!(KeyPath::parse(bad).is_err(), "{bad:?}");
    }
    assert!(key("files").overlaps(&key("files.read_dirs")));
    assert!(!key("sandbox.allow_read").overlaps(&key("sandbox.allow_write")));
    assert_eq!(
        KeyPath::new(["mcp", "servers", "a.b"]).segments(),
        ["mcp", "servers", "a.b"]
    );
}

#[test]
fn set_replaces_the_tier_s_value_in_both_files() {
    let mut tier = files(
        json!({"files": {"read_dirs": ["a"], "extra_dirs": ["/x"]}}),
        json!({"files": {"read_dirs": ["local"]}}),
    );
    let local_changed = apply(
        &mut tier,
        &[EditOp::Set(key("files.read_dirs"), json!(["b"]))],
    );
    assert!(local_changed);
    assert_eq!(
        Value::Object(tier.shared),
        json!({"files": {"read_dirs": ["b"], "extra_dirs": ["/x"]}})
    );
    assert!(tier.local.is_empty(), "{:?}", tier.local);
}

#[test]
fn an_object_set_is_not_merged_with_what_the_tier_had() {
    let mut tier = files(json!({"files": {"extra_dirs": ["/x"]}}), json!({}));
    apply(
        &mut tier,
        &[EditOp::Set(key("files"), json!({"read_dirs": []}))],
    );
    assert_eq!(
        Value::Object(tier.shared),
        json!({"files": {"read_dirs": []}})
    );
}

#[test]
fn a_level_that_is_not_an_object_becomes_one() {
    let mut tier = files(json!({"model": "opus"}), json!({}));
    apply(
        &mut tier,
        &[EditOp::Set(key("model.model_name"), json!("x"))],
    );
    assert_eq!(
        Value::Object(tier.shared),
        json!({"model": {"model_name": "x"}})
    );
}

#[test]
fn remove_takes_the_key_from_both_files_and_prunes_what_it_empties() {
    let mut tier = files(
        json!({"recorder": "u", "files": {"read_dirs": ["a"]}}),
        json!({"files": {"read_dirs": ["b"]}, "hooks": {}}),
    );
    apply(
        &mut tier,
        &[
            EditOp::Remove(key("files.read_dirs")),
            EditOp::Remove(key("recorder")),
        ],
    );
    assert!(tier.shared.is_empty(), "{:?}", tier.shared);
    assert_eq!(Value::Object(tier.local), json!({"hooks": {}}));
}

#[test]
fn a_credential_goes_to_the_local_file_wherever_it_is_written() {
    let mut tier = files(json!({}), json!({}));
    let local_changed = apply(
        &mut tier,
        &[
            EditOp::Set(key("mcp.servers.s.env"), json!({"T": "t"})),
            EditOp::Set(
                key("providers"),
                json!({"p": {"api_key": "k", "base_url": "u"}}),
            ),
        ],
    );
    assert!(local_changed);
    assert_eq!(
        Value::Object(tier.shared),
        json!({"providers": {"p": {"base_url": "u"}}})
    );
    assert_eq!(
        Value::Object(tier.local),
        json!({"mcp": {"servers": {"s": {"env": {"T": "t"}}}}, "providers": {"p": {"api_key": "k"}}})
    );
}

#[test]
fn a_server_set_whole_replaces_its_local_credentials() {
    let path = KeyPath::new(["mcp", "servers", "gh"]);
    let mut tier = files(
        json!({"mcp": {"servers": {"gh": {"url": "old"}}}}),
        json!({"mcp": {"servers": {"gh": {"headers": {"A": "old"}, "env": {"X": "1"}}}}}),
    );
    apply(
        &mut tier,
        &[EditOp::Set(
            path.clone(),
            json!({"url": "new", "headers": {"A": "new"}}),
        )],
    );
    assert_eq!(
        Value::Object(tier.shared.clone()),
        json!({"mcp": {"servers": {"gh": {"url": "new"}}}})
    );
    assert_eq!(
        Value::Object(tier.local.clone()),
        json!({"mcp": {"servers": {"gh": {"headers": {"A": "new"}}}}})
    );

    apply(&mut tier, &[EditOp::Set(path, json!({"url": "plain"}))]);
    assert!(tier.local.is_empty(), "{:?}", tier.local);
}

#[test]
fn merge_keeps_the_local_overrides_and_moves_an_old_key_out() {
    let path = KeyPath::new(["providers", "p"]);
    let mut tier = files(
        json!({"providers": {"p": {"api_key": "stale", "base_url": "u"}}}),
        json!({"providers": {"p": {"default_model": "local-model"}}}),
    );
    let local_changed = apply(
        &mut tier,
        &[EditOp::Merge(path.clone(), json!({"default_model": "m"}))],
    );
    assert!(local_changed);
    assert_eq!(
        Value::Object(tier.shared.clone()),
        json!({"providers": {"p": {"base_url": "u", "default_model": "m"}}})
    );
    assert_eq!(
        Value::Object(tier.local.clone()),
        json!({"providers": {"p": {"default_model": "local-model", "api_key": "stale"}}})
    );

    apply(
        &mut tier,
        &[EditOp::Merge(path, json!({"api_key": "fresh"}))],
    );
    assert_eq!(tier.local["providers"]["p"]["api_key"], "fresh");
    assert!(tier.shared["providers"]["p"].get("api_key").is_none());
}

#[test]
fn a_moved_credential_does_not_replace_the_local_one() {
    let mut tier = files(
        json!({"providers": {"p": {"api_key": "old", "base_url": "u"}}}),
        json!({"providers": {"p": {"api_key": "current"}}}),
    );
    apply(
        &mut tier,
        &[EditOp::Merge(
            KeyPath::new(["providers", "p"]),
            json!({"base_url": "v"}),
        )],
    );
    assert_eq!(
        Value::Object(tier.shared),
        json!({"providers": {"p": {"base_url": "v"}}})
    );
    assert_eq!(tier.local["providers"]["p"]["api_key"], "current");
}

#[test]
fn the_concatenated_lists_are_edited_in_settings_json_only() {
    let mut tier = files(
        json!({"permissions": {"allow": ["Read"]}}),
        json!({"permissions": {"allow": ["Bash(ls)"]}}),
    );
    let rules = json!(["Edit"]);
    assert!(!apply(
        &mut tier,
        &[EditOp::Set(key("permissions.allow"), rules.clone())]
    ));
    assert_eq!(tier.shared["permissions"]["allow"], rules);
    assert_eq!(tier.local["permissions"]["allow"][0], "Bash(ls)");

    assert!(!apply(
        &mut tier,
        &[EditOp::Remove(key("permissions.allow"))]
    ));
    assert!(tier.shared.is_empty());
    assert_eq!(tier.local["permissions"]["allow"][0], "Bash(ls)");
}

/// "Allow every time" writes `settings.local.json`; setting or removing the
/// whole `permissions` block does not take those answers away.
#[test]
fn a_change_of_the_block_above_keeps_the_local_lists() {
    let mut tier = files(
        json!({"permissions": {"mode": "plan", "deny": ["Bash"]}}),
        json!({"permissions": {"allow_exact": ["Bash(ls)"], "mode": "dontAsk"}}),
    );
    apply(
        &mut tier,
        &[EditOp::Set(key("permissions"), json!({"mode": "default"}))],
    );
    assert_eq!(
        Value::Object(tier.shared.clone()),
        json!({"permissions": {"mode": "default"}})
    );
    assert_eq!(
        Value::Object(tier.local.clone()),
        json!({"permissions": {"allow_exact": ["Bash(ls)"]}})
    );

    apply(&mut tier, &[EditOp::Remove(key("permissions"))]);
    assert!(tier.shared.is_empty());
    assert_eq!(tier.local["permissions"]["allow_exact"][0], "Bash(ls)");
}

#[test]
fn declares_reads_either_file() {
    let tier = files(
        json!({"mcp": {"servers": {"a": {}}}}),
        json!({"mcp": {"servers": {"b": {}}}}),
    );
    assert!(tier.declares(&KeyPath::new(["mcp", "servers", "a"])));
    assert!(tier.declares(&KeyPath::new(["mcp", "servers", "b"])));
    assert!(!tier.declares(&KeyPath::new(["mcp", "servers", "c"])));
}

#[test]
fn a_revision_follows_both_files_bytes() {
    let r = revision(b"{}", b"");
    assert!(r.starts_with("r1:"), "{r}");
    assert_eq!(r, revision(b"{}", b""));
    assert_ne!(r, revision(b"{ }", b""));
    assert_ne!(r, revision(b"", b"{}"));
    assert_ne!(revision(b"ab", b"c"), revision(b"a", b"bc"));
}

#[test]
fn every_credential_path_is_a_settings_key() {
    let keys = crate::SchemaKeys::of(&Sections::none());
    for path in SECRET_PATHS.iter().chain(CONCATENATED_KEYS) {
        let segments: Vec<&str> = path.split('.').collect();
        assert!(keys.holds(&segments), "`{path}` is no settings key");
    }
}

#[test]
fn the_schema_says_what_a_key_names() {
    use crate::KeyKind;
    let keys = crate::SchemaKeys::of(&Sections::none());
    assert_eq!(keys.kind(&["sandbox"]), KeyKind::Section);
    assert_eq!(keys.kind(&["sandbox", "allow_read"]), KeyKind::Value);
    assert_eq!(keys.kind(&["sandbox", "network"]), KeyKind::Section);
    assert_eq!(
        keys.kind(&["mcp", "servers", "gh", "enabled"]),
        KeyKind::Value
    );
    assert_eq!(
        keys.kind(&["mcp", "servers", "gh", "command"]),
        KeyKind::Value
    );
    assert_eq!(
        keys.kind(&["sandbox", "cache_dirs", "python"]),
        KeyKind::Value
    );
    assert_eq!(keys.kind(&["sandbox", "nope"]), KeyKind::Unknown);
    assert_eq!(keys.kind(&["paths"]), KeyKind::Unknown);
    assert_eq!(
        keys.unknown_in(&["sandbox"], &json!({"allow_read": [], "nope": 1})),
        ["sandbox.nope"]
    );
    assert!(keys
        .unknown_in(&["providers"], &json!({"p": {"base_url": "u"}}))
        .is_empty());
}

#[test]
fn set_keeping_replaces_the_rest_and_keeps_what_it_names() {
    let path = KeyPath::new(["mcp", "servers", "gh"]);
    let mut tier = files(
        json!({"mcp": {"servers": {"gh": {"url": "old", "enabled": false}}}}),
        json!({"mcp": {"servers": {"gh": {"headers": {"A": "old"}}}}}),
    );
    assert!(tier.declares_besides(&path, "enabled"));
    apply(
        &mut tier,
        &[EditOp::SetKeeping(
            path.clone(),
            json!({"command": "gh", "env": {"T": "t"}}),
            vec!["enabled".into()],
        )],
    );
    assert_eq!(
        Value::Object(tier.shared.clone()),
        json!({"mcp": {"servers": {"gh": {"command": "gh", "enabled": false}}}})
    );
    assert_eq!(
        Value::Object(tier.local.clone()),
        json!({"mcp": {"servers": {"gh": {"env": {"T": "t"}}}}})
    );

    let only_a_switch = files(
        json!({"mcp": {"servers": {"gh": {"enabled": false}}}}),
        json!({}),
    );
    assert!(only_a_switch.declares(&path));
    assert!(!only_a_switch.declares_besides(&path, "enabled"));
}
