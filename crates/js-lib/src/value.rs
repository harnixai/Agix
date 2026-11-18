//! The JSON values that cross between JavaScript and Rust, with object keys
//! in insertion order. Both directions go through JSON text: JavaScript's own
//! `JSON.parse` / `JSON.stringify` on its side, this module on the Rust side.

use std::fmt;
use std::rc::Rc;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

/// serde_json's own limit on the way in, so both directions agree.
pub(crate) const MAX_DEPTH: usize = 127;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
    /// The node an alias names, held once however often it is named.
    Shared(Rc<Value>),
}

impl Value {
    pub(crate) fn resolved(&self) -> &Value {
        match self {
            Value::Shared(v) => v.resolved(),
            other => other,
        }
    }

    pub(crate) fn to_json(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }

    fn write_json(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&number_text(*n)),
            Value::String(s) => push_json_string(out, s),
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_json(out);
                }
                out.push(']');
            }
            Value::Object(entries) => {
                out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    push_json_string(out, k);
                    out.push(':');
                    v.write_json(out);
                }
                out.push('}');
            }
            Value::Shared(v) => v.write_json(out),
        }
    }

    pub(crate) fn from_json(text: &str) -> Result<Value, serde_json::Error> {
        let mut de = serde_json::Deserializer::from_str(text);
        let value = de.deserialize_any(ValueVisitor)?;
        de.end()?;
        Ok(value)
    }
}

const SAFE_INTEGER: f64 = 9_007_199_254_740_992.0;

pub(crate) fn number_text(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() <= SAFE_INTEGER {
        format!("{}", n as i64)
    } else {
        serde_json::Number::from_f64(n)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "null".into())
    }
}

fn push_json_string(out: &mut String, s: &str) {
    out.push_str(&serde_json::Value::String(s.to_string()).to_string());
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_bool<E>(self, b: bool) -> Result<Value, E> {
        Ok(Value::Bool(b))
    }

    fn visit_i64<E>(self, n: i64) -> Result<Value, E> {
        Ok(Value::Number(n as f64))
    }

    fn visit_u64<E>(self, n: u64) -> Result<Value, E> {
        Ok(Value::Number(n as f64))
    }

    fn visit_f64<E>(self, n: f64) -> Result<Value, E> {
        Ok(Value::Number(n))
    }

    fn visit_str<E>(self, s: &str) -> Result<Value, E> {
        Ok(Value::String(s.to_string()))
    }

    fn visit_string<E>(self, s: String) -> Result<Value, E> {
        Ok(Value::String(s))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(Seed)? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut entries: Vec<(String, Value)> = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            entries.push((key, map.next_value_seed(Seed)?));
        }
        Ok(Value::Object(entries))
    }
}

struct Seed;

impl<'de> de::DeserializeSeed<'de> for Seed {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(ValueVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_text_keeps_key_order_both_ways() {
        let v = Value::from_json(r#"{"z":1,"a":[true,null,"x"],"m":{"b":2.5,"a":-0}}"#).unwrap();
        assert_eq!(
            v.to_json(),
            r#"{"z":1,"a":[true,null,"x"],"m":{"b":2.5,"a":0}}"#
        );
    }

    #[test]
    fn numbers_beyond_the_safe_range_keep_their_value() {
        assert_eq!(number_text(1e21), "1e+21");
        assert_eq!(number_text(0.1), "0.1");
        assert_eq!(number_text(-3.0), "-3");
    }

    #[test]
    fn nesting_past_the_limit_is_refused() {
        let deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
        assert!(Value::from_json(&deep).is_err());
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(Value::from_json(&ok).is_ok());
    }
}
