//! The JSON Schema of a settings.json file and the list of keys a layer may
//! hold, derived from the settings types and the registered [`Sections`].

use crate::section::Sections;
use crate::{Settings, SettingsFile};

/// JSON Schema for one settings.json file, published as
/// `docs/schemas/settings.schema.json` for editor autocomplete/validation
/// (reference it from a settings.json file via a `"$schema"` key).
///
/// Generated from [`SettingsFile`]'s type definitions, then adjusted to what
/// [`LoadedSettings::load`] does with a file: a file is merged over the
/// built-in defaults, so a field the defaults already supply is optional
/// however the type declares it ([`optional_where_defaulted`]); program-only
/// modes are left out. Regenerate with
/// `cargo test -p assembly --test settings_schema write_settings_schema -- --ignored`
/// after changing any `SettingsFile`-reachable type or a registered section.
///
/// The core types give the schema; each registered section's schema is added
/// under its key, with the section's definitions merged into `$defs`.
pub fn settings_json_schema(sections: &Sections) -> serde_json::Value {
    let mut generator = schemars::SchemaGenerator::default();
    let mut schema =
        serde_json::to_value(generator.root_schema_for::<SettingsFile>()).unwrap_or_default();
    let mut defaults = serde_json::to_value(SettingsFile {
        settings: Settings::defaults_for(""),
        permission_rules: Vec::new(),
    })
    .unwrap_or_default();
    for spec in sections.iter() {
        let mut property = serde_json::to_value(spec.schema(&mut generator)).unwrap_or_default();
        let default = spec.default_value();
        if let (Some(object), false) = (property.as_object_mut(), default.is_null()) {
            object.insert("default".into(), default.clone());
        }
        if let Some(properties) = schema["properties"].as_object_mut() {
            properties.insert(spec.key.to_string(), property);
        }
        if let Some(fields) = defaults.as_object_mut() {
            fields.insert(spec.key.to_string(), default);
        }
    }
    if let Some(definitions) = schema["$defs"].as_object_mut() {
        for (name, definition) in generator.definitions() {
            definitions
                .entry(name.clone())
                .or_insert_with(|| definition.clone());
        }
    }
    optional_where_defaulted(&mut schema, &defaults);
    admit_permission_mode_aliases(&mut schema);
    schema
}

/// Drop from each object schema's `required` the keys the loader's defaults
/// hold at that place, following `properties` from the root wherever the
/// defaults have an object. Entries of lists and maps are not merged with
/// anything, so their `required` stays.
fn optional_where_defaulted(schema: &mut serde_json::Value, defaults: &serde_json::Value) {
    let mut found = Vec::new();
    defaulted_objects(schema, "", defaults, &mut found);
    for (pointer, defaulted) in found {
        let Some(object) = schema
            .pointer_mut(&pointer)
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        if let Some(serde_json::Value::Array(required)) = object.get_mut("required") {
            required.retain(|key| key.as_str().is_none_or(|key| !defaulted.contains_key(key)));
            if required.is_empty() {
                object.remove("required");
            }
        }
    }
}

fn defaulted_objects(
    root: &serde_json::Value,
    pointer: &str,
    defaults: &serde_json::Value,
    out: &mut Vec<(String, serde_json::Map<String, serde_json::Value>)>,
) {
    let Some(defaulted) = defaults.as_object() else {
        return;
    };
    let Some(node) = root.pointer(pointer) else {
        return;
    };
    if let Some(target) = node["$ref"].as_str() {
        defaulted_objects(root, target.trim_start_matches('#'), defaults, out);
        return;
    }
    for branch in ["anyOf", "oneOf", "allOf"] {
        for index in 0..node[branch].as_array().map_or(0, Vec::len) {
            defaulted_objects(root, &format!("{pointer}/{branch}/{index}"), defaults, out);
        }
    }
    let Some(properties) = node["properties"].as_object() else {
        return;
    };
    for (key, value) in defaulted {
        if properties.contains_key(key) {
            defaulted_objects(root, &format!("{pointer}/properties/{key}"), value, out);
        }
    }
    out.push((pointer.to_string(), defaulted.clone()));
}

/// schemars renders each variant as a `const`, which has no room for serde's
/// `alias`: the spellings `Settings` accepts for `bypassPermissions` have to
/// be added by hand or editors flag them.
fn admit_permission_mode_aliases(schema: &mut serde_json::Value) {
    let variants = schema
        .pointer_mut("/$defs/PermissionMode/oneOf")
        .and_then(serde_json::Value::as_array_mut);
    for variant in variants.into_iter().flatten() {
        let Some(object) = variant.as_object_mut() else {
            continue;
        };
        if object.get("const") == Some(&serde_json::json!("bypassPermissions")) {
            object.remove("const");
            object.insert(
                "enum".into(),
                serde_json::json!(["bypassPermissions", "auto", "yolo"]),
            );
        }
    }
}

/// The top-level keys of one settings file that no setting reads — a typo,
/// or a key from another tool's settings. `$schema` is the editor's, and is
/// not reported.
pub fn unknown_settings_keys(layer: &serde_json::Value, sections: &Sections) -> Vec<String> {
    let known = known_core_keys();
    let Some(fields) = layer.as_object() else {
        return Vec::new();
    };
    fields
        .keys()
        .filter(|key| {
            key.as_str() != "$schema" && !known.contains(key) && sections.get(key).is_none()
        })
        .cloned()
        .collect()
}

/// The top-level keys the core settings types read.
pub(crate) fn core_keys() -> Vec<String> {
    settings_json_schema(&Sections::none())["properties"]
        .as_object()
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default()
}

pub(crate) fn known_core_keys() -> &'static [String] {
    static KNOWN: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(core_keys);
    &KNOWN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_json_schema_has_expected_top_level_properties() {
        let schema = settings_json_schema(&Sections::none());
        let props = schema["properties"]
            .as_object()
            .expect("schema should have a top-level `properties` object");
        for key in [
            "model",
            "providers",
            "default_provider",
            "task_models",
            "hooks_config",
            "permission_rules",
        ] {
            assert!(props.contains_key(key), "schema missing property `{key}`");
        }
        for key in [
            "paths",
            "session_dir",
            "local_permission_rules",
            "project_layer",
            "ignored_values",
            "managed",
            "mcp_server_dirs",
        ] {
            assert!(
                !props.contains_key(key),
                "`{key}` is not read from a file, so it is not in the file schema"
            );
        }
    }

    #[test]
    fn the_schema_requires_nothing_the_defaults_supply() {
        let schema = settings_json_schema(&Sections::none());
        assert!(schema.get("required").is_none(), "{}", schema["required"]);
        for def in ["ModelSettings", "ExecutionSettings", "SandboxConfig"] {
            assert!(
                schema["$defs"][def].get("required").is_none(),
                "{def}: {}",
                schema["$defs"][def]["required"]
            );
        }
        assert_eq!(
            schema["$defs"]["PermissionRuleEntry"]["required"],
            serde_json::json!(["tool", "action"]),
            "a list entry is not merged with defaults"
        );
        assert!(!schema["$defs"]["PermissionMode"]
            .to_string()
            .contains("\"bubble\""));
    }

    #[test]
    fn the_schema_admits_the_permission_mode_aliases() {
        let schema = settings_json_schema(&Sections::none());
        let admitted = schema.to_string();
        assert!(
            admitted.contains(r#"["bypassPermissions","auto","yolo"]"#),
            "{admitted}"
        );
    }
}
