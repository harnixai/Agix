use super::*;
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
    assert_eq!(
        key("sandbox.allowed_domains").segments(),
        ["sandbox", "allowed_domains"]
    );
    for bad in [
        "",
        ".",
        "a..b",
        "a.",
        "providers.*.api_key",
        "permission_rules[0]",
    ] {
        assert!(KeyPath::parse(bad).is_err(), "{bad:?}");
    }
    assert!(key("sandbox").overlaps(&key("sandbox.allowed_domains")));
    assert!(!key("sandbox.allow_read").overlaps(&key("sandbox.allow_write")));
    assert_eq!(
        KeyPath::new(["mcp_servers", "a.b"]).segments(),
        ["mcp_servers", "a.b"]
    );
}

#[test]
fn set_replaces_the_tier_s_value_in_both_files() {
    let mut tier = files(
        json!({"sandbox": {"allowed_domains": ["a"], "read_anywhere": true}}),
        json!({"sandbox": {"allowed_domains": ["local"]}}),
    );
    let local_changed = apply(
        &mut tier,
        &[EditOp::Set(key("sandbox.allowed_domains"), json!(["b"]))],
    );
    assert!(local_changed);
    assert_eq!(
        Value::Object(tier.shared),
        json!({"sandbox": {"allowed_domains": ["b"], "read_anywhere": true}})
    );
    assert!(tier.local.is_empty(), "{:?}", tier.local);
}

#[test]
fn an_object_set_is_not_merged_with_what_the_tier_had() {
    let mut tier = files(json!({"sandbox": {"read_anywhere": true}}), json!({}));
    apply(
        &mut tier,
        &[EditOp::Set(key("sandbox"), json!({"allowed_domains": []}))],
    );
    assert_eq!(
        Value::Object(tier.shared),
        json!({"sandbox": {"allowed_domains": []}})
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
        json!({"telemetry_url": "u", "sandbox": {"allowed_domains": ["a"]}}),
        json!({"sandbox": {"allowed_domains": ["b"]}, "language": "x"}),
    );
    apply(
        &mut tier,
        &[
            EditOp::Remove(key("sandbox.allowed_domains")),
            EditOp::Remove(key("telemetry_url")),
        ],
    );
    assert!(tier.shared.is_empty(), "{:?}", tier.shared);
    assert_eq!(Value::Object(tier.local), json!({"language": "x"}));
}

#[test]
fn a_credential_goes_to_the_local_file_wherever_it_is_written() {
    let mut tier = files(json!({}), json!({}));
    let local_changed = apply(
        &mut tier,
        &[
            EditOp::Set(key("model.auth_token"), json!("t")),
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
        json!({"model": {"auth_token": "t"}, "providers": {"p": {"api_key": "k"}}})
    );
}

#[test]
fn a_server_set_whole_replaces_its_local_credentials() {
    let path = KeyPath::new(["mcp_servers", "gh"]);
    let mut tier = files(
        json!({"mcp_servers": {"gh": {"url": "old"}}}),
        json!({"mcp_servers": {"gh": {"headers": {"A": "old"}, "env": {"X": "1"}}}}),
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
        json!({"mcp_servers": {"gh": {"url": "new"}}})
    );
    assert_eq!(
        Value::Object(tier.local.clone()),
        json!({"mcp_servers": {"gh": {"headers": {"A": "new"}}}})
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
        json!({"permission_rules": [{"tool": "Read", "action": "allow"}]}),
        json!({"permission_rules": [{"tool": "Bash(ls)", "action": "allow"}]}),
    );
    let rules = json!([{"tool": "Edit", "action": "deny"}]);
    assert!(!apply(
        &mut tier,
        &[EditOp::Set(key("permission_rules"), rules.clone())]
    ));
    assert_eq!(tier.shared["permission_rules"], rules);
    assert_eq!(tier.local["permission_rules"][0]["tool"], "Bash(ls)");

    assert!(!apply(
        &mut tier,
        &[EditOp::Remove(key("permission_rules"))]
    ));
    assert!(tier.shared.is_empty());
    assert!(tier.local.contains_key("permission_rules"));
}

#[test]
fn declares_reads_either_file() {
    let tier = files(
        json!({"mcp_servers": {"a": {}}}),
        json!({"mcp_servers": {"b": {}}}),
    );
    assert!(tier.declares(&KeyPath::new(["mcp_servers", "a"])));
    assert!(tier.declares(&KeyPath::new(["mcp_servers", "b"])));
    assert!(!tier.declares(&KeyPath::new(["mcp_servers", "c"])));
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
fn a_value_is_judged_by_its_setting() {
    let sections = Sections::none();
    let base = serde_json::to_value(crate::Settings::defaults_for("m")).unwrap();
    assert!(fits(&base, &key("language"), &json!("zh-CN"), &sections).is_ok());
    assert!(fits(
        &base,
        &key("sandbox.network_mode"),
        &json!("open"),
        &sections
    )
    .is_err());
    assert!(fits(&base, &key("memory_enabled"), &json!("yes"), &sections).is_err());
    assert!(fits(&base, &key("permission_mode"), &json!("plan"), &sections).is_ok());
    assert!(fits(&base, &key("permission_mode"), &json!("bubble"), &sections).is_err());
    let rules = json!([{"tool": "Bash(rm", "action": "deny"}]);
    let refused = fits(&base, &key("permission_rules"), &rules, &sections).unwrap_err();
    assert!(refused.contains("permission_rules[0]"), "{refused}");
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
    assert_eq!(keys.kind(&["sandbox", "allowed_domains"]), KeyKind::Value);
    assert_eq!(
        keys.kind(&["sandbox", "cache_directories", "python"]),
        KeyKind::Value
    );
    assert_eq!(keys.kind(&["sandbox", "nope"]), KeyKind::Unknown);
    assert_eq!(keys.kind(&["paths"]), KeyKind::Unknown);
    assert_eq!(
        keys.unknown_in(&["sandbox"], &json!({"allowed_domains": [], "nope": 1})),
        ["sandbox.nope"]
    );
    assert!(keys
        .unknown_in(&["providers"], &json!({"p": {"base_url": "u"}}))
        .is_empty());
}
