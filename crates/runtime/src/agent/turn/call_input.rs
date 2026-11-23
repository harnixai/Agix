//! What a model call's input will count against a budget, estimated before
//! the call is sent, and what is left for its reply once that is paid.
//!
//! Budgets count input not read from the cache, cache writes and output, so
//! the estimate is the part of the request the provider is not expected to
//! serve from its prompt cache: what was added since the agent's last call,
//! when that call read from the cache. Judging every call by its whole prompt
//! would stop a cached session long before it has spent anything.

use base::interface::budget_policy::{Remaining, Scope, MIN_CALL_TOKENS};
use base::interface::model::{MessageRole, ModelMessage, Usage};
use base::interface::token_counter::TokenCounter;

/// A request as it went out: how many messages, their size and the size of
/// the system prompt and tool definitions before them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SentPrompt {
    messages: usize,
    message_tokens: usize,
    overhead: usize,
}

impl SentPrompt {
    pub(crate) fn of(
        counter: &dyn TokenCounter,
        messages: &[ModelMessage],
        overhead: usize,
    ) -> Self {
        Self {
            messages: messages.len(),
            message_tokens: counter.count_messages(messages),
            overhead,
        }
    }

    /// The call, once the provider said what it cost.
    pub(crate) fn answered(self, usage: Usage) -> LastCall {
        let billed = u64::from(usage.input_tokens) + u64::from(usage.cache_creation_input_tokens);
        LastCall {
            sent: self,
            uncached: (usage.cache_read_input_tokens == 0).then_some(billed),
        }
    }
}

/// An agent's last call, which the next one is estimated from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LastCall {
    sent: SentPrompt,
    /// All of its prompt the provider billed, when it read none of it from
    /// the cache: the next call sends it again, as uncached.
    uncached: Option<u64>,
}

/// The input of a request of `messages` after `overhead` that will count:
///
/// - extending the last call's prompt, what was added since, and all the
///   last call was billed for when it read nothing from the cache;
/// - not extending it (compacted, trimmed, edited, other tools), the whole
///   request;
/// - with no last call, what came after the model last spoke — the system
///   prompt, the tools and what the model has seen assumed cached.
pub(crate) fn counted_input(
    counter: &dyn TokenCounter,
    messages: &[ModelMessage],
    overhead: usize,
    last: Option<&LastCall>,
) -> u64 {
    let tokens = |m: &[ModelMessage]| counter.count_messages(m) as u64;
    let Some(last) = last else {
        let after = messages
            .iter()
            .rposition(|m| m.role == MessageRole::Assistant)
            .map_or(0, |at| at + 1);
        return tokens(&messages[after..]);
    };
    let sent = last.sent;
    let extends = sent.overhead == overhead
        && sent.messages <= messages.len()
        && counter.count_messages(&messages[..sent.messages]) == sent.message_tokens;
    if !extends {
        return tokens(messages) + overhead as u64;
    }
    tokens(&messages[sent.messages..]) + last.uncached.unwrap_or(0)
}

/// A ceiling that leaves too little for a call once its input is paid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unaffordable {
    pub(crate) scope: Scope,
    pub(crate) left: Remaining,
    pub(crate) input: u64,
}

impl Unaffordable {
    /// Whose budget, and — when it was the input that did not fit — how
    /// much was left and asked for.
    pub(crate) fn message(&self) -> String {
        let Self { scope, left, input } = *self;
        if input == 0 || left.tokens < MIN_CALL_TOKENS {
            return base::interface::budget_policy::exhausted_message(scope, left.limit);
        }
        format!(
            "{} token budget of {} has {} left, too little for the next call's input of \
             about {input} tokens and its reply",
            scope.whose(),
            left.limit,
            left.tokens
        )
    }
}

