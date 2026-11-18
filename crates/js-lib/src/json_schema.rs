//! `agix.json.validate`: JSON Schema through the workspace's `jsonschema`,
//! with every reference outside the schema refused rather than fetched, and
//! a schema whose validation would cost too much refused before it starts:
//! the validator is native code, which no script time limit interrupts.

use jsonschema::error::ValidationErrorKind;
use jsonschema::{ReferencingError, Retrieve, Uri};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::Failure;

const MAX_ERRORS: usize = 100;

/// How many times validating may apply a subschema to a part of the value.
pub(crate) const MAX_APPLICATIONS: u64 = 1_000_000;

/// How long a chain of subschemas reached one from another may be.
const MAX_CHAIN: usize = 1_024;

struct NoRetrieval;

impl Retrieve for NoRetrieval {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(format!("{uri} is outside the schema").into())
    }
}

pub(crate) fn validate(schema: &Value, value: &Value) -> Result<Value, Failure> {
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .with_retriever(NoRetrieval)
        .build(schema)
        .map_err(|e| match e.kind() {
            ValidationErrorKind::Referencing(ReferencingError::Unretrievable { uri, .. }) => {
                Failure::new(
                    "json.external_ref",
                    format!(
                        "$ref to {uri}: only references inside the schema are resolved; \
                         nothing is fetched or read"
                    ),
                )
            }
            _ => Failure::new("json.schema", format!("the schema is not valid: {e}")),
        })?;
    Cost::of(schema).walk(schema, value, 0).map_err(|()| {
        Failure::new(
            "json.too_complex",
            format!(
                "validating this would apply the schema's parts more than {} times; a \
                 recursive schema with branches that recurse on the same part of the value \
                 grows this exponentially",
                group(MAX_APPLICATIONS)
            ),
        )
    })?;
    let errors: Vec<Value> = validator
        .iter_errors(value)
        .take(MAX_ERRORS)
        .map(|e| {
            json!({
                "instancePath": e.instance_path().as_str(),
                "schemaPath": e.schema_path().as_str(),
                "message": e.to_string(),
            })
        })
        .collect();
    Ok(json!({ "valid": errors.is_empty(), "errors": errors }))
}

fn percent_decoded(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// What validating costs, counted by walking the schema over the value the
/// way the validator does, without checking anything. Every applicator
/// counts, each branch of `anyOf` and every pattern of `patternProperties`
/// included, so the count is at least what the validator spends.
struct Cost<'s> {
    ids: HashMap<&'s str, &'s Value>,
    anchors: HashMap<&'s str, &'s Value>,
    resource_of: HashMap<*const Value, &'s Value>,
    left: u64,
}

const SAME_PLACE_LISTS: [&str; 3] = ["allOf", "anyOf", "oneOf"];
const SAME_PLACE: [&str; 4] = ["not", "if", "then", "else"];
const REFERENCES: [&str; 3] = ["$ref", "$dynamicRef", "$recursiveRef"];

impl<'s> Cost<'s> {
    fn of(root: &'s Value) -> Self {
        let mut cost = Cost {
            ids: HashMap::new(),
            anchors: HashMap::new(),
            resource_of: HashMap::new(),
            left: MAX_APPLICATIONS,
        };
        cost.index(root, root);
        cost
    }

    fn index(&mut self, node: &'s Value, resource: &'s Value) {
        let resource = match node.get("$id").and_then(Value::as_str) {
            Some(id) => {
                self.ids.insert(id, node);
                node
            }
            None => resource,
        };
        self.resource_of.insert(node as *const Value, resource);
        match node {
            Value::Object(map) => {
                for key in ["$anchor", "$dynamicAnchor"] {
                    if let Some(name) = map.get(key).and_then(Value::as_str) {
                        self.anchors.insert(name, node);
                    }
                }
                for child in map.values() {
                    self.index(child, resource);
                }
            }
            Value::Array(items) => {
                for child in items {
                    self.index(child, resource);
                }
            }
            _ => {}
        }
    }

