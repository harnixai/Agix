//! What Anthropic's Messages API takes from each Claude model family: the
//! shape that turns thinking off, whether a fixed `budget_tokens` is
//! accepted, and whether a call may force a tool. This is the one place that
//! knows which family an id belongs to.

use base::interface::model::{Effort, StreamParams, ThinkingMode};

use crate::adapter::MIN_THINKING_BUDGET;
use crate::types::ThinkingConfig;

/// What a model accepts for turning thinking off and for a fixed budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingRules {
    /// Thinking cannot be turned off and takes no budget: only a lower effort
    /// makes it think less. Opus 5.5 and later, Fable, Mythos.
    AlwaysOn,
    /// `between_tools` turns it off, at effort `high` or below, with no other
    /// field; no budget. Sonnet 5.5 and later.
    BetweenTools,
    /// `disabled` only at effort `high` or below; no budget. Opus 5, Haiku 5.5
    /// and later.
    DisabledUpToHigh,
    /// `disabled` at any effort; no budget. Opus 4.7 and 4.8, Sonnet 5.
    NoBudget,
    /// `disabled` and `budget_tokens` both taken — older Claude models, and
    /// any id that is not a Claude one, which compatible endpoints serve.
    Legacy,
}

/// The family and `(major, minor)` of `model`, read from the
/// `claude-<family>-<major>-<minor>` in it, so a provider prefix
/// (`us.anthropic.`), a date or a `@`/`-v1:0` snapshot suffix does not change
/// the answer. `None` for an id that is not a Claude one.
fn family_of(model: &str) -> Option<(String, (u32, u32))> {
    const MARK: &str = "claude-";
    let id = model.to_ascii_lowercase();
    let start = id.find(MARK)?;
    let mut parts = id[start + MARK.len()..].split(|c: char| !c.is_ascii_alphanumeric());
    let family = parts.next().unwrap_or_default().to_string();
    // A date is eight digits; a version number is one or two.
    let numbers: Vec<u32> = parts
        .map_while(|p| (p.len() <= 2).then(|| p.parse().ok()).flatten())
        .collect();
    let version = (
        numbers.first().copied().unwrap_or(0),
        numbers.get(1).copied().unwrap_or(0),
    );
    Some((family, version))
}

/// Whether `tool_choice` may be `any` or `tool` on `model`. Opus and Sonnet
/// 5.5, Fable and Mythos 5.1 and later refuse both with a 400; `auto` and
/// `none` are taken everywhere.
pub(crate) fn forced_tool_choice_taken(model: &str) -> bool {
    match family_of(model) {
        Some((family, version)) => match family.as_str() {
            "opus" | "sonnet" => version < (5, 5),
            "fable" | "mythos" => version < (5, 1),
            _ => true,
        },
        None => true,
    }
}

pub(crate) fn rules_for(model: &str) -> ThinkingRules {
    let Some((family, version)) = family_of(model) else {
        return ThinkingRules::Legacy;
    };
    match family.as_str() {
        "fable" | "mythos" => ThinkingRules::AlwaysOn,
        "opus" if version >= (5, 5) => ThinkingRules::AlwaysOn,
        "opus" if version >= (5, 0) => ThinkingRules::DisabledUpToHigh,
        "opus" if version >= (4, 7) => ThinkingRules::NoBudget,
        "sonnet" if version >= (5, 5) => ThinkingRules::BetweenTools,
        "sonnet" if version >= (5, 0) => ThinkingRules::NoBudget,
        "haiku" if version >= (5, 5) => ThinkingRules::DisabledUpToHigh,
        _ => ThinkingRules::Legacy,
    }
}

/// [`wire`] for `params`.
pub(crate) fn of(params: &StreamParams) -> (Option<ThinkingConfig>, Option<Effort>) {
    wire(
        params.thinking_mode.clone(),
        params.effort,
        &params.model,
        params.max_tokens,
    )
}

