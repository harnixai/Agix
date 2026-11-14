//! `TokenCounter` — how the engine decides how big a conversation is.
//!
//! Every budget decision rests on one number: how many tokens the context
//! currently holds. Compaction triggers on it, strategies choose what to drop
//! by it, and the warning shown to the user quotes it. That number came from
//! one hardcoded algorithm — a local character-class estimate standing in for
//! Anthropic's tokenizer, which nobody publishes — with no way for a host to
//! supply a better one.
//!
//! A better one is not hypothetical. A host talking to a provider with a
//! published tokenizer can count exactly. A host willing to spend a call on
//! `/v1/messages/count_tokens` can be exact for Anthropic too. Both are
//! strictly more accurate than an estimate documented to be off by up to ~12%, and
//! neither could be plugged in.
//!
//! # An estimate, not a measurement
//!
//! Implementations are not required to be exact, and callers must not treat
//! the result as if they were: it decides when to compact, not what is billed.
//! An implementation that is *cheap* and slightly wrong is a better default
//! than one that is exact and costs a network round-trip per turn, which is
//! why the shipped default is the local estimate.

use crate::interface::model::{ModelContentBlock, ModelMessage, IMAGE_TOKEN_ESTIMATE};
use std::sync::OnceLock;

/// Counts the tokens a piece of conversation will cost.
pub trait TokenCounter: Send + Sync {
    /// Tokens in a plain string.
    fn count_text(&self, text: &str) -> usize;

    /// Tokens in one content block, including whatever that block type costs
    /// beyond its text.
    fn count_block(&self, block: &ModelContentBlock) -> usize;

    /// Tokens in a conversation.
    ///
    /// Content only: per-message API framing (role, JSON punctuation) is a
    /// handful of tokens per message and is deliberately not modelled, so an
    /// implementation that ignores it agrees with the default.
    fn count_messages(&self, messages: &[ModelMessage]) -> usize {
        messages
            .iter()
            .flat_map(|m| &m.content)
            .map(|b| self.count_block(b))
            .sum()
    }
}

/// Charges a fixed number of tokens per block and per character.
///
/// The second implementation, and the one tests want: a budget test that has
/// to reach a compaction threshold should say so in one line rather than by
/// generating enough prose to trip a real tokenizer, and a test asserting
/// *which* strategy ran should not change its answer when the tokenizer's
/// table does.
pub struct FixedTokenCounter {
    pub per_char: usize,
    pub per_block: usize,
}

impl FixedTokenCounter {
    pub fn new(per_char: usize, per_block: usize) -> Self {
        Self {
            per_char,
            per_block,
        }
    }
}

impl TokenCounter for FixedTokenCounter {
    fn count_text(&self, text: &str) -> usize {
        text.len() * self.per_char
    }

    fn count_block(&self, block: &ModelContentBlock) -> usize {
        let text = match block {
            ModelContentBlock::Text { text } => text.len(),
            ModelContentBlock::Thinking { text, .. } => text.len(),
            ModelContentBlock::RedactedThinking { data, .. } => data.len(),
            ModelContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => tool_use_id.len() + content.len(),
            ModelContentBlock::ToolUse { id, name, input } => {
                id.len() + name.len() + input.to_string().len()
            }
            ModelContentBlock::Image { .. } => 0,
        };
        self.per_block + text * self.per_char
    }
}

const SHORT_WORD: usize = 12;
const CHARS_PER_WORD_TOKEN: usize = 5;
const DIGITS_PER_TOKEN: usize = 3;
const PUNCT_PER_TOKEN: usize = 3;

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F
        | 0x3040..=0x30FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xAC00..=0xD7AF
        | 0xF900..=0xFAFF
        | 0xFF00..=0xFFEF
        | 0x20000..=0x2FFFF)
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphabetic() || (!c.is_ascii() && c.is_alphabetic() && !is_cjk(c))
}

fn run_len(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    pred: impl Fn(char) -> bool,
) -> usize {
    let mut len = 1;
    while chars.next_if(|&d| pred(d)).is_some() {
        len += 1;
    }
    len
}

