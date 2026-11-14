//! 写文件的工具（`Write` / `Edit` / `NotebookEdit`）在 `tool_result` 帧上报的结构化变更：
//! 前后哈希、字节数、增删行数与 unified diff。给宿主看的，模型那边的文字不受影响；
//! 上限与模型文字里那段 diff 的 100 行各算各的。

use base::tool::{DiffOmitted, FileChange, FileOp};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const HASHED_UP_TO: u64 = 16 * 1024 * 1024;
pub const DIFFED_UP_TO: u64 = 1024 * 1024;
pub const DIFF_TEXT_CAP: usize = 256 * 1024;
pub const CHANGED_LINES_CAP: usize = 2000;

/// What the file held before the write.
#[derive(Debug, Clone, Copy)]
pub enum Before<'a> {
    Absent,
    Content(&'a [u8]),
    /// There, but too large to have been read.
    Unread {
        bytes: u64,
    },
}

pub fn sha256(content: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(content))
}

pub fn describe(path: &Path, before: Before<'_>, after: &[u8], root: &Path) -> FileChange {
    let after_bytes = after.len() as u64;
    let mut change = FileChange::new(
        path.display().to_string(),
        match before {
            Before::Absent => FileOp::Create,
            Before::Content(_) | Before::Unread { .. } => FileOp::Modify,
        },
        after_bytes,
    );
    change.before_bytes = match before {
        Before::Absent => None,
        Before::Content(content) => Some(content.len() as u64),
        Before::Unread { bytes } => Some(bytes),
    };
    let old: Option<&[u8]> = match before {
        Before::Absent => Some(&[]),
        Before::Content(content) => Some(content),
        Before::Unread { .. } => None,
    };
    if let Before::Content(content) = before {
        if content.len() as u64 <= HASHED_UP_TO {
            change.before_hash = Some(sha256(content));
        }
    }
    if after_bytes <= HASHED_UP_TO {
        change.after_hash = Some(sha256(after));
    }

    let small = |bytes: u64| bytes <= DIFFED_UP_TO;
    let Some(old) = old.filter(|old| small(old.len() as u64) && small(after_bytes)) else {
        change.omitted = Some(DiffOmitted::TooLarge);
        return change;
    };
    let (Ok(old), Ok(new)) = (std::str::from_utf8(old), std::str::from_utf8(after)) else {
        change.omitted = Some(DiffOmitted::Binary);
        return change;
    };
    let (shown_before, shown_after) = match path.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => (
            format!("a/{}", rel.display()),
            format!("b/{}", rel.display()),
        ),
        _ => (path.display().to_string(), path.display().to_string()),
    };
    let shown_before = match before {
        Before::Absent => "/dev/null".to_string(),
        _ => shown_before,
    };
    let diff = unified(old, new, &shown_before, &shown_after);
    change.added = diff.added;
    change.removed = diff.removed;
    change.truncated = diff.truncated;
    change.unified_diff = Some(diff.text);
    change
}

struct Unified {
    text: String,
    added: u64,
    removed: u64,
    truncated: bool,
}