/// The `thinking` field and the effort a call to `model` goes out with.
///
/// A budget is held under `max_tokens`, which Anthropic requires of it; a
/// call whose `max_tokens` leaves no room for the least budget goes without
/// thinking. A model that takes no budget thinks adaptively instead.
pub(crate) fn wire(
    mode: ThinkingMode,
    effort: Option<Effort>,
    model: &str,
    max_tokens: u32,
) -> (Option<ThinkingConfig>, Option<Effort>) {
    let rules = rules_for(model);
    match mode {
        ThinkingMode::Auto => (None, effort),
        ThinkingMode::On => (Some(ThinkingConfig::Adaptive), effort),
        ThinkingMode::Off => off(rules, effort),
        ThinkingMode::OnBudget(_) if rules != ThinkingRules::Legacy => {
            (Some(ThinkingConfig::Adaptive), effort)
        }
        ThinkingMode::OnBudget(n) => match budget(n, max_tokens) {
            Some(budget_tokens) => (Some(ThinkingConfig::Enabled { budget_tokens }), effort),
            None => off(rules, effort),
        },
    }
}

fn off(rules: ThinkingRules, effort: Option<Effort>) -> (Option<ThinkingConfig>, Option<Effort>) {
    match rules {
        ThinkingRules::AlwaysOn => (None, Some(effort.unwrap_or(Effort::Low))),
        ThinkingRules::BetweenTools => {
            (Some(ThinkingConfig::BetweenTools), effort.map(at_most_high))
        }
        ThinkingRules::DisabledUpToHigh => {
            (Some(ThinkingConfig::Disabled), effort.map(at_most_high))
        }
        ThinkingRules::NoBudget | ThinkingRules::Legacy => (Some(ThinkingConfig::Disabled), effort),
    }
}

fn at_most_high(effort: Effort) -> Effort {
    match effort {
        Effort::Xhigh | Effort::Max => Effort::High,
        effort => effort,
    }
}

