//! The frontmatter of a markdown file: the `---` block at its head and the
//! YAML scalars and lists a file's reader pulls out of it.

/// 把 `---\n<yaml>\n---\n<body>` 切成 (Some(yaml), body)；不带 frontmatter 时
/// 返回 (None, content)。
pub fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    if !content.starts_with("---") {
        return (None, content);
    }
    // 第一个 `---` 后紧跟换行
    let after_first = &content[3..];
    let after_first = after_first.strip_prefix('\n').unwrap_or(after_first);
    // 在剩余内容里找下一个 `\n---` 行
    let Some(rel) = find_closing_marker(after_first) else {
        return (None, content);
    };
    let yaml = &after_first[..rel];
    let after_close = &after_first[rel..];
    // 跳过 `---` 后的换行
    let after_close = after_close.strip_prefix("---").unwrap_or(after_close);
    let after_close = after_close.strip_prefix('\n').unwrap_or(after_close);
    (Some(yaml), after_close)
}

/// 找 `\n---` 之后是 \n 或 EOF 的位置；返回相对 yaml 起点的 byte offset。
fn find_closing_marker(s: &str) -> Option<usize> {
    let mut search_from = 0usize;
    while let Some(rel) = s[search_from..].find("\n---") {
        let abs = search_from + rel;
        let after = abs + 4; // `\n---` 4 字节
        let next = s.as_bytes().get(after);
        if next.is_none() || matches!(next, Some(b'\n') | Some(b' ') | Some(b'\r')) {
            // `\n---\n` 命中；返回 abs+1（跳过开头那个 `\n`）
            return Some(abs + 1);
        }
        search_from = abs + 1;
    }
    None
}

/// Fallback description when frontmatter omits it: the first markdown
/// *paragraph* of the body (consecutive non-blank lines, joined with a
/// space), not just the first line. A leading `#` heading prefix is
/// stripped from the paragraph's first line only, so a lone heading line
/// (heading, blank line, then body) still yields just the heading text.
pub fn first_paragraph(body: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for line in body.lines().skip_while(|l| l.trim().is_empty()) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        let text = if parts.is_empty() {
            trimmed.trim_start_matches('#').trim()
        } else {
            trimmed
        };
        if !text.is_empty() {
            parts.push(text.to_string());
        }
    }
    parts.join(" ")
}

pub fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        let last = s.as_bytes()[s.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Parse a YAML inline list value: `[a, b, c]`, comma-separated `a, b, c`, or
/// space-separated `a b c` — frontmatter fields like `allowed-tools`/
/// `arguments`/`skills` all accept any of the three forms. Separators inside
/// parentheses do not split, so `Bash(cargo test:*)` stays one entry.
pub fn parse_yaml_list(raw: &str) -> Vec<String> {
    let s = raw.trim();
    let (inner, by_comma) = match s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        Some(inner) => (inner, true),
        None => (s, split_outside_parens(s, |c| c == ',').len() > 1),
    };
    let items = if by_comma {
        split_outside_parens(inner, |c| c == ',')
    } else {
        split_outside_parens(inner, char::is_whitespace)
    };
    items
        .into_iter()
        .map(|item| strip_quotes(item.trim()).to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn split_outside_parens(s: &str, is_separator: impl Fn(char) -> bool) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 && is_separator(c) => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Extract a simple YAML key:value from a one-level frontmatter block.
/// Handles `name: my-memory` and `description: "one line"`.
pub fn extract_yaml_field(yaml: &str, key: &str) -> Option<String> {
    for line in yaml.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(key) {
            let rest = rest.trim_start().strip_prefix(':')?.trim_start();
            if rest.is_empty() {
                return None;
            }
            return Some(strip_quotes(rest).to_string());
        }
    }
    None
}

/// The value of the top-level `key` in a frontmatter block, with YAML's
/// quoting undone: double-quoted strings and their escapes, single-quoted
/// strings and `''`, plain scalars up to a ` #` comment, and `|` / `>` block
/// scalars. A flat block is all the engine writes; this covers what a person
/// editing one by hand is likely to write too.
pub fn scalar(frontmatter: &str, key: &str) -> Option<String> {
    let lines: Vec<&str> = frontmatter.lines().collect();
    let prefix = format!("{key}:");
    let at = lines
        .iter()
        .position(|l| !l.starts_with([' ', '\t']) && l.starts_with(&prefix))?;
    let rest = lines[at][prefix.len()..].trim();
    match rest.chars().next() {
        Some('|') | Some('>') => Some(block_scalar(&lines[at + 1..], rest.starts_with('>'))),
        _ => Some(inline_scalar(rest)),
    }
}

fn inline_scalar(raw: &str) -> String {
    if let Some(body) = raw.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = body.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('0') => out.push('\0'),
                    Some('u') => {
                        let hex: String = chars.by_ref().take(4).collect();
                        if let Some(ch) =
                            u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                        {
                            out.push(ch);
                        }
                    }
                    Some(other) => out.push(other),
                    None => {}
                },
                other => out.push(other),
            }
        }
        return out;
    }
    if let Some(body) = raw.strip_prefix('\'') {
        let mut out = String::new();
        let mut chars = body.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    out.push('\'');
                    continue;
                }
                break;
            }
            out.push(c);
        }
        return out;
    }
    match raw.find(" #") {
        Some(i) => raw[..i].trim_end().to_string(),
        None => raw.to_string(),
    }
}

