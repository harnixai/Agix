//! The log file as bytes: reading, torn-tail repair, durable line writes and
//! the directory scans a listing is built from.

use super::super::externalize::hydrate;
use super::super::SessionFacts;
use crate::blob::BlobStore;
use crate::entry::{EnvelopedEntry, SessionKind};
use crate::error::HistoryError;
use crate::path::session_metadata_file;
use crate::project::SessionMetadata;
use base::session::SessionId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(super) fn format_mtime(t: std::time::SystemTime) -> String {
    format_ts(time::OffsetDateTime::from(t))
}

pub(in crate::store) fn format_ts(dt: time::OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| unknown_time())
}

pub(in crate::store) fn unknown_time() -> String {
    "unknown".into()
}

pub(super) enum ProjectDirFilter {
    All,
    UnderPath(PathBuf),
}

impl ProjectDirFilter {
    pub(super) fn matches(&self, metadata: Option<&SessionMetadata>) -> bool {
        match self {
            Self::All => true,
            Self::UnderPath(root) => metadata
                .map(|m| Path::new(&m.canonical_cwd).starts_with(root))
                .unwrap_or(false),
        }
    }
}

/// Every `<session>.jsonl` in one project directory, by id. A directory that
/// isn't there holds no sessions — not an error, just a project nothing has
/// been written for yet.
pub(super) async fn session_ids_in(dir: &Path) -> Result<Vec<SessionId>, HistoryError> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(HistoryError::Io(e)),
    };
    let mut out = Vec::new();
    while let Some(ent) = entries.next_entry().await? {
        let name = ent.file_name();
        let Some(s) = name.to_str() else { continue };
        let Some(stem) = s.strip_suffix(".jsonl") else {
            continue;
        };
        if let Ok(id) = SessionId::parse(stem) {
            out.push(id);
        }
    }
    Ok(out)
}

pub(super) async fn collect_session_files(
    dir: PathBuf,
) -> Result<Vec<(SessionId, PathBuf, std::time::SystemTime)>, HistoryError> {
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(HistoryError::Io(e)),
    };
    let mut out = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(session_id) = SessionId::parse(stem) else {
            continue;
        };
        let mtime = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        out.push((session_id, path, mtime));
    }
    Ok(out)
}

/// Every `<session>.jsonl` in every project directory under `projects_root`.
pub(super) async fn collect_every_project(
    projects_root: &Path,
) -> Result<Vec<(SessionId, PathBuf, std::time::SystemTime)>, HistoryError> {
    let mut project_dirs = match tokio::fs::read_dir(projects_root).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(HistoryError::Io(e)),
    };
    let mut out = Vec::new();
    while let Some(entry) = project_dirs.next_entry().await? {
        if entry.file_type().await.is_ok_and(|ft| ft.is_dir()) {
            out.append(&mut collect_session_files(entry.path()).await?);
        }
    }
    Ok(out)
}

/// A search reads every transcript it reaches, and one that will not read is
/// a property of that session, not a reason to answer nothing.
pub(super) fn skip_unreadable(session: SessionId, error: &HistoryError) {
    tracing::warn!(%session, %error, "leaving an unreadable transcript out of the search");
}

/// A log's text, without a last line that a crash cut short — or `None` when
/// the log is not there. A last line that is whole but lacks only its newline
/// is kept.
pub(super) async fn read_log(path: &Path) -> Result<Option<String>, HistoryError> {
    let mut bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(HistoryError::Io(e)),
    };
    let last_line = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let tail = &bytes[last_line..];
    if !tail.iter().all(u8::is_ascii_whitespace) && !is_whole_entry(tail) {
        tracing::warn!(
            path = %path.display(),
            dropped_bytes = tail.len(),
            "the log's last line was cut short; reading it without that line"
        );
        bytes.truncate(last_line);
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|e| HistoryError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

pub(super) fn is_whole_entry(line: &[u8]) -> bool {
    std::str::from_utf8(line)
        .is_ok_and(|text| serde_json::from_str::<EnvelopedEntry>(text.trim()).is_ok())
}

/// Make the log end at a line break before anything is appended to it.
///
/// A process that died mid-write leaves the last line cut short, and an
/// append after it would weld the next entry onto the fragment — a broken
/// line in the middle of the log, which no reader can tell from corruption.
/// The fragment is cut off; a last line that is whole and lacks only its
/// newline gets one.
pub(super) fn end_on_a_line_break(file: &mut std::fs::File, path: &Path) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    const CHUNK: u64 = 64 * 1024;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    let mut last = [0u8; 1];
    file.seek(SeekFrom::Start(len - 1))?;
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }
    let mut start = 0;
    let mut pos = len;
    let mut chunk = Vec::new();
    while pos > 0 {
        let n = pos.min(CHUNK);
        pos -= n;
        chunk.resize(n as usize, 0);
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut chunk)?;
        if let Some(i) = chunk.iter().rposition(|&b| b == b'\n') {
            start = pos + i as u64 + 1;
            break;
        }
    }
    let mut tail = Vec::new();
    file.seek(SeekFrom::Start(start))?;
    file.read_to_end(&mut tail)?;
    if is_whole_entry(&tail) {
        return file.write_all(b"\n");
    }
    tracing::warn!(
        path = %path.display(),
        dropped_bytes = tail.len(),
        "cutting off a last line that was cut short before appending"
    );
    file.set_len(start)
}

