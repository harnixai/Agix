//! The value of `settings.prompt.append`: one text, or a list of named
//! blocks, so a report can say whose each block is; and the prompt blocks
//! either becomes. A list's blocks are named for the layer that gave it — the
//! host's (`host.<name>`) only when the host gave it, a settings file's
//! (`config.<name>`) otherwise — so a cloned project cannot pass its text off
//! as a host extension's. A list that does not hold together does not read
//! at all.

use std::collections::HashSet;

use base::interface::config_source::Tier;
use base::prompt::{names, BlockOrigin, PromptBlock};
use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
#[non_exhaustive]
pub enum PromptAppend {
    /// One block, `config.prompt_append`.
    Text(String),
    /// One block per entry, in this order: `host.<name>` when the host gave
    /// the list, `config.<name>` when a settings file did.
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
    /// The system blocks this appends when `by` gave it, in order: one
    /// `config.prompt_append` for a text; for a list, one block per entry,
    /// `host.<name>` / `{"host": name}` when `by` is a layer the host sets (a
    /// session's own, the `cli` layer), `config.<name>` / `{"config": name}`
    /// when it is a settings file or the managed layer.
    pub fn blocks(&self, by: Tier) -> Vec<PromptBlock> {
        match self {
            Self::Text(text) => vec![PromptBlock::system(text.clone())
                .named(names::CONFIG_PROMPT_APPEND)
                .from_origin(BlockOrigin::Config)],
            Self::Blocks(blocks) => blocks
                .iter()
                .map(|block| {
                    let name = block.name.clone();
                    let (prefix, origin) = if set_by_host(by) {
                        (names::HOST_PREFIX, BlockOrigin::Host(name))
                    } else {
                        (
                            names::CONFIG_PREFIX,
                            BlockOrigin::ConfigEntry { config: name },
                        )
                    };
                    PromptBlock::system(block.text.clone())
                        .named(format!("{prefix}{}", block.name))
                        .from_origin(origin)
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
            if RESERVED.contains(&name.as_str()) {
                return Err(format!(
                    "[{i}].name {name:?} is reserved: `config.{name}` is the engine's own block"
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

/// Entry names that would read as the text form's or the override's block
/// once prefixed with `config.`.
const RESERVED: [&str; 2] = ["prompt_append", "prompt_override"];

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-')
}

fn set_by_host(tier: Tier) -> bool {
    matches!(tier, Tier::Session | Tier::Cli)
}

/// `prompt.append` of a settings layer, as JSON.
pub(crate) fn append_of(layer: &Value) -> Option<&Value> {
    layer.get("prompt")?.get("append")
}

/// Whether merging `given` — a layer's `prompt.append` — left it the value
/// `merged` holds: a value that did not fit was not merged, and does not win.
pub(crate) fn took(given: Option<&Value>, merged: &Value) -> bool {
    given.is_some() && given == append_of(merged)
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
    fn a_list_is_named_for_the_host_only_when_a_host_layer_gave_it() {
        let list = read(json!([{"name": "harnix:review", "text": "x"}])).unwrap();
        let named = |by| {
            let block = list.blocks(by).remove(0);
            (block.name.unwrap(), block.origin)
        };
        for by in [Tier::Session, Tier::Cli] {
            assert_eq!(
                named(by),
                (
                    "host.harnix:review".to_string(),
                    BlockOrigin::Host("harnix:review".into())
                ),
                "{by:?}"
            );
        }
        for by in [Tier::Global, Tier::Scene, Tier::Project, Tier::Managed] {
            assert_eq!(
                named(by),
                (
                    "config.harnix:review".to_string(),
                    BlockOrigin::ConfigEntry {
                        config: "harnix:review".into()
                    }
                ),
                "{by:?}"
            );
        }
        let text = PromptAppend::from("t").blocks(Tier::Session).remove(0);
        assert_eq!(
            (text.name.as_deref(), text.origin),
            (Some(names::CONFIG_PROMPT_APPEND), BlockOrigin::Config)
        );
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
            (
                json!([{"name": "prompt_override", "text": "x"}]),
                "[0].name \"prompt_override\" is reserved",
            ),
            (
                json!([{"name": "prompt_append", "text": "x"}]),
                "is reserved",
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