/// Estimated token count of `s`.
pub fn count_text(s: &str) -> usize {
    let mut n = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if is_word_char(c) {
            let mut len = 1;
            let mut ascii = c.is_ascii();
            while let Some(d) = chars.next_if(|&d| is_word_char(d)) {
                ascii &= d.is_ascii();
                len += 1;
            }
            n += match (ascii, len) {
                (true, l) if l <= SHORT_WORD => 1,
                (true, l) => l.div_ceil(CHARS_PER_WORD_TOKEN),
                (false, l) => l.div_ceil(2),
            };
        } else if c.is_ascii_digit() {
            n += run_len(&mut chars, |d| d.is_ascii_digit()).div_ceil(DIGITS_PER_TOKEN);
        } else if c == ' ' || c == '\t' {
            // cl100k folds spaces into the next token, indentation included.
            run_len(&mut chars, |d| d == ' ' || d == '\t');
        } else if c == '\n' || c == '\r' {
            run_len(&mut chars, |d| d == '\n' || d == '\r');
            n += 1;
        } else if is_cjk(c) {
            n += 1;
        } else if c.is_ascii() {
            n += run_len(&mut chars, |d| d.is_ascii_punctuation()).div_ceil(PUNCT_PER_TOKEN);
        } else {
            n += 2;
        }
    }
    n
}

/// Estimate input tokens for a slice of [`ModelMessage`]s — the protocol-agnostic
/// message type that `session` and `compaction` operate on.
///
/// This is the default counter's implementation. A session counts with the
/// `TokenCounter` it was built with, and compaction is handed that same
/// counter, so the number that trips compaction, the number that decides which
/// rounds to drop and the number reported in telemetry are the same number.
///
/// Counts message *content* only. Per-message API framing (role, JSON
/// punctuation) is a handful of tokens per message and is deliberately not
/// modelled — same omission as [`estimate_input_tokens`].
pub fn estimate_message_tokens(messages: &[ModelMessage]) -> usize {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .map(estimate_model_block_tokens)
        .sum()
}

/// Estimate input tokens for a single [`ModelContentBlock`].
pub fn estimate_model_block_tokens(block: &ModelContentBlock) -> usize {
    match block {
        ModelContentBlock::Text { text } => count_text(text),
        // Flat estimate. Counting the base64 payload would read a 1 MB
        // screenshot as hundreds of thousands of tokens and trip compaction on
        // the first image the user pastes — see `IMAGE_TOKEN_ESTIMATE`.
        ModelContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
        // Thinking is billed as text. The signature is an opaque blob that the
        // API requires us to echo back; it is not counted.
        ModelContentBlock::Thinking { text, .. } => count_text(text),
        ModelContentBlock::RedactedThinking { data, .. } => count_text(data),
        ModelContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => count_text(tool_use_id) + count_text(content),
        ModelContentBlock::ToolUse { id, name, input } => {
            count_text(id) + count_text(name) + count_text(&input.to_string())
        }
    }
}

/// The engine's default [`TokenCounter`]: everything above, behind the
/// contract, so a host with a better answer can supply one.
///
/// Stateless, so constructing one costs nothing and any number of them agree.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicCounter;

impl TokenCounter for HeuristicCounter {
    fn count_text(&self, text: &str) -> usize {
        count_text(text)
    }

    fn count_block(&self, block: &ModelContentBlock) -> usize {
        estimate_model_block_tokens(block)
    }

    fn count_messages(&self, messages: &[ModelMessage]) -> usize {
        estimate_message_tokens(messages)
    }
}

