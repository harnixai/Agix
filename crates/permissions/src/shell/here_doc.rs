//! Here-documents for the line splitter: where a body starts and ends, so
//! the quotes and `#` in it are not read as the shell's.

use super::{find_char, is_blank};

/// The here-documents of a line: those whose operator has been read and
/// whose body starts at the next newline, then those whose body is being
/// read. A body runs up to its delimiter line, and in it quotes and `#` are
/// ordinary characters while `$(...)`, `${...}` and backquotes still run.
/// Its lines are still split as commands: that can only add segments, and
/// a delimiter read wrong costs prompts, not a hidden command.
#[derive(Default)]
pub(super) struct HereDocs {
    waiting: Vec<HereDoc>,
    reading: std::collections::VecDeque<HereDoc>,
    at_line_start: bool,
}

struct HereDoc {
    delimiter: String,
    strip_tabs: bool,
}

impl HereDocs {
    pub(super) fn in_body(&self) -> bool {
        !self.reading.is_empty()
    }

    pub(super) fn line_ended(&mut self) {
        self.reading.extend(self.waiting.drain(..));
        self.at_line_start = true;
    }

    /// When `i` starts a line that closes the body being read, the index
    /// just past that line.
    pub(super) fn end_at_line_start(&mut self, chars: &[char], i: usize) -> Option<usize> {
        if !std::mem::take(&mut self.at_line_start) {
            return None;
        }
        let doc = self.reading.front()?;
        let end = find_char(chars, i, '\n');
        let mut line = &chars[i..end];
        if doc.strip_tabs {
            while let [first, rest @ ..] = line {
                if *first != '\t' {
                    break;
                }
                line = rest;
            }
        }
        if !line.iter().copied().eq(doc.delimiter.chars()) {
            return None;
        }
        self.reading.pop_front();
        self.at_line_start = true;
        Some((end + 1).min(chars.len()))
    }

    /// When `chars[i]` starts a `<<` or `<<-` operator with its delimiter
    /// word, note the document and return the index just past the word.
    pub(super) fn open_at(&mut self, chars: &[char], i: usize) -> Option<usize> {
        if chars[i] != '<' || chars.get(i + 1) != Some(&'<') || chars.get(i + 2) == Some(&'<') {
            return None;
        }
        let mut j = i + 2;
        let strip_tabs = chars.get(j) == Some(&'-');
        if strip_tabs {
            j += 1;
        }
        while chars.get(j).is_some_and(|&c| c == ' ' || c == '\t') {
            j += 1;
        }
        let mut delimiter = String::new();
        while let Some(&c) = chars.get(j) {
            match c {
                c if is_blank(c) || matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')') => break,
                // A delimiter the scanner might read differently from the
                // shell could swallow the rest of the line as a body.
                '$' | '`' => return None,
                '\'' => {
                    let end = find_char(chars, j + 1, '\'');
                    delimiter.extend(&chars[j + 1..end.min(chars.len())]);
                    j = end + 1;
                }
                '"' => {
                    let end = find_char(chars, j + 1, '"');
                    let inner = &chars[j + 1..end.min(chars.len())];
                    if inner.contains(&'\\') {
                        return None;
                    }
                    delimiter.extend(inner);
                    j = end + 1;
                }
                '\\' => {
                    delimiter.extend(chars.get(j + 1));
                    j += 2;
                }
                c => {
                    delimiter.push(c);
                    j += 1;
                }
            }
        }
        if delimiter.is_empty() || delimiter.contains('\n') {
            return None;
        }
        self.waiting.push(HereDoc {
            delimiter,
            strip_tabs,
        });
        Some(j.min(chars.len()))
    }
}