fn unified(old: &str, new: &str, shown_before: &str, shown_after: &str) -> Unified {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut out = Unified {
        text: format!("--- {shown_before}\n+++ {shown_after}\n"),
        added: 0,
        removed: 0,
        truncated: false,
    };
    let mut changed_shown = 0usize;
    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        let mut changed = 0usize;
        for change in hunk.iter_changes() {
            match change.tag() {
                ChangeTag::Insert => out.added += 1,
                ChangeTag::Delete => out.removed += 1,
                ChangeTag::Equal => continue,
            }
            changed += 1;
        }
        if out.truncated {
            continue;
        }
        let text = hunk.to_string();
        if out.text.len() + text.len() > DIFF_TEXT_CAP
            || changed_shown + changed > CHANGED_LINES_CAP
        {
            out.truncated = true;
            continue;
        }
        changed_shown += changed;
        out.text.push_str(&text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies a unified diff as `patch` would, to check it says what changed.
    fn apply(before: &str, diff: &str) -> String {
        let old: Vec<&str> = before.split_inclusive('\n').collect();
        let mut out = String::new();
        let mut at = 0usize;
        let mut lines = diff.lines().skip(2).peekable();
        while let Some(header) = lines.next() {
            let start: usize = header
                .trim_start_matches("@@ -")
                .split([',', ' '])
                .next()
                .unwrap()
                .parse()
                .unwrap();
            let start = start.saturating_sub(1);
            while at < start {
                out.push_str(old[at]);
                at += 1;
            }
            while let Some(line) = lines.peek().copied() {
                if line.starts_with("@@") {
                    break;
                }
                lines.next();
                let no_newline = lines.peek().is_some_and(|l| l.starts_with('\\'));
                if no_newline {
                    lines.next();
                }
                let body = &line[1..];
                let eol = if no_newline { "" } else { "\n" };
                match &line[..1] {
                    " " => {
                        out.push_str(body);
                        out.push_str(eol);
                        at += 1;
                    }
                    "-" => at += 1,
                    "+" => {
                        out.push_str(body);
                        out.push_str(eol);
                    }
                    other => panic!("not a hunk line: {other:?}"),
                }
            }
        }
        while at < old.len() {
            out.push_str(old[at]);
            at += 1;
        }
        out
    }

    fn root() -> &'static Path {
        Path::new("/repo")
    }

    #[test]
    fn a_modification_carries_both_hashes_and_a_diff_that_reproduces_it() {
        let before = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
        let after = "one\ntwo\nTHREE\nfour\nfive\nsix\nseven\neight\nnine";
        let change = describe(
            Path::new("/repo/src/a.txt"),
            Before::Content(before.as_bytes()),
            after.as_bytes(),
            root(),
        );
        assert_eq!(change.op, FileOp::Modify);
        assert_eq!(change.before_hash, Some(sha256(before.as_bytes())));
        assert_eq!(change.after_hash, Some(sha256(after.as_bytes())));
        assert_eq!(change.before_bytes, Some(before.len() as u64));
        let diff = change.unified_diff.as_deref().unwrap();
        assert!(
            diff.starts_with("--- a/src/a.txt\n+++ b/src/a.txt\n@@ "),
            "{diff}"
        );
        assert_eq!(apply(before, diff), after);
        assert_eq!((change.added, change.removed), (2, 1));
        assert!(!change.truncated && change.omitted.is_none());
    }

    #[test]
    fn a_new_file_is_a_create_from_nothing() {
        let change = describe(
            Path::new("/elsewhere/new.txt"),
            Before::Absent,
            b"hi\n",
            root(),
        );
        assert_eq!(change.op, FileOp::Create);
        assert_eq!(change.before_hash, None);
        assert_eq!(change.before_bytes, None);
        let diff = change.unified_diff.unwrap();
        assert!(
            diff.starts_with("--- /dev/null\n+++ /elsewhere/new.txt\n"),
            "{diff}"
        );
        assert_eq!(apply("", &diff), "hi\n");
    }

    #[test]
    fn content_past_the_diff_limit_or_not_text_carries_hashes_only() {
        let big = "x\n".repeat((DIFFED_UP_TO as usize) / 2 + 1);
        let change = describe(
            Path::new("/repo/big"),
            Before::Content(b"x\n"),
            big.as_bytes(),
            root(),
        );
        assert_eq!(change.omitted, Some(DiffOmitted::TooLarge));
        assert!(change.unified_diff.is_none() && change.after_hash.is_some());

        let change = describe(
            Path::new("/repo/bin"),
            Before::Content(&[0xff, 0xfe]),
            b"text",
            root(),
        );
        assert_eq!(change.omitted, Some(DiffOmitted::Binary));
        assert!(change.before_hash.is_some() && change.after_hash.is_some());

        let change = describe(
            Path::new("/repo/huge"),
            Before::Unread {
                bytes: HASHED_UP_TO + 1,
            },
            b"",
            root(),
        );
        assert_eq!(change.before_hash, None);
        assert_eq!(change.before_bytes, Some(HASHED_UP_TO + 1));
        assert_eq!(change.omitted, Some(DiffOmitted::TooLarge));
    }

    #[test]
    fn a_long_change_stops_at_a_hunk_boundary_and_still_counts_every_line() {
        let lines = CHANGED_LINES_CAP + 1000;
        let before: String = (0..lines).map(|i| format!("{i}\n\n\n\n\n\n\n\n")).collect();
        let after: String = (0..lines)
            .map(|i| format!("{i}!\n\n\n\n\n\n\n\n"))
            .collect();
        let change = describe(
            Path::new("/repo/f"),
            Before::Content(before.as_bytes()),
            after.as_bytes(),
            root(),
        );
        assert!(change.truncated);
        assert_eq!((change.added, change.removed), (lines as u64, lines as u64));
        let diff = change.unified_diff.unwrap();
        assert!(diff.len() <= DIFF_TEXT_CAP);
        let shown = diff
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .count();
        assert!(shown <= CHANGED_LINES_CAP && shown > 0, "{shown}");
        let last_hunk = diff.rfind("\n@@ ").unwrap();
        let tail = &diff[last_hunk + 1..];
        let header_end = tail.find('\n').unwrap();
        let header = &tail[..header_end];
        let counted = tail[header_end + 1..]
            .lines()
            .filter(|l| !l.starts_with('+'))
            .count();
        let declared: usize = header
            .trim_start_matches("@@ -")
            .split(' ')
            .next()
            .unwrap()
            .split(',')
            .nth(1)
            .map_or(1, |n| n.parse().unwrap());
        assert_eq!(counted, declared, "the last hunk shown is whole: {header}");
    }
}