/// The counter used wherever nothing more specific was supplied.
pub fn default_counter() -> std::sync::Arc<dyn TokenCounter> {
    static C: OnceLock<std::sync::Arc<dyn TokenCounter>> = OnceLock::new();
    C.get_or_init(|| std::sync::Arc::new(HeuristicCounter))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_count_is_the_sum_of_its_blocks() {
        let counter = FixedTokenCounter::new(1, 10);
        let messages = vec![ModelMessage {
            role: crate::interface::model::MessageRole::User,
            content: vec![
                ModelContentBlock::Text {
                    text: "abcd".into(),
                },
                ModelContentBlock::Text { text: "ef".into() },
            ],
        }];
        assert_eq!(counter.count_messages(&messages), (10 + 4) + (10 + 2));
    }

    fn model_user(blocks: Vec<ModelContentBlock>) -> ModelMessage {
        ModelMessage {
            role: crate::interface::model::MessageRole::User,
            content: blocks,
        }
    }

    #[test]
    fn message_tokens_are_real_tokens_not_char_quarters() {
        // "len() / 4" would call this 7; a long word is several tokens, not one.
        let msgs = vec![model_user(vec![ModelContentBlock::Text {
            text: "antidisestablishmentarianism".into(),
        }])];
        let n = estimate_message_tokens(&msgs);
        assert!(n >= 4, "expected real tokenization, got {n}");
    }

    #[test]
    fn image_block_is_flat_estimate_not_base64_length() {
        // A ~1 MB base64 payload. Under `len() / 4` this reads as ~250_000
        // tokens, which trips compaction on the first pasted screenshot.
        let msgs = vec![model_user(vec![ModelContentBlock::Image {
            media_type: "image/png".into(),
            data: "A".repeat(1_000_000),
        }])];
        assert_eq!(estimate_message_tokens(&msgs), IMAGE_TOKEN_ESTIMATE);
    }

    #[test]
    fn empty_messages_are_zero_tokens() {
        assert_eq!(estimate_message_tokens(&[]), 0);
    }

    #[test]
    fn tool_use_counts_its_serialized_input() {
        let small = vec![model_user(vec![ModelContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input: serde_json::json!({}),
        }])];
        let large = vec![model_user(vec![ModelContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "grep -rn pattern ./src --include='*.rs'"}),
        }])];
        assert!(
            estimate_message_tokens(&large) > estimate_message_tokens(&small),
            "tool input must contribute tokens"
        );
    }

    #[test]
    fn estimates_track_the_shape_of_text() {
        assert_eq!(count_text(""), 0);
        assert_eq!(count_text("hello world"), 2);
        assert_eq!(count_text("1234567"), 3);
        assert_eq!(count_text("这是中文"), 4);
        assert_eq!(count_text("fn main() {}"), 4);
        assert_eq!(count_text("    indented"), count_text("indented"));
        assert_eq!(count_text("a\n\n\nb"), 3);
    }

    #[test]
    fn multibyte_text_does_not_panic_and_costs_tokens() {
        let msgs = vec![model_user(vec![ModelContentBlock::Text {
            text: "这是一段用于测试的中文内容".repeat(100),
        }])];
        assert!(estimate_message_tokens(&msgs) > 0);
    }
}

#[cfg(test)]
mod counter_contract_tests {
    use super::*;

    /// The contract's default implementation must be the algorithm the free
    /// functions already use — otherwise "the number that trips compaction
    /// and the number that decides what to drop are the same number" stops
    /// being true the moment a caller moves to the trait.
    #[test]
    fn the_default_counter_agrees_with_the_free_functions() {
        let counter = HeuristicCounter;
        let messages = vec![ModelMessage {
            role: crate::interface::model::MessageRole::User,
            content: vec![
                ModelContentBlock::Text {
                    text: "a moderately long piece of prose, long enough to matter".repeat(20),
                },
                ModelContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "git status"}),
                },
                ModelContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                },
            ],
        }];

        assert_eq!(
            counter.count_messages(&messages),
            estimate_message_tokens(&messages)
        );
        for block in &messages[0].content {
            assert_eq!(
                counter.count_block(block),
                estimate_model_block_tokens(block)
            );
        }
        assert_eq!(counter.count_text("hello there"), count_text("hello there"));
    }

    #[test]
    fn the_default_counter_is_the_one_handed_out() {
        let messages = vec![ModelMessage {
            role: crate::interface::model::MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: "hello".into(),
            }],
        }];
        assert_eq!(
            default_counter().count_messages(&messages),
            estimate_message_tokens(&messages)
        );
    }
}
