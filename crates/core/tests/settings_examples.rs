//! The settings JSON Schema under `docs/schemas` accepts the documents the
//! settings loader accepts — an empty file, the README's examples — and
//! refuses a permission mode the protocol says a settings file cannot hold.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};
use std::path::Path;

/// `settings_json_schema` 的注释说这份 schema "can never drift from what `LoadedSettings::load()`
/// actually accepts",README 让用户拿它做编辑器校验。可它要求顶层的 `model`、`paths`,
/// `sandbox` 要求 `deny_read`:空文件、README 的路由示例、`{"sandbox": {"unattended_needs_network_policy": true}}` 都过不了校验。`bubble` 则反过来:
/// schema 收,协议说写进 settings.json 会被退回 `default`。
#[test]
fn the_settings_schema_accepts_what_the_loader_accepts() {
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/schemas/settings.schema.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let examples = [
        json!({}),
        json!({"sandbox": {"unattended_needs_network_policy": true}}),
        json!({"turn": {"max_model_calls": 10}}),
        json!({
            "providers": {
                "anthropic": { "api_type": "anthropic", "default_model": "claude-sonnet-4-6" }
            },
            "default_provider": "anthropic"
        }),
    ];
    let mut refused = Vec::new();
    for example in &examples {
        let mut missing = Vec::new();
        missing_required(&schema, &schema, example, "", &mut missing);
        if !missing.is_empty() {
            refused.push(format!("{example} lacks {missing:?}"));
        }
    }
    let modes = schema["$defs"]["PermissionMode"].to_string();
    if modes.contains("\"bubble\"") {
        refused.push("PermissionMode admits \"bubble\"".into());
    }
    assert!(refused.is_empty(), "{refused:#?}");
}

fn resolve<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    match node["$ref"].as_str() {
        Some(r) => resolve(root, &root["$defs"][r.trim_start_matches("#/$defs/")]),
        None => node,
    }
}

fn missing_required(root: &Value, node: &Value, value: &Value, at: &str, out: &mut Vec<String>) {
    let node = resolve(root, node);
    if let Some(options) = node["anyOf"].as_array().or(node["oneOf"].as_array()) {
        let mut best: Option<Vec<String>> = None;
        for option in options {
            let mut found = Vec::new();
            missing_required(root, option, value, at, &mut found);
            if best.as_ref().is_none_or(|b| found.len() < b.len()) {
                best = Some(found);
            }
        }
        out.extend(best.unwrap_or_default());
        return;
    }
    let Some(object) = value.as_object() else {
        return;
    };
    for key in node["required"].as_array().into_iter().flatten() {
        let key = key.as_str().unwrap();
        if !object.contains_key(key) {
            out.push(format!("{at}/{key}"));
        }
    }
    for (key, child) in object {
        let schema = match node["properties"].get(key) {
            Some(s) => s,
            None => match node.get("additionalProperties") {
                Some(s) if s.is_object() => s,
                _ => continue,
            },
        };
        missing_required(root, schema, child, &format!("{at}/{key}"), out);
    }
}
