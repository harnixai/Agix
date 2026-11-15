//! File operations on a session's directory of saved tool results: write one,
//! put one in from an archive, list, copy between sessions, remove what no
//! transcript refers to. Directory and file permissions and the refusal to write
//! through a symlink live here so every writer gets them. All functions block;
//! callers run them on a blocking thread.

use base::message::{ContentBlock, Message, ToolResultContent};
use compaction::saved::{is_saved_file_name, saved_name_of, with_marker_moved};
use history::entry::LogEntry;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

fn prepare(dir: &Path) -> std::io::Result<()> {
    create_private_dir(dir)?;
    for level in [Some(dir), dir.parent()].into_iter().flatten() {
        if std::fs::symlink_metadata(level)?.file_type().is_symlink() {
            return Err(std::io::Error::other(format!(
                "{} is a symlink; a saved result is not written through one",
                level.display()
            )));
        }
    }
    let ignore = dir.join(".ignore");
    if !ignore.exists() {
        base::fs::write_atomic(&ignore, b"*\n", base::fs::WriteOpts::PRIVATE)?;
    }
    Ok(())
}

/// Write one saved result as `<stem>.txt`, or `<stem>-<n>.txt` when taken.
pub fn write(dir: &Path, stem: &str, text: &str) -> std::io::Result<PathBuf> {
    prepare(dir)?;
    let path = (0..)
        .map(|n| match n {
            0 => dir.join(format!("{stem}.txt")),
            n => dir.join(format!("{stem}-{n}.txt")),
        })
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .unwrap_or_else(|| dir.join(format!("{stem}.txt")));
    base::fs::write_atomic(&path, text.as_bytes(), base::fs::WriteOpts::PRIVATE)?;
    Ok(path)
}

/// Put `bytes` at `dir/name`, refusing an existing file, a symlinked `dir` or
/// parent, and a name that is not a saved file's.
pub fn put(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    if !is_saved_file_name(name) {
        return Err(std::io::Error::other(format!(
            "{name:?} is not a saved result's name"
        )));
    }
    prepare(dir)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(dir.join(name))?.write_all(bytes)
}

/// The saved files in `dir`: regular files whose names pass `is_saved_file_name`.
/// A missing or symlinked `dir` has none.
pub fn files_in(dir: &Path) -> Vec<String> {
    if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return Vec::new();
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| is_saved_file_name(name))
        .collect()
}

/// Hard-link (else copy) `names` from `from` into `to`; the names that made it.
pub fn link_into(from: &Path, to: &Path, names: &BTreeSet<String>) -> BTreeSet<String> {
    let mut done = BTreeSet::new();
    if names.is_empty() || prepare(to).is_err() {
        return done;
    }
    for name in names.iter().filter(|n| is_saved_file_name(n)) {
        let (src, dst) = (from.join(name), to.join(name));
        if !std::fs::symlink_metadata(&src).is_ok_and(|m| m.is_file()) {
            continue;
        }
        if std::fs::symlink_metadata(&dst).is_ok() {
            continue;
        }
        let linked = std::fs::hard_link(&src, &dst).is_ok()
            || std::fs::copy(&src, &dst).is_ok_and(|_| {
                #[cfg(unix)]
                let _ = std::fs::set_permissions(
                    &dst,
                    std::os::unix::fs::PermissionsExt::from_mode(0o600),
                );
                true
            });
        if linked {
            done.insert(name.clone());
        }
    }
    done
}

/// Remove the saved files in `dir` not in `keep`; how many went.
pub fn remove_all_but(dir: &Path, keep: &BTreeSet<String>) -> usize {
    files_in(dir)
        .into_iter()
        .filter(|name| !keep.contains(name))
        .filter(|name| std::fs::remove_file(dir.join(name)).is_ok())
        .count()
}

fn tool_result_texts<'a>(entry: &'a LogEntry, out: &mut Vec<&'a str>) {
    fn text_of(content: &ToolResultContent) -> Option<&str> {
        match content {
            ToolResultContent::Text(s) => Some(s),
            ToolResultContent::Blocks(v) => v.iter().find_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            }),
        }
    }
    fn of_blocks<'a>(blocks: &'a [ContentBlock], out: &mut Vec<&'a str>) {
        for block in blocks {
            if let ContentBlock::ToolResult { content, .. } = block {
                out.extend(text_of(content));
            }
        }
    }
    match entry {
        LogEntry::ToolResult { content, .. } => out.extend(text_of(content)),
        LogEntry::User { content } | LogEntry::Assistant { content, .. } => of_blocks(content, out),
        LogEntry::Compact {
            replacement_history: Some(messages),
            ..
        } => {
            for message in messages {
                if let Message::User { content } | Message::Assistant { content, .. } = message {
                    of_blocks(content, out);
                }
            }
        }
        _ => {}
    }
}

fn for_each_tool_result_text(entry: &mut LogEntry, mut f: impl FnMut(&mut String)) {
    fn text_of(content: &mut ToolResultContent) -> Option<&mut String> {
        match content {
            ToolResultContent::Text(s) => Some(s),
            ToolResultContent::Blocks(v) => v.iter_mut().find_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text),
                _ => None,
            }),
        }
    }
    fn of_blocks(blocks: &mut [ContentBlock], f: &mut impl FnMut(&mut String)) {
        for block in blocks {
            if let ContentBlock::ToolResult { content, .. } = block {
                text_of(content).into_iter().for_each(&mut *f);
            }
        }
    }
    match entry {
        LogEntry::ToolResult { content, .. } => text_of(content).into_iter().for_each(f),
        LogEntry::User { content } | LogEntry::Assistant { content, .. } => {
            of_blocks(content, &mut f)
        }
        LogEntry::Compact {
            replacement_history: Some(messages),
            ..
        } => {
            for message in messages {
                if let Message::User { content } | Message::Assistant { content, .. } = message {
                    of_blocks(content, &mut f);
                }
            }
        }
        _ => {}
    }
}

