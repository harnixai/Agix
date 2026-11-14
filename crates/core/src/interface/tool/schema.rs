//! `compact_schema`: the tool schema as the model reads it.

use super::*;

/// `schema` as sent to the model: the same parameters, without what schema
/// generators add for other readers — the root `$schema` and `title`,
/// `"default": null`, Rust integer `format`s, and per-variant descriptions on
/// an enum of string constants (which becomes a plain `enum`).
///
/// Only what the model reads changes; inputs are still parsed against the
/// tool's own types.
pub fn compact_schema(mut schema: Value) -> Value {
    if let Some(root) = schema.as_object_mut() {
        root.remove("$schema");
        root.remove("title");
    }
    compact_node(&mut schema);
    schema
}

fn compact_node(node: &mut Value) {
    match node {
        Value::Object(map) => {
            if map.get("default").is_some_and(Value::is_null) {
                map.remove("default");
            }
            if map
                .get("format")
                .and_then(Value::as_str)
                .is_some_and(is_numeric_format)
            {
                map.remove("format");
            }
            if let Some(constants) = map.get("oneOf").and_then(string_constants) {
                map.remove("oneOf");
                map.insert("type".into(), Value::String("string".into()));
                map.insert("enum".into(), Value::Array(constants));
            }
            for child in map.values_mut() {
                compact_node(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(compact_node),
        _ => {}
    }
}

fn is_numeric_format(format: &str) -> bool {
    matches!(
        format,
        "uint"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "float"
            | "double"
    )
}

/// The constants of a `oneOf` whose every branch is a string `const`,
/// optionally typed and described — nothing else may be lost by flattening.
fn string_constants(one_of: &Value) -> Option<Vec<Value>> {
    let branches = one_of.as_array()?;
    if branches.is_empty() {
        return None;
    }
    branches
        .iter()
        .map(|branch| {
            let branch = branch.as_object()?;
            let constant = branch.get("const").filter(|c| c.is_string())?;
            let only_known_keys = branch.keys().all(|k| {
                k == "const" || k == "description" || (k == "type" && branch[k] == "string")
            });
            only_known_keys.then(|| constant.clone())
        })
        .collect()
}

#[cfg(test)]
mod compact_schema_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drops_generator_noise_and_keeps_every_parameter() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "BashInput",
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": {"type": "string", "description": "The command."},
                "timeout_ms": {"type": ["integer", "null"], "default": null,
                               "format": "uint64", "minimum": 0},
                "url": {"type": "string", "format": "uri"},
                "flag": {"type": "boolean", "default": false}
            }
        });
        assert_eq!(
            compact_schema(schema),
            json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": {"type": "string", "description": "The command."},
                    "timeout_ms": {"type": ["integer", "null"], "minimum": 0},
                    "url": {"type": "string", "format": "uri"},
                    "flag": {"type": "boolean", "default": false}
                }
            })
        );
    }

    #[test]
    fn an_enum_of_described_constants_becomes_a_plain_enum() {
        let schema = json!({"$defs": {"Mode": {
            "description": "Permission mode.",
            "oneOf": [
                {"const": "default", "type": "string", "description": "Ask."},
                {"const": "plan", "type": "string", "description": "Read-only."}
            ]
        }}});
        assert_eq!(
            compact_schema(schema),
            json!({"$defs": {"Mode": {
                "description": "Permission mode.",
                "type": "string",
                "enum": ["default", "plan"]
            }}})
        );
    }

    #[test]
    fn a_oneof_that_is_not_only_constants_is_left_alone() {
        let one_of = json!([
            {"const": "auto", "type": "string"},
            {"type": "object", "properties": {"n": {"type": "integer"}}}
        ]);
        let compacted = compact_schema(json!({"oneOf": one_of.clone()}));
        assert_eq!(compacted["oneOf"], one_of);
    }
}
