//! The line a tool result saved to a file opens with, and the check that a line
//! really names one of this session's saved files. The runtime's save step writes
//! it; the budget pass, fork, export and import read it. A marker for any other
//! path is only text a tool printed.

use crate::budget::group_thousands;
use std::collections::BTreeSet;
use std::path::Path;

pub const SAVED_MARKER_PREFIX: &str = "[Output too long for one result: ";
const SAVED_TO: &str = " Saved in full to ";
const MARKER_SUFFIX: &str =
    " \u{2014} read the rest with Read (offset/limit are lines) or search it with Grep.]";

pub fn saved_marker(chars: usize, lines: usize, path: &Path) -> String {
    format!(
        "{SAVED_MARKER_PREFIX}{} characters, {lines} lines.{SAVED_TO}{}{MARKER_SUFFIX}",
        group_thousands(chars),
        path.display()
    )
}

fn marker_path_of(content: &str) -> Option<&str> {
    let line = content.strip_prefix(SAVED_MARKER_PREFIX)?.lines().next()?;
    let (_, rest) = line.split_once(SAVED_TO)?;
    rest.strip_suffix(MARKER_SUFFIX)
        .or_else(|| rest.split_once(MARKER_SUFFIX).map(|(path, _)| path))
}

pub fn is_saved_file_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".txt") else {
        return false;
    };
    (1..=200).contains(&name.len())
        && !stem.is_empty()
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The file name a marker for a file directly in `dir` names.
pub fn saved_name_of<'a>(content: &'a str, dir: &Path) -> Option<&'a str> {
    let path = Path::new(marker_path_of(content)?);
    if !path.is_absolute() || path.parent() != Some(dir) {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    is_saved_file_name(name).then_some(name)
}

/// The saved file `content` names, when it opens with a marker for a file in `dir`.
pub fn saved_path_of<'a>(content: &'a str, dir: &Path) -> Option<&'a str> {
    saved_name_of(content, dir)?;
    marker_path_of(content)
}

/// `content` with its marker's file moved from `from` to `to`, when it opens with a
/// marker for a file in `from` whose name is in `moved`; the rest is untouched.
pub fn with_marker_moved(
    content: &str,
    from: &Path,
    to: &Path,
    moved: &BTreeSet<String>,
) -> Option<String> {
    let name = saved_name_of(content, from)?;
    if !moved.contains(name) {
        return None;
    }
    let old = marker_path_of(content)?;
    let start = SAVED_MARKER_PREFIX.len()
        + content[SAVED_MARKER_PREFIX.len()..].find(SAVED_TO)?
        + SAVED_TO.len();
    let mut out = String::with_capacity(content.len());
    out.push_str(&content[..start]);
    out.push_str(&to.join(name).display().to_string());
    out.push_str(&content[start + old.len()..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_for_a_file_in_the_sessions_directory_is_read_back() {
        let dir = Path::new("/d/tool-results/s");
        let marker = saved_marker(31_000, 2, &dir.join("s-toolu_1.txt"));
        assert!(marker.contains("31,000 characters"));
        let text = format!("{marker}\nrest");
        assert_eq!(
            saved_path_of(&text, dir),
            Some("/d/tool-results/s/s-toolu_1.txt")
        );
        assert_eq!(saved_name_of(&text, dir), Some("s-toolu_1.txt"));
        assert_eq!(saved_path_of("plain output", dir), None);
    }

    #[test]
    fn a_marker_naming_another_directory_or_climbing_out_is_not() {
        let dir = Path::new("/d/tool-results/s");
        for path in [
            "/d/tool-results/other/x.txt",
            "/etc/passwd",
            "/d/tool-results/s/../x.txt",
            "/d/tool-results/s/sub/x.txt",
            "tool-results/s/x.txt",
            "/d/tool-results/s/a b.txt",
            "/d/tool-results/s/x.json",
            "/d/tool-results/s/.txt",
        ] {
            let text = saved_marker(1, 1, Path::new(path));
            assert_eq!(saved_path_of(&text, dir), None, "{path}");
        }
    }

    #[test]
    fn moving_a_marker_rewrites_only_its_path() {
        let (from, to) = (Path::new("/a/s"), Path::new("/b/t"));
        let text = format!(
            "{}\nbody mentions /a/s/x.txt too",
            saved_marker(9, 1, &from.join("x.txt"))
        );
        let moved: BTreeSet<String> = ["x.txt".to_string()].into();
        let out = with_marker_moved(&text, from, to, &moved).unwrap();
        assert_eq!(saved_path_of(&out, to), Some("/b/t/x.txt"));
        assert!(out.ends_with("\nbody mentions /a/s/x.txt too"));
        assert_eq!(with_marker_moved(&text, from, to, &BTreeSet::new()), None);
        assert_eq!(with_marker_moved(&text, to, from, &moved), None);
    }
}