/// The names of files in `dir` that valid markers in `entries` point at.
pub fn marked_in<'a>(
    entries: impl IntoIterator<Item = &'a LogEntry>,
    dir: &Path,
) -> BTreeSet<String> {
    let mut texts = Vec::new();
    for entry in entries {
        tool_result_texts(entry, &mut texts);
    }
    texts
        .into_iter()
        .filter_map(|t| saved_name_of(t, dir).map(str::to_string))
        .collect()
}

/// The names of files in `dir` that any text in `entries` mentions as `<dir>/<name>`.
/// Wider than [`marked_in`]: a `Read` call's path or a summary counts. For
/// cleanup only, where deleting too little is the safe error.
pub fn mentioned_in<'a>(
    entries: impl IntoIterator<Item = &'a LogEntry>,
    dir: &Path,
) -> BTreeSet<String> {
    fn walk(value: &serde_json::Value, needle: &str, found: &mut BTreeSet<String>) {
        match value {
            serde_json::Value::String(s) => {
                for (at, _) in s.match_indices(needle) {
                    let name: String = s[at + needle.len()..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                        .collect();
                    if is_saved_file_name(&name) {
                        found.insert(name);
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| walk(v, needle, found)),
            serde_json::Value::Object(map) => map.values().for_each(|v| walk(v, needle, found)),
            _ => {}
        }
    }
    let needle = format!("{}/", dir.display());
    let mut found = BTreeSet::new();
    for entry in entries {
        if let Ok(value) = serde_json::to_value(entry) {
            walk(&value, &needle, &mut found);
        }
    }
    found
}

/// `entry` with every valid marker for a file of `from` in `moved` pointing into `to`.
pub fn move_markers(
    entry: &mut LogEntry,
    from: &Path,
    to: &Path,
    moved: &BTreeSet<String>,
) -> usize {
    let mut count = 0;
    for_each_tool_result_text(entry, |text| {
        if let Some(new) = with_marker_moved(text, from, to, moved) {
            *text = new;
            count += 1;
        }
    });
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use compaction::saved::saved_marker;

    fn result(text: String) -> LogEntry {
        LogEntry::ToolResult {
            tool_use_id: "t".into(),
            content: ToolResultContent::Text(text),
            is_error: false,
        }
    }

    #[test]
    fn put_refuses_an_existing_file_and_a_symlinked_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tool-results/s");
        put(&dir, "a.txt", b"one").unwrap();
        assert!(put(&dir, "a.txt", b"two").is_err());
        assert!(put(&dir, "../a.txt", b"x").is_err());
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"one");
        #[cfg(unix)]
        {
            let outside = tmp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            let linked = tmp.path().join("tool-results/l");
            std::os::unix::fs::symlink(&outside, &linked).unwrap();
            assert!(put(&linked, "a.txt", b"x").is_err());
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        }
    }

    #[test]
    fn link_into_skips_a_missing_source_and_links_a_present_one() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (
            tmp.path().join("tool-results/a"),
            tmp.path().join("tool-results/b"),
        );
        put(&from, "x.txt", b"body").unwrap();
        let names: BTreeSet<String> = ["x.txt", "gone.txt"].map(String::from).into();
        let done = link_into(&from, &to, &names);
        assert_eq!(done, ["x.txt".to_string()].into());
        assert_eq!(std::fs::read(to.join("x.txt")).unwrap(), b"body");
    }

    #[test]
    fn remove_all_but_keeps_the_ignore_file_and_listed_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tool-results/s");
        for name in ["keep.txt", "drop.txt"] {
            put(&dir, name, b"x").unwrap();
        }
        let removed = remove_all_but(&dir, &["keep.txt".to_string()].into());
        assert_eq!(removed, 1);
        assert!(dir.join("keep.txt").exists() && dir.join(".ignore").exists());
        assert!(!dir.join("drop.txt").exists());
    }

    #[test]
    fn marked_in_sees_markers_inside_a_compaction_replacement() {
        let dir = Path::new("/d/tool-results/s");
        let marker = saved_marker(9, 1, &dir.join("x.txt"));
        let entry = LogEntry::Compact {
            before_tokens: 1,
            after_tokens: 1,
            summary_block_id: None,
            replacement_history: Some(vec![Message::User {
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t".into(),
                    content: ToolResultContent::Text(marker),
                    is_error: false,
                }],
            }]),
            summary: None,
            snip_removed_uuids: None,
        };
        assert_eq!(marked_in([&entry], dir), ["x.txt".to_string()].into());
        let mut moved_entry = entry.clone();
        let to = Path::new("/e/tool-results/t");
        let moved = ["x.txt".to_string()].into();
        assert_eq!(move_markers(&mut moved_entry, dir, to, &moved), 1);
        assert_eq!(marked_in([&moved_entry], to), moved);
    }

    #[test]
    fn mentioned_in_counts_a_path_in_a_read_call() {
        let dir = Path::new("/d/tool-results/s");
        let entry = LogEntry::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "u".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "/d/tool-results/s/y.txt", "limit": 2}),
            }],
            stop_reason: None,
            usage: None,
            model: None,
        };
        assert_eq!(mentioned_in([&entry], dir), ["y.txt".to_string()].into());
        assert!(marked_in([&entry], dir).is_empty());
        assert!(mentioned_in([&result("nothing".into())], dir).is_empty());
    }
}
