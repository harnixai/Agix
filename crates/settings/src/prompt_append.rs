//! The value of `settings.prompt.append`: one text, or a list of blocks each
//! named by the host that gave it, so a report can say whose each block is;
//! and the prompt blocks either becomes. A list that does not hold together
//! does not read at all.

use std::collections::HashSet;

use base::prompt::{names, BlockOrigin, PromptBlock};
use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
#[non_exhaustive]
pub enum PromptAppend {
    /// One block, `config.prompt_append`.
    Text(String),
    /// One block per entry, `host.<name>`, in this order.
    Blocks(Vec<AppendBlock>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AppendBlock {
    /// Unique in the list; letters, digits and `_ . : -`.
    #[schemars(pattern(r"^[A-Za-z0-9_.:-]+$"))]
    pub name: String,
    #[schemars(length(min = 1))]
    pub text: String,
}

impl AppendBlock {
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            text: text.into(),
        }
    }
}

impl From<&str> for PromptAppend {
    fn from(text: &str) -> Self {
        Self::Text(text.to_string())
    }
}

impl From<String> for PromptAppend {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl PromptAppend {
    /// The system blocks this appends, in order: one `config.prompt_append`
    /// for a text, one `host.<name>` per entry of a list.
    pub fn blocks(&self) -> Vec<PromptBlock> {
        match self {
            Self::Text(text) => vec![PromptBlock::system(text.clone())
                .named(names::CONFIG_PROMPT_APPEND)
                .from_origin(BlockOrigin::Config)],
            Self::Blocks(blocks) => blocks
                .iter()
                .map(|block| {
                    PromptBlock::system(block.text.clone())
                        .named(format!("{}{}", names::HOST_PREFIX, block.name))
                        .from_origin(BlockOrigin::Host(block.name.clone()))
                })
                .collect(),
        }
    }

    /// Why `blocks` cannot be a `prompt.append`, naming the first entry that
    /// fails.
    pub fn check(blocks: &[AppendBlock]) -> Result<(), String> {
        let mut seen = HashSet::new();
        for (i, block) in blocks.iter().enumerate() {
            let name = &block.name;
            if name.is_empty() {
                return Err(format!("[{i}].name is empty"));
            }
            if let Some(c) = name.chars().find(|c| !name_char(*c)) {
                return Err(format!(
                    "[{i}].name {name:?} has {c:?}; a name is letters, digits and `_ . : -`"
                ));
            }
            if !seen.insert(name.as_str()) {
                return Err(format!("[{i}].name {name:?} is given twice"));
            }
            if block.text.is_empty() {
                return Err(format!("[{i}].text of {name:?} is empty"));
            }
        }
        Ok(())
    }
}

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-')
}

impl<'de> Deserialize<'de> for PromptAppend {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Shape;
        impl<'de> Visitor<'de> for Shape {
            type Value = PromptAppend;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(r#"a string, or a list of {"name", "text"} blocks"#)
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                Ok(PromptAppend::Text(text.to_string()))
            }

            fn visit_string<E: de::Error>(self, text: String) -> Result<Self::Value, E> {
                Ok(PromptAppend::Text(text))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut blocks = Vec::new();
                while let Some(block) = seq.next_element::<AppendBlock>()? {
                    blocks.push(block);
                }
                PromptAppend::check(&blocks).map_err(de::Error::custom)?;
                Ok(PromptAppend::Blocks(blocks))
            }
        }
        deserializer.deserialize_any(Shape)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(value: serde_json::Value) -> Result<PromptAppend, String> {
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    #[test]
    fn a_string_and_a_list_both_read_and_write_back_as_given() {
        for value in [
            json!("plain"),
            json!([{"name": "harnix:ext", "text": "x"}, {"name": "user_1.a-b", "text": "y"}]),
            json!([]),
        ] {
            let read = read(value.clone()).expect("reads");
            assert_eq!(serde_json::to_value(&read).unwrap(), value);
        }
    }

    #[test]
    fn a_list_that_does_not_hold_together_is_refused_with_the_entry() {
        for (value, says) in [
            (json!([{"name": "", "text": "x"}]), "[0].name is empty"),
            (
                json!([{"name": "a b", "text": "x"}]),
                "[0].name \"a b\" has ' '",
            ),
            (json!([{"name": "é", "text": "x"}]), "has 'é'"),
            (
                json!([{"name": "a", "text": "x"}, {"name": "a", "text": "y"}]),
                "[1].name \"a\" is given twice",
            ),
            (
                json!([{"name": "a", "text": ""}]),
                "[0].text of \"a\" is empty",
            ),
            (json!([{"name": "a"}]), "missing field `text`"),
            (
                json!([{"name": "a", "text": "x", "role": "user"}]),
                "unknown field",
            ),
            (json!(5), "a string, or a list"),
            (json!({"name": "a", "text": "x"}), "a string, or a list"),
        ] {
            let error = read(value.clone()).expect_err(&value.to_string());
            assert!(error.contains(says), "{value}: {error}");
        }
    }
}