/// fsync `path`, and its directory too unless `dir_done`.
pub(super) fn sync_log(path: &Path, dir_done: bool) -> Result<(), HistoryError> {
    match std::fs::File::open(path) {
        Ok(file) => file.sync_data()?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(HistoryError::Io(e)),
    }
    if !dir_done {
        if let Some(dir) = path.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
    }
    Ok(())
}

/// What this process knows about one session's log.
#[derive(Debug, Default)]
pub(super) struct LogState {
    /// The log's length right after this process last appended to it. A log
    /// still that long ends on the line break that append wrote, and the next
    /// append need not look.
    pub(super) len_after_last_append: Option<u64>,
}

/// Append one line to the log at `path` and return the log's new length.
pub(super) fn write_line(
    path: &Path,
    line: &[u8],
    known_len: Option<u64>,
) -> Result<u64, HistoryError> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)?;
    if Some(f.metadata()?.len()) != known_len {
        end_on_a_line_break(&mut f, path)?;
    }
    #[cfg(not(test))]
    f.write_all(line)?;
    #[cfg(test)]
    super::tests::crash::mid_write::write_all(&mut f, path, line)?;
    f.flush()?;
    Ok(f.metadata()?.len())
}

/// Run blocking file work off the async worker threads.
pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, HistoryError> + Send + 'static,
) -> Result<T, HistoryError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| HistoryError::Io(std::io::Error::other(e)))?
}

/// One log's entries, with references into `blobs` put back.
pub(super) fn parse_log(
    content: &str,
    blobs: Option<&dyn BlobStore>,
) -> Result<Vec<EnvelopedEntry>, HistoryError> {
    let mut entries = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut env = serde_json::from_str::<EnvelopedEntry>(line)
            .map_err(|error| HistoryError::Parse { line: i + 1, error })?;
        if let Some(blobs) = blobs {
            hydrate(&mut env, blobs);
        }
        entries.push(env);
    }
    Ok(entries)
}

/// The first `len` bytes of a log, cut back to the last whole line, or `None`
/// when the file is gone.
pub(super) async fn read_prefix(path: &Path, len: u64) -> Result<Option<String>, HistoryError> {
    use tokio::io::AsyncReadExt;
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(HistoryError::Io(e)),
    };
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    file.take(len).read_to_end(&mut bytes).await?;
    let whole_lines = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    bytes.truncate(whole_lines);
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|e| HistoryError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

/// What the `Meta` line within a log's first `len` bytes says, read no
/// further than that line. A log with no `Meta` has the empty facts; `None`
/// is a log that is gone.
pub(super) async fn head_facts(
    path: &Path,
    len: u64,
) -> Result<Option<SessionFacts>, HistoryError> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(HistoryError::Io(e)),
    };
    let mut lines = tokio::io::BufReader::new(file.take(len));
    let mut line = String::new();
    let mut number = 0;
    loop {
        line.clear();
        number += 1;
        if lines.read_line(&mut line).await? == 0 || !line.ends_with('\n') {
            return Ok(Some(SessionFacts::default()));
        }
        if line.trim().is_empty() {
            continue;
        }
        let env =
            serde_json::from_str::<EnvelopedEntry>(&line).map_err(|error| HistoryError::Parse {
                line: number,
                error,
            })?;
        if let Some(facts) = SessionFacts::of(&env.entry) {
            return Ok(Some(facts));
        }
    }
}

/// `root`'s lineage over a known parent map: its sidechains breadth-first,
/// and the forks met on the way.
pub(in crate::store) fn lineage_of(
    root: SessionId,
    facts: &HashMap<SessionId, SessionFacts>,
) -> (Vec<SessionId>, Vec<SessionId>) {
    let mut children: HashMap<&str, Vec<SessionId>> = HashMap::new();
    for (session, f) in facts {
        if let Some(parent) = f.parent_session_id.as_deref() {
            children.entry(parent).or_default().push(*session);
        }
    }
    for list in children.values_mut() {
        list.sort_by_key(SessionId::to_string);
    }
    let mut members = vec![root];
    let mut forks = Vec::new();
    let mut seen = std::collections::HashSet::from([root]);
    let mut next = 0;
    while next < members.len() {
        let parent = members[next].to_string();
        next += 1;
        for child in children.get(parent.as_str()).into_iter().flatten() {
            if !seen.insert(*child) {
                continue;
            }
            if facts.get(child).map(|f| f.session_kind) == Some(SessionKind::Sidechain) {
                members.push(*child);
            } else {
                forks.push(*child);
            }
        }
    }
    (members, forks)
}

pub(super) async fn load_session_metadata(
    sessions_root: &Path,
    session: SessionId,
) -> Option<SessionMetadata> {
    let path = session_metadata_file(sessions_root, &session);
    let content = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&content).ok()
}
