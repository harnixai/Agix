//! Content search across sessions: where in each session a piece of text was
//! first said by the person or answered by the model.
//!
//! Distinct from [`crate::query`]: that one finds sessions and matches
//! everything a transcript renders to, tool traffic included, because `/resume`
//! wants any session that mentions the text. This one answers "where did we
//! talk about it" — only what the person typed (prompts, steers, command
//! expansions) and the model's text replies count, reminders injected into
//! them are cut out first, and every hit carries the `session.history`
//! position it sits at.

use crate::entry::{EnvelopedEntry, LogEntry, SessionKind};
use crate::transcript::{project_messages_sourced, MessageSource, TranscriptProjection};
use base::message::{ContentBlock, Message};
use base::session::SessionId;

/// What [`crate::store::HistoryStore::search_text`] is asked.
#[derive(Debug, Clone)]
pub struct TextQuery {
    /// Matched trimmed and case-insensitively; an empty needle matches nothing.
    pub text: String,
    /// The sessions to look in, in the order to look in them; hits keep it.
    pub sessions: Vec<SessionId>,
    /// Sub-agent and team-member transcripts are passed over unless set.
    pub include_sidechains: bool,
    /// Stop once this many sessions have a hit.
    pub limit: usize,
    /// Stop before the next session once this has passed.
    pub deadline: Option<std::time::Instant>,
    /// Characters of context in each snippet, the match included.
    pub snippet_chars: usize,
}

/// The first place in one session where the text occurs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextHit {
    pub session_id: SessionId,
    /// The message's position in `session.history`'s coordinates.
    pub seq: usize,
    pub source: MessageSource,
    pub ts: time::OffsetDateTime,
    pub snippet: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextSearch {
    pub hits: Vec<TextHit>,
    /// Sessions read to the end, hit or not.
    pub scanned: usize,
    /// A limit or the deadline stopped the search before the last session.
    pub truncated: bool,
}

/// What the text can be found in: the person's words and the model's replies.
fn searched(source: MessageSource) -> bool {
    matches!(
        source,
        MessageSource::User
            | MessageSource::Steer
            | MessageSource::Command
            | MessageSource::Assistant
    )
}

/// The first hit in one session's log, or `None`.
pub fn first_hit(
    entries: &[EnvelopedEntry],
    projection: &dyn TranscriptProjection,
    needle: &str,
    snippet_chars: usize,
) -> Option<(usize, MessageSource, time::OffsetDateTime, String)> {
    let needle = needle.trim();
    if needle.is_empty() {
        return None;
    }
    let needle = Folded::of(needle);
    for (seq, sourced) in project_messages_sourced(entries, projection)
        .iter()
        .enumerate()
    {
        let Some(source) = sourced.source().filter(|s| searched(*s)) else {
            continue;
        };
        let blocks = match &sourced.message {
            Message::User { content } | Message::Assistant { content, .. } => content,
            Message::System { .. } => continue,
        };
        for block in blocks {
            let ContentBlock::Text { text, .. } = block else {
                continue;
            };
            let said = without_reminders(text);
            if let Some(snippet) = snippet_of(&said, &needle, snippet_chars) {
                return Some((seq, source, sourced.entry.ts, snippet));
            }
        }
    }
    None
}

/// Whether a session's log is a sidechain, by its `Meta` line.
pub fn is_sidechain(entries: &[EnvelopedEntry]) -> bool {
    entries.iter().any(|env| {
        matches!(
            env.entry,
            LogEntry::Meta {
                session_kind: SessionKind::Sidechain,
                ..
            }
        )
    })
}

const REMINDER_OPEN: &str = "<system-reminder>";
const REMINDER_CLOSE: &str = "</system-reminder>";

/// `text` with every `<system-reminder>` span cut out; one left open runs to
/// the end.
fn without_reminders(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(REMINDER_OPEN) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find(REMINDER_OPEN) {
        out.push_str(&rest[..open]);
        let inside = &rest[open + REMINDER_OPEN.len()..];
        match inside.find(REMINDER_CLOSE) {
            Some(close) => rest = &inside[close + REMINDER_CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

/// Text lowercased char by char, remembering which original char each
/// lowered one came from — lowercasing can change how many chars there are,
/// so a match position in the folded text is not one in the original.
struct Folded {
    chars: Vec<char>,
    origin: Vec<usize>,
}

impl Folded {
    fn of(text: &str) -> Self {
        let mut chars = Vec::with_capacity(text.len());
        let mut origin = Vec::with_capacity(text.len());
        for (i, c) in text.chars().enumerate() {
            for lower in c.to_lowercase() {
                chars.push(lower);
                origin.push(i);
            }
        }
        Self { chars, origin }
    }

    /// The original char range of the first occurrence of `needle`.
    fn find(&self, needle: &Folded) -> Option<(usize, usize)> {
        let n = needle.chars.len();
        if n == 0 || n > self.chars.len() {
            return None;
        }
        let at = self.chars.windows(n).position(|w| w == needle.chars)?;
        Some((self.origin[at], self.origin[at + n - 1] + 1))
    }
}

/// `snippet_chars` characters of `text` around the first match of `needle`,
/// whitespace runs collapsed, with `…` where it was cut.
fn snippet_of(text: &str, needle: &Folded, snippet_chars: usize) -> Option<String> {
    let (start, end) = Folded::of(text).find(needle)?;
    let chars: Vec<char> = text.chars().collect();
    let width = snippet_chars.max(end - start);
    let spare = width - (end - start);
    let mut from = start.saturating_sub(spare / 2);
    let to = (from + width).min(chars.len());
    from = to.saturating_sub(width).min(from);
    let window: String = chars[from..to].iter().collect();
    let mut snippet = window.split_whitespace().collect::<Vec<_>>().join(" ");
    if from > 0 {
        snippet.insert(0, '…');
    }
    if to < chars.len() {
        snippet.push('…');
    }
    Some(snippet)
}

/// The default [`crate::store::HistoryStore::search_text`]: every session in
/// order, loaded whole.
pub(crate) async fn scan<S>(
    store: &S,
    query: &TextQuery,
) -> Result<TextSearch, crate::error::HistoryError>
where
    S: crate::store::HistoryStore + ?Sized,
{
    let mut out = TextSearch::default();
    if query.text.trim().is_empty() {
        return Ok(out);
    }
    for sid in &query.sessions {
        let late = query
            .deadline
            .is_some_and(|d| std::time::Instant::now() >= d);
        if out.hits.len() >= query.limit || late {
            out.truncated = true;
            break;
        }
        let entries = match store.load(*sid).await {
            Ok(entries) => entries,
            Err(crate::error::HistoryError::SessionNotFound(_)) => continue,
            Err(e) => return Err(e),
        };
        if !query.include_sidechains && is_sidechain(&entries) {
            continue;
        }
        out.scanned += 1;
        if let Some((seq, source, ts, snippet)) = first_hit(
            &entries,
            store.projection(),
            &query.text,
            query.snippet_chars,
        ) {
            out.hits.push(TextHit {
                session_id: *sid,
                seq,
                source,
                ts,
                snippet,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
