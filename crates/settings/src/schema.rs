//! The JSON Schema of a settings.json file and the list of keys a layer may
//! hold, derived from the settings types and the registered [`Sections`].

use serde_json::Value;

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
pub fn settings_json_schema(sections: &Sections) -> Value {
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
fn optional_where_defaulted(schema: &mut Value, defaults: &Value) {
    let mut found = Vec::new();
    defaulted_objects(schema, "", defaults, &mut found);
    for (pointer, defaulted) in found {
        let Some(object) = schema.pointer_mut(&pointer).and_then(Value::as_object_mut) else {
            continue;
        };
        if let Some(Value::Array(required)) = object.get_mut("required") {
            required.retain(|key| key.as_str().is_none_or(|key| !defaulted.contains_key(key)));
            if required.is_empty() {
                object.remove("required");
            }
        }
    }
}

fn defaulted_objects(
    root: &Value,
    pointer: &str,
    defaults: &Value,
    out: &mut Vec<(String, serde_json::Map<String, Value>)>,
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
fn admit_permission_mode_aliases(schema: &mut Value) {
    let variants = schema
        .pointer_mut("/$defs/PermissionMode/oneOf")
        .and_then(Value::as_array_mut);
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
pub fn unknown_settings_keys(layer: &Value, sections: &Sections) -> Vec<String> {
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

/// The settings keys a file can hold, read off the published schema:
/// an object with properties, or a map, is looked into; anything else —
/// including a value that may be an object or something else — is one key.
pub(crate) fn schema_keys(root: &Value) -> Vec<Vec<String>> {
    fn resolve<'a>(root: &'a Value, mut node: &'a Value) -> &'a Value {
        while let Some(target) = node.get("$ref").and_then(Value::as_str) {
            let Some(target) = root.pointer(target.trim_start_matches('#')) else {
                break;
            };
            node = target;
        }
        node
    }
    fn walk(root: &Value, node: &Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        let node = resolve(root, node);
        let branches: Vec<&Value> = ["anyOf", "oneOf", "allOf"]
            .iter()
            .flat_map(|b| node.get(*b).and_then(Value::as_array).into_iter().flatten())
            .map(|b| resolve(root, b))
            .filter(|b| b.get("type") != Some(&Value::from("null")))
            .collect();
        let alternatives = if branches.is_empty() {
            vec![node]
        } else {
            branches
        };
        let is_section =
            |a: &&Value| a.get("properties").is_some() || a.get("additionalProperties").is_some();
        if !alternatives.iter().all(is_section) {
            out.push(path.clone());
            return;
        }
        for alternative in alternatives {
            for (key, inner) in alternative
                .get("properties")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                path.push(key.clone());
                walk(root, inner, path, out);
                path.pop();
            }
            match alternative.get("additionalProperties") {
                Some(inner @ Value::Object(_)) => {
                    path.push("*".into());
                    walk(root, inner, path, out);
                    path.pop();
                }
                Some(Value::Bool(true)) => {
                    path.push("*".into());
                    out.push(path.clone());
                    path.pop();
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut Vec::new(), &mut out);
    out.sort();
    out.dedup();
    out
}

/// What a dotted key names in the settings schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Unknown,
    /// An object whose keys are settings of their own.
    Section,
    /// One setting, written whole.
    Value,
}

/// The settings keys of the schema with the registered sections, to look a
/// key up in.
pub struct SchemaKeys(Vec<Vec<String>>);

impl SchemaKeys {
    pub fn of(sections: &Sections) -> Self {
        Self(schema_keys(&settings_json_schema(sections)))
    }

    pub fn kind(&self, path: &[&str]) -> KeyKind {
        let agrees = |key: &[String]| key.iter().zip(path).all(|(k, p)| k == "*" || k == p);
        let mut kind = KeyKind::Unknown;
        for key in &self.0 {
            if key.len() < path.len() || !agrees(key) {
                continue;
            }
            if key.len() == path.len() {
                return KeyKind::Value;
            }
            kind = KeyKind::Section;
        }
        kind
    }

    /// The keys at or beneath `path` that no setting reads, written out as
    /// `value` would be: a section's object is looked into key by key.
    pub fn unknown_in(&self, path: &[&str], value: &Value) -> Vec<String> {
        match self.kind(path) {
            KeyKind::Unknown => vec![path.join(".")],
            KeyKind::Value => Vec::new(),
            KeyKind::Section => value
                .as_object()
                .into_iter()
                .flatten()
                .flat_map(|(key, inner)| {
                    let mut beneath = path.to_vec();
                    beneath.push(key);
                    self.unknown_in(&beneath, inner)
                })
                .collect(),
        }
    }

    /// Whether some setting is at `path` or holds it whole.
    pub fn holds(&self, path: &[&str]) -> bool {
        self.0.iter().any(|key| {
            key.len() <= path.len() && key.iter().zip(path).all(|(k, p)| k == "*" || k == p)
        })
    }
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
