//! UTF-8-safe string truncation.
//!
//! Rust's `&s[..n]` **panics** when `n` lands inside a multi-byte character.
//! Every truncation in this codebase is expressed as a byte budget (token
//! estimates, tool-result caps, prompt budgets), and the content being
//! truncated is routinely non-ASCII — skill descriptions, prompts, and
//! conversation text in this repo are frequently Chinese, where one character
//! is 3 bytes and a cut at an arbitrary byte offset lands mid-character with
//! probability ~2/3.

pub mod frontmatter;

/// Truncate `s` to at most `max_bytes` bytes, cutting at a character boundary.
///
/// Returns a prefix of `s` whose length is `<= max_bytes`. When `max_bytes`
/// falls inside a multi-byte character, the cut moves *backwards* to the start
/// of that character, so the result is never longer than requested (important
/// when the caller is enforcing a hard budget).
///
/// ```
/// # use base::text::truncate_at_char_boundary as t;
/// assert_eq!(t("hello", 10), "hello");        // shorter than budget
/// assert_eq!(t("hello", 3), "hel");           // ASCII cut
/// assert_eq!(t("中文测试", 5), "中");          // byte 5 is mid-character → back off
/// assert_eq!(t("中文测试", 6), "中文");        // byte 6 is a boundary
/// ```
pub fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Truncate a string at `max` chars, appending `marker` when truncated.
pub fn truncate_chars(s: &str, max: usize, marker: &str) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}{marker}")
    }
}

/// `text` with the credentials a log line could carry masked: `sk-…` style
/// keys, the word after `Bearer` / `Basic`, and the value of any `name=value`
/// whose name says token, key, secret, password or auth (URL query strings
/// and `--flag=value` arguments included).
pub fn scrub_secrets(text: &str) -> String {
    const MASK: &str = "***";
    const SECRET_NAMES: &[&str] = &["token", "key", "secret", "passw", "auth", "credential"];
    let is_boundary =
        |c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '&' | '?' | ',' | ';');
    let mut out = String::with_capacity(text.len());
    let mut after_scheme = false;
    let mut rest = text;
    while !rest.is_empty() {
        let end = rest.find(is_boundary).unwrap_or(rest.len());
        let (word, tail) = rest.split_at(end);
        let masked = if word.is_empty() {
            None
        } else if after_scheme {
            Some(MASK.to_string())
        } else if let Some((name, value)) = word.split_once('=') {
            let lower = name.to_ascii_lowercase();
            (!value.is_empty() && SECRET_NAMES.iter().any(|n| lower.contains(n)))
                .then(|| format!("{name}={MASK}"))
        } else if (word.starts_with("sk-") || word.starts_with("sk_")) && word.len() >= 10 {
            Some(MASK.to_string())
        } else {
            None
        };
        if !word.is_empty() {
            after_scheme = matches!(word, "Bearer" | "bearer" | "Basic" | "basic");
        }
        out.push_str(masked.as_deref().unwrap_or(word));
        let sep_len = tail.chars().next().map_or(0, char::len_utf8);
        out.push_str(&tail[..sep_len]);
        rest = &tail[sep_len..];
    }
    out
}

/// [`scrub_secrets`], and the password of each URL's userinfo
/// (`https://user:***@host`).
pub fn scrub_url(text: &str) -> String {
    let scrubbed = scrub_secrets(text);
    let mut out = String::with_capacity(scrubbed.len());
    let mut rest = scrubbed.as_str();
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
            .unwrap_or(tail.len());
        let (authority, after) = tail.split_at(end);
        match authority
            .rsplit_once('@')
            .and_then(|(userinfo, host)| Some((userinfo.split_once(':')?.0, host)))
        {
            Some((user, host)) => out.push_str(&format!("{user}:***@{host}")),
            None => out.push_str(authority),
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::truncate_at_char_boundary as t;

    #[test]
    fn shorter_than_budget_is_returned_whole() {
        assert_eq!(t("hello", 10), "hello");
        assert_eq!(t("中文", 100), "中文");
        assert_eq!(t("", 0), "");
    }

    #[test]
    fn ascii_cuts_exactly_at_the_budget() {
        assert_eq!(t("hello world", 5), "hello");
    }

    /// Every byte offset inside a multi-byte string must be safe, not just the
    /// boundaries.
    #[test]
    fn every_byte_offset_of_a_multibyte_string_is_safe() {
        let s = "中文测试abc混合デ";
        for n in 0..=s.len() + 4 {
            let out = t(s, n);
            assert!(
                out.len() <= n.min(s.len()),
                "n={n} produced {} bytes",
                out.len()
            );
            assert!(s.starts_with(out), "n={n} produced a non-prefix");
        }
    }

    #[test]
    fn cut_moves_backwards_never_forwards() {
        // byte 5 is inside the second 3-byte char, so we must fall back to 3.
        assert_eq!(t("中文测试", 5), "中");
        assert_eq!(t("中文测试", 6), "中文");
        // A budget of 0, or one smaller than the first character, yields "".
        assert_eq!(t("中文", 0), "");
        assert_eq!(t("中文", 2), "");
    }

    #[test]
    fn combining_marks_and_emoji_survive() {
        let s = "e\u{301}👨‍👩‍👧"; // combining acute + ZWJ family sequence
        for n in 0..=s.len() {
            let out = t(s, n);
            assert!(s.starts_with(out));
        }
    }

    #[test]
    fn truncate_long_string() {
        let s: String = "x".repeat(3000);
        let t = super::truncate_chars(&s, 100, "[truncated]");
        assert_eq!(t.chars().count(), 100 + "[truncated]".chars().count());
        assert!(t.ends_with("[truncated]"));
    }

    #[test]
    fn truncate_short_string_is_passthrough() {
        let t = super::truncate_chars("hi", 10, "[truncated]");
        assert_eq!(t, "hi");
    }

    #[test]
    fn scrub_secrets_masks_keys_bearers_and_secret_named_values() {
        use super::scrub_secrets;
        let line = "got sk-abcdef1234567890 and Authorization: Bearer abc.def \
                    from https://x.test/cb?token=hunter2&page=2 --api-key=zzz";
        let out = scrub_secrets(line);
        for leaked in ["abcdef1234567890", "abc.def", "hunter2", "zzz"] {
            assert!(!out.contains(leaked), "{leaked} in {out}");
        }
        assert!(
            out.contains("page=2") && out.contains("https://x.test/cb"),
            "{out}"
        );
    }
}