/// The indented lines after a `|` (kept) or `>` (folded) header.
fn block_scalar(following: &[&str], folded: bool) -> String {
    let body: Vec<&str> = following
        .iter()
        .take_while(|l| l.is_empty() || l.starts_with([' ', '\t']))
        .copied()
        .collect();
    let indent = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches([' ', '\t']).len())
        .min()
        .unwrap_or(0);
    let lines: Vec<&str> = body.iter().map(|l| l.get(indent..).unwrap_or("")).collect();
    let joined = if folded {
        lines.join(" ")
    } else {
        lines.join("\n")
    };
    joined.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn scalars_undo_yaml_quoting() {
        let fm = "name: plain value # a comment\n\
                  description: \"says \\\"hi\\\" at C:\\\\x: ok\\u00e9\"\n\
                  single: 'it''s'\n\
                  empty:\n\
                  folded: >\n  one\n  two\n\
                  kept: |\n  a\n  b\n\
                  metadata:\n  type: feedback\n";
        assert_eq!(scalar(fm, "name").as_deref(), Some("plain value"));
        assert_eq!(
            scalar(fm, "description").as_deref(),
            Some("says \"hi\" at C:\\x: ok\u{e9}")
        );
        assert_eq!(scalar(fm, "single").as_deref(), Some("it's"));
        assert_eq!(scalar(fm, "empty").as_deref(), Some(""));
        assert_eq!(scalar(fm, "folded").as_deref(), Some("one two"));
        assert_eq!(scalar(fm, "kept").as_deref(), Some("a\nb"));
        assert_eq!(
            scalar(fm, "type"),
            None,
            "a nested key is not a top-level one"
        );
        assert_eq!(scalar(fm, "missing"), None);
    }

    #[test]
    fn a_block_indented_with_wide_whitespace_does_not_split_a_character() {
        let fm = "description: |\n  \u{3000}x\n   y\n    \u{3000}\n";
        assert_eq!(scalar(fm, "description").as_deref(), Some("\u{3000}x\n y"));
    }

    use super::*;

    #[test]
    fn frontmatter_split_basic() {
        let s = "---\nname: foo\ndescription: bar\n---\nbody here\n";
        let (front, body) = split_frontmatter(s);
        assert_eq!(front, Some("name: foo\ndescription: bar\n"));
        assert_eq!(body, "body here\n");
    }

    #[test]
    fn frontmatter_split_no_frontmatter_returns_none() {
        let s = "no frontmatter here\nbody";
        let (front, body) = split_frontmatter(s);
        assert_eq!(front, None);
        assert_eq!(body, s);
    }

    #[test]
    fn frontmatter_split_unclosed_returns_none() {
        let s = "---\nname: foo\nno closing marker";
        let (front, body) = split_frontmatter(s);
        assert_eq!(front, None);
        assert_eq!(body, s);
    }

    #[test]
    fn a_scoped_grant_with_spaces_stays_one_entry() {
        let list = |raw: &str| parse_yaml_list(raw);
        assert_eq!(list("Bash(sh build.sh)"), vec!["Bash(sh build.sh)"]);
        assert_eq!(
            list("Bash(cargo test:*) Read"),
            vec!["Bash(cargo test:*)", "Read"]
        );
        assert_eq!(
            list("Bash(git add, commit), Read"),
            vec!["Bash(git add, commit)", "Read"]
        );
        assert_eq!(
            list("[Bash(npm run build), \"Edit\"]"),
            vec!["Bash(npm run build)", "Edit"]
        );
        assert_eq!(list("issue, branch"), vec!["issue", "branch"]);
    }
}