    /// Where `reference`, written in `from`, leads; `None` for what this
    /// does not resolve, which the validator has already resolved or refused.
    fn resolve(&self, reference: &str, from: &'s Value) -> Option<&'s Value> {
        let (uri, fragment) = reference.split_once('#').unwrap_or((reference, ""));
        let resource = if uri.is_empty() {
            *self.resource_of.get(&(from as *const Value))?
        } else {
            let suffix = format!("/{uri}");
            self.ids
                .iter()
                .find(|(id, _)| **id == uri || id.ends_with(&suffix))
                .map(|(_, node)| *node)?
        };
        if fragment.is_empty() {
            Some(resource)
        } else if fragment.starts_with('/') {
            resource.pointer(&percent_decoded(fragment)?)
        } else {
            self.anchors.get(fragment).copied()
        }
    }

    fn walk(&mut self, schema: &'s Value, value: &Value, chain: usize) -> Result<(), ()> {
        if self.left == 0 || chain > MAX_CHAIN {
            return Err(());
        }
        self.left -= 1;
        let Some(s) = schema.as_object() else {
            return Ok(());
        };
        let next = chain + 1;
        for key in SAME_PLACE_LISTS {
            for sub in s.get(key).and_then(Value::as_array).into_iter().flatten() {
                self.walk(sub, value, next)?;
            }
        }
        for key in SAME_PLACE {
            if let Some(sub) = s.get(key) {
                self.walk(sub, value, next)?;
            }
        }
        for key in REFERENCES {
            let target = s
                .get(key)
                .and_then(Value::as_str)
                .and_then(|r| self.resolve(r, schema));
            if let Some(target) = target {
                self.walk(target, value, next)?;
            }
        }
        match value {
            Value::Array(items) => self.walk_items(s, items, next),
            Value::Object(props) => self.walk_properties(s, value, props, next),
            _ => Ok(()),
        }
    }

    fn walk_items(
        &mut self,
        s: &'s serde_json::Map<String, Value>,
        items: &[Value],
        next: usize,
    ) -> Result<(), ()> {
        let prefix = s
            .get("prefixItems")
            .or_else(|| s.get("items").filter(|i| i.is_array()))
            .and_then(Value::as_array);
        let rest = s
            .get("items")
            .filter(|i| !i.is_array())
            .or_else(|| s.get("additionalItems"));
        for (i, item) in items.iter().enumerate() {
            if let Some(sub) = prefix.and_then(|p| p.get(i)).or(rest) {
                self.walk(sub, item, next)?;
            }
            for key in ["contains", "unevaluatedItems"] {
                if let Some(sub) = s.get(key) {
                    self.walk(sub, item, next)?;
                }
            }
        }
        Ok(())
    }

    fn walk_properties(
        &mut self,
        s: &'s serde_json::Map<String, Value>,
        value: &Value,
        props: &serde_json::Map<String, Value>,
        next: usize,
    ) -> Result<(), ()> {
        let dependent = s.get("dependentSchemas").or_else(|| s.get("dependencies"));
        for (name, sub) in dependent.and_then(Value::as_object).into_iter().flatten() {
            if props.contains_key(name) && sub.is_object() {
                self.walk(sub, value, next)?;
            }
        }
        let patterns = s.get("patternProperties").and_then(Value::as_object);
        for (name, item) in props {
            let named = s.get("properties").and_then(|p| p.get(name));
            if let Some(sub) = named {
                self.walk(sub, item, next)?;
            }
            for sub in patterns.into_iter().flat_map(|p| p.values()) {
                self.walk(sub, item, next)?;
            }
            if named.is_none() {
                if let Some(sub) = s.get("additionalProperties") {
                    self.walk(sub, item, next)?;
                }
            }
            if let Some(sub) = s.get("unevaluatedProperties") {
                self.walk(sub, item, next)?;
            }
            if let Some(sub) = s.get("propertyNames") {
                self.walk(sub, &Value::Null, next)?;
            }
        }
        Ok(())
    }
}