fn budget(n: u32, max_tokens: u32) -> Option<u32> {
    if n < max_tokens {
        return Some(n);
    }
    let room = max_tokens.saturating_sub(1);
    if room < MIN_THINKING_BUDGET {
        tracing::warn!(
            budget = n,
            max_tokens,
            "thinking budget left out: max_tokens leaves no room for one"
        );
        return None;
    }
    tracing::warn!(
        budget = n,
        max_tokens,
        held_to = room,
        "thinking budget held under max_tokens"
    );
    Some(room)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ThinkingRules::*;

    #[test]
    fn each_family_reads_its_rules_from_the_id() {
        let cases = [
            ("claude-opus-5-5", AlwaysOn),
            ("claude-fable-5", AlwaysOn),
            ("claude-fable-5-1", AlwaysOn),
            ("claude-fable-5-2", AlwaysOn),
            ("claude-mythos-5-2", AlwaysOn),
            ("claude-mythos-preview", AlwaysOn),
            ("claude-sonnet-5-5", BetweenTools),
            ("claude-opus-5", DisabledUpToHigh),
            ("claude-haiku-5-5", DisabledUpToHigh),
            ("claude-opus-4-8", NoBudget),
            ("claude-opus-4-7", NoBudget),
            ("claude-sonnet-5", NoBudget),
            ("claude-opus-4-6", Legacy),
            ("claude-sonnet-4-6", Legacy),
            ("claude-haiku-4-5", Legacy),
            ("claude-sonnet-4-5-20250929", Legacy),
            ("claude-3-5-sonnet-20241022", Legacy),
            ("deepseek-v4-pro", Legacy),
            ("deepseek-chat", Legacy),
            ("", Legacy),
        ];
        for (id, rules) in cases {
            assert_eq!(rules_for(id), rules, "{id}");
        }
    }

    #[test]
    fn a_date_a_snapshot_or_a_provider_prefix_does_not_change_the_family() {
        let cases = [
            ("claude-opus-5-5-20260401", AlwaysOn),
            ("anthropic.claude-opus-5-5", AlwaysOn),
            ("us.anthropic.claude-fable-5-2-v1:0", AlwaysOn),
            ("claude-sonnet-5-5-20260901", BetweenTools),
            ("us.anthropic.claude-sonnet-5-5-v1:0", BetweenTools),
            ("claude-sonnet-5-5[1m]", BetweenTools),
            ("claude-opus-5-20260101", DisabledUpToHigh),
            ("claude-opus-5@20260101", DisabledUpToHigh),
            ("anthropic.claude-haiku-5-5", DisabledUpToHigh),
            ("claude-opus-4-8-20260301", NoBudget),
            ("Claude-Sonnet-5", NoBudget),
            ("claude-opus-4-6@20260205", Legacy),
        ];
        for (id, rules) in cases {
            assert_eq!(rules_for(id), rules, "{id}");
        }
    }

    #[test]
    fn a_tool_is_forced_only_on_models_that_take_it() {
        let refused = [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-fable-5-1",
            "claude-fable-5-2",
            "claude-mythos-5-1",
            "claude-mythos-5-2",
            "claude-opus-5-5-20260401",
            "us.anthropic.claude-sonnet-5-5-v1:0",
            "claude-sonnet-5-5[1m]",
            "claude-opus-6",
        ];
        for id in refused {
            assert!(!forced_tool_choice_taken(id), "{id}");
        }
        let taken = [
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            "claude-fable-5",
            "claude-mythos-preview",
            "claude-haiku-5-5",
            "claude-3-5-sonnet-20241022",
            "deepseek-v4-pro",
            "deepseek-chat",
            "",
        ];
        for id in taken {
            assert!(forced_tool_choice_taken(id), "{id}");
        }
    }

    fn off_for(model: &str, effort: Option<Effort>) -> (Option<ThinkingConfig>, Option<Effort>) {
        wire(ThinkingMode::Off, effort, model, 1000)
    }

    #[test]
    fn off_on_a_model_that_always_thinks_sends_no_thinking_and_a_low_effort() {
        assert_eq!(off_for("claude-opus-5-5", None), (None, Some(Effort::Low)));
        assert_eq!(off_for("claude-fable-5-2", None), (None, Some(Effort::Low)));
        assert_eq!(
            off_for("claude-opus-5-5", Some(Effort::High)),
            (None, Some(Effort::High))
        );
    }

    #[test]
    fn off_on_sonnet_5_5_is_between_tools_at_high_or_below() {
        assert_eq!(
            off_for("claude-sonnet-5-5", None),
            (Some(ThinkingConfig::BetweenTools), None)
        );
        assert_eq!(
            off_for("claude-sonnet-5-5", Some(Effort::Max)),
            (Some(ThinkingConfig::BetweenTools), Some(Effort::High))
        );
        assert_eq!(
            off_for("claude-sonnet-5-5", Some(Effort::Low)),
            (Some(ThinkingConfig::BetweenTools), Some(Effort::Low))
        );
    }

    #[test]
    fn off_on_opus_5_and_haiku_5_5_is_disabled_at_high_or_below() {
        for model in ["claude-opus-5", "claude-haiku-5-5"] {
            assert_eq!(
                off_for(model, Some(Effort::Xhigh)),
                (Some(ThinkingConfig::Disabled), Some(Effort::High)),
                "{model}"
            );
            assert_eq!(
                off_for(model, Some(Effort::Medium)),
                (Some(ThinkingConfig::Disabled), Some(Effort::Medium)),
                "{model}"
            );
        }
    }

    #[test]
    fn off_elsewhere_is_disabled_with_the_effort_as_given() {
        for model in ["claude-opus-4-8", "claude-sonnet-4-6", "deepseek-v4-pro"] {
            assert_eq!(
                off_for(model, Some(Effort::Max)),
                (Some(ThinkingConfig::Disabled), Some(Effort::Max)),
                "{model}"
            );
        }
    }

    #[test]
    fn a_budget_goes_only_to_models_that_take_one() {
        for model in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-opus-5",
            "claude-opus-4-7",
            "claude-sonnet-5",
            "claude-fable-5-1",
        ] {
            assert_eq!(
                wire(ThinkingMode::OnBudget(4096), None, model, 8192),
                (Some(ThinkingConfig::Adaptive), None),
                "{model}"
            );
        }
        assert_eq!(
            wire(
                ThinkingMode::OnBudget(4096),
                None,
                "claude-sonnet-4-6",
                8192
            ),
            (
                Some(ThinkingConfig::Enabled {
                    budget_tokens: 4096
                }),
                None
            )
        );
        assert_eq!(
            wire(ThinkingMode::OnBudget(4096), None, "deepseek-v4-pro", 2048),
            (
                Some(ThinkingConfig::Enabled {
                    budget_tokens: 2047
                }),
                None
            )
        );
        assert_eq!(
            wire(ThinkingMode::OnBudget(4096), None, "claude-haiku-4-5", 500),
            (Some(ThinkingConfig::Disabled), None)
        );
    }
}
