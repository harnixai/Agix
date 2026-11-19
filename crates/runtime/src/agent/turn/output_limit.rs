//! The most `max_tokens` a model accepts, by model name: how far an
//! output-limit recovery may raise a call's limit before the provider refuses
//! the request. A provider's `models[].max_output` overrides it.

pub(crate) const UNKNOWN_MODEL: u32 = 32_000;

/// Prefixes are matched in order, so a family's exceptions come before the
/// family.
const FAMILIES: &[(&str, u32)] = &[
    ("claude-3-haiku", 4_096),
    ("claude-3-opus", 4_096),
    ("claude-3-sonnet", 4_096),
    ("claude-3-5", 8_192),
    ("claude-3-7", 64_000),
    ("claude-opus-4-0", 32_000),
    ("claude-opus-4-1", 32_000),
    ("claude-opus-4-2025", 32_000),
    ("claude-opus-4", 64_000),
    ("claude-sonnet-4", 64_000),
    ("claude-haiku-4", 64_000),
    ("deepseek-chat", 8_192),
    ("deepseek", 64_000),
    ("gpt-4o", 16_384),
    ("gpt-4.1", 32_768),
    ("gpt-5", 128_000),
    ("o1", 100_000),
    ("o3", 100_000),
    ("o4", 100_000),
];

pub(crate) fn max_output_tokens(model: &str) -> u32 {
    let lower = model.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let name = name
        .strip_prefix("us.anthropic.")
        .or_else(|| name.strip_prefix("anthropic."))
        .unwrap_or(name);
    FAMILIES
        .iter()
        .find(|(family, _)| name.starts_with(family))
        .map_or(UNKNOWN_MODEL, |(_, limit)| *limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_familys_exception_wins_over_the_family() {
        assert_eq!(max_output_tokens("claude-opus-4-1-20250805"), 32_000);
        assert_eq!(max_output_tokens("claude-opus-4-5"), 64_000);
        assert_eq!(max_output_tokens("deepseek-chat"), 8_192);
        assert_eq!(max_output_tokens("deepseek-v4-flash"), 64_000);
    }

    #[test]
    fn a_provider_prefix_is_looked_through() {
        assert_eq!(max_output_tokens("anthropic/claude-3-5-haiku"), 8_192);
        assert_eq!(max_output_tokens("us.anthropic.claude-sonnet-4-5"), 64_000);
        assert_eq!(max_output_tokens("openai/GPT-4o-mini"), 16_384);
        assert_eq!(max_output_tokens("llama-3.3-70b"), UNKNOWN_MODEL);
    }
}