/// The most output a call may ask for under `tightest` once `input` is
/// paid; `None` under no ceiling.
pub(crate) fn headroom(
    tightest: Option<(Scope, Remaining)>,
    input: u64,
) -> Result<Option<u32>, Unaffordable> {
    let Some((scope, left)) = tightest else {
        return Ok(None);
    };
    let after = left.tokens.saturating_sub(input);
    if after < MIN_CALL_TOKENS {
        return Err(Unaffordable { scope, left, input });
    }
    Ok(Some(u32::try_from(after).unwrap_or(u32::MAX)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::ModelContentBlock;

    fn text(role: MessageRole, n: usize) -> ModelMessage {
        ModelMessage {
            role,
            content: vec![ModelContentBlock::Text {
                text: "x".repeat(n),
            }],
        }
    }

    fn counter() -> base::interface::token_counter::FixedTokenCounter {
        base::interface::token_counter::FixedTokenCounter::new(1, 0)
    }

    fn cached() -> Usage {
        Usage {
            input_tokens: 10,
            cache_read_input_tokens: 5000,
            ..Default::default()
        }
    }

    #[test]
    fn a_first_call_counts_what_came_after_the_model_last_spoke() {
        let c = counter();
        let fresh = [text(MessageRole::User, 40)];
        assert_eq!(counted_input(&c, &fresh, 900, None), 40);
        let resumed = [
            text(MessageRole::User, 40),
            text(MessageRole::Assistant, 30),
            text(MessageRole::User, 7),
        ];
        assert_eq!(counted_input(&c, &resumed, 900, None), 7);
    }

    #[test]
    fn after_a_cached_call_only_what_was_added_counts() {
        let c = counter();
        let mut messages = vec![text(MessageRole::User, 40)];
        let last = SentPrompt::of(&c, &messages, 900).answered(cached());
        messages.push(text(MessageRole::Assistant, 30));
        messages.push(text(MessageRole::User, 200));
        assert_eq!(counted_input(&c, &messages, 900, Some(&last)), 230);
    }

    #[test]
    fn after_an_uncached_call_its_whole_bill_counts_again() {
        let c = counter();
        let mut messages = vec![text(MessageRole::User, 40)];
        let usage = Usage {
            input_tokens: 2500,
            cache_creation_input_tokens: 100,
            ..Default::default()
        };
        let last = SentPrompt::of(&c, &messages, 900).answered(usage);
        messages.push(text(MessageRole::Assistant, 30));
        assert_eq!(counted_input(&c, &messages, 900, Some(&last)), 2630);
    }

    #[test]
    fn a_prompt_that_does_not_extend_the_last_counts_whole() {
        let c = counter();
        let messages = vec![text(MessageRole::User, 40), text(MessageRole::User, 50)];
        let last = SentPrompt::of(&c, &messages, 900).answered(cached());
        let compacted = [text(MessageRole::User, 20)];
        assert_eq!(counted_input(&c, &compacted, 900, Some(&last)), 920);
        let edited = [text(MessageRole::User, 41), text(MessageRole::User, 50)];
        assert_eq!(counted_input(&c, &edited, 900, Some(&last)), 991);
        assert_eq!(counted_input(&c, &messages, 950, Some(&last)), 1040);
    }

    #[test]
    fn what_is_left_after_the_input_caps_the_reply_and_under_the_minimum_stops() {
        let left = |tokens| {
            Some((
                Scope::Turn,
                Remaining {
                    tokens,
                    limit: 6000,
                },
            ))
        };
        assert_eq!(headroom(None, 50_000), Ok(None));
        assert_eq!(headroom(left(3000), 500), Ok(Some(2500)));
        let refused = headroom(left(3000), 2500).unwrap_err();
        assert_eq!(refused.input, 2500);
        assert!(
            refused.message().contains("has 3000 left"),
            "{}",
            refused.message()
        );
        let spent = headroom(left(900), 0).unwrap_err();
        assert!(
            spent.message().ends_with("is used up"),
            "{}",
            spent.message()
        );
    }
}
