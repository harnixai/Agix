//! The rule string a grant is written as: `<tool_name>(<content>)`.

/// The rule string `<tool_name>(<content>)` for a call whose content is
/// `match_content`. None when there is no content to scope it to.
pub fn build_rule_string(tool_name: &str, match_content: Option<&str>) -> Option<String> {
    let content = match_content?.trim();
    if content.is_empty() {
        return None;
    }
    Some(format!("{tool_name}({content})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_rule_string_returns_none_for_empty() {
        assert!(build_rule_string("Bash", None).is_none());
        assert!(build_rule_string("Bash", Some("")).is_none());
        assert!(build_rule_string("Bash", Some("   ")).is_none());
    }

    #[test]
    fn build_rule_string_basic() {
        assert_eq!(
            build_rule_string("Bash", Some("git status")).as_deref(),
            Some("Bash(git status)")
        );
    }

    #[test]
    fn a_rule_string_carries_the_content_as_written() {
        assert_eq!(
            build_rule_string("WebFetch", Some("https://h.example/**")).as_deref(),
            Some("WebFetch(https://h.example/**)")
        );
    }
}
