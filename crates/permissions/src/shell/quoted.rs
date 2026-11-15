//! Stepping over what the line splitter must not split inside: escapes,
//! quotes, `${...}` and substitutions, whose own commands are split out.

use super::{ansi_c_close, find_char, find_close_paren, find_unescaped, split_into, Segment};

/// When `chars[i]` opens an escape, a quoted string or a substitution, scan
/// it — splitting what runs inside into `out` — and return the index just
/// past it.
pub(super) fn step_over_quoted(chars: &[char], i: usize, out: &mut Vec<Segment>) -> Option<usize> {
    let next = chars.get(i + 1);
    let end = match chars[i] {
        '$' if next == Some(&'\'') => ansi_c_close(chars, i + 2),
        '\'' => find_char(chars, i + 1, '\''),
        '"' => scan_double_quoted(chars, i + 1, out),
        '<' | '>' if next == Some(&'(') => {
            let end = find_close_paren(chars, i + 2);
            split_into_span(chars, i + 2, end, out);
            end
        }
        _ => return step_over_substitution(chars, i, out),
    };
    Some(end + 1)
}

/// [`step_over_quoted`] for what still runs where quotes are plain text: an
/// escape, `$(...)`, `${...}` and backquotes.
pub(super) fn step_over_substitution(
    chars: &[char],
    i: usize,
    out: &mut Vec<Segment>,
) -> Option<usize> {
    let next = chars.get(i + 1);
    let end = match chars[i] {
        '\\' => return Some(i + 2),
        '`' => {
            let end = find_unescaped(chars, i + 1, '`');
            split_into_span(chars, i + 1, end, out);
            end
        }
        '$' if next == Some(&'(') => {
            let end = find_close_paren(chars, i + 2);
            split_into_span(chars, i + 2, end, out);
            end
        }
        '$' if next == Some(&'{') => scan_parameter_expansion(chars, i + 2, out),
        _ => return None,
    };
    Some(end + 1)
}

/// Scan a `${...}` starting just past its `{`, splitting any substitution
/// inside it into `out`; returns the index of the closing `}`. Inside it a
/// `#` is an operator (`${x#p}`, `${x:- #}`), never a comment, and `;` or `|`
/// separate nothing.
fn scan_parameter_expansion(chars: &[char], mut i: usize, out: &mut Vec<Segment>) -> usize {
    while i < chars.len() {
        match chars[i] {
            '}' => return i,
            '\'' => i = find_char(chars, i + 1, '\'') + 1,
            '"' => i = scan_double_quoted(chars, i + 1, out) + 1,
            '$' if chars.get(i + 1) == Some(&'\'') => i = ansi_c_close(chars, i + 2) + 1,
            _ => match step_over_substitution(chars, i, out) {
                Some(next) => i = next,
                None => i += 1,
            },
        }
    }
    chars.len()
}

fn split_into_span(chars: &[char], from: usize, end: usize, out: &mut Vec<Segment>) {
    let inner: String = chars[from..end.min(chars.len())].iter().collect();
    split_into(&inner, out);
}

/// Scan a double-quoted string starting just past its opening quote,
/// splitting any substitution inside it into `out`; returns the index of
/// the closing quote.
fn scan_double_quoted(chars: &[char], mut i: usize, out: &mut Vec<Segment>) -> usize {
    while i < chars.len() {
        match chars[i] {
            '"' => return i,
            '\\' => i += 2,
            '`' => {
                let end = find_unescaped(chars, i + 1, '`');
                let inner: String = chars[i + 1..end.min(chars.len())].iter().collect();
                split_into(&inner, out);
                i = end + 1;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                let end = find_close_paren(chars, i + 2);
                let inner: String = chars[i + 2..end.min(chars.len())].iter().collect();
                split_into(&inner, out);
                i = end + 1;
            }
            _ => i += 1,
        }
    }
    chars.len()
}
