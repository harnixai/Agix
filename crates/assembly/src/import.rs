//! `session.import`: the inverse of `session.export`. An archive (zip or
//! directory) is read, checked end to end, and rewritten into new sessions
//! whose transcripts keep every line's id, timestamp and tags, so a
//! compaction's references into earlier lines still resolve.
//!
//! What is rewritten, and nothing else: each line's `session_id` and a `Meta`
//! line's `parent_session_id`, `cwd` and `project_root`. Text
//! in messages and tool results that names an old session id or path is left
//! as it was written. Nothing is read from the archive outside its layout:
//! `manifest.json`, `sessions/<id>/transcript.jsonl` and
//! `sessions/<id>/session_memory.md`, and `tool-results/<name>` for the
//! saved tool results the manifest lists. The views (`history.json`,
//! `entries.json`), `runtime.json`, attachments, recordings and checkpoint
//! copies are not restored, and the pool leaves out the checkpoint records and
//! the recorded spec (`carried_by_an_import`).
//!
//! All blocking file I/O; the pool calls it on a blocking thread.

use crate::export::{FORMAT, FORMAT_VERSION};
use crate::session_pool::api::PoolError;
use base::session::SessionId;
use history::entry::{
    EnvelopedEntry, LogEntry, SessionKind, CURRENT_META_SCHEMA_VERSION, CURRENT_SCHEMA_VERSION,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub use crate::rpc::methods::session::ProjectRootParam;

const MAX_ENTRIES: usize = 100_000;
const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ImportRequest {
    pub path: PathBuf,
    pub project_root: ProjectRootParam,
    pub keep_ids: bool,
}

#[derive(Debug)]
pub enum ImportError {
    Invalid(String),
    Failed(String),
}

impl From<ImportError> for PoolError {
    fn from(e: ImportError) -> Self {
        match e {
            ImportError::Invalid(m) => PoolError::InvalidRequest(m),
            ImportError::Failed(m) => PoolError::Internal(m),
        }
    }
}

fn invalid<T>(message: impl Into<String>) -> Result<T, ImportError> {
    Err(ImportError::Invalid(message.into()))
}

pub(crate) struct ArchivedSession {
    pub(crate) id: SessionId,
    pub(crate) entries: Vec<EnvelopedEntry>,
    pub(crate) memory: Option<String>,
    pub(crate) message_count: Option<u64>,
}

/// The saved tool results an archive carries, with the directory its
/// transcripts' markers name.
pub(crate) struct SavedInArchive {
    pub(crate) dir: PathBuf,
    pub(crate) files: Vec<(String, Vec<u8>)>,
    pub(crate) missing: Vec<String>,
}

pub(crate) struct Archive {
    pub(crate) root: SessionId,
    pub(crate) sessions: Vec<ArchivedSession>,
    pub(crate) saved: Option<SavedInArchive>,
}

impl Archive {
    pub(crate) fn meta_of(&self, id: SessionId) -> Option<&LogEntry> {
        let session = self.sessions.iter().find(|s| s.id == id)?;
        session
            .entries
            .first()
            .map(|e| &e.entry)
            .filter(|e| matches!(e, LogEntry::Meta { .. }))
    }
}

/// Refuse a source before anything is read: absolute, an existing file or
/// directory, and not inside the session history store, where the files it
/// holds would read as sessions.
pub fn check_source(path: &Path, history_roots: &[PathBuf]) -> Result<(), ImportError> {
    if !path.is_absolute() {
        return invalid(format!("path must be absolute: {}", path.display()));
    }
    let meta = std::fs::symlink_metadata(path)
        .map_err(|_| ImportError::Invalid(format!("{} does not exist", path.display())))?;
    if meta.file_type().is_symlink() || !(meta.is_file() || meta.is_dir()) {
        return invalid(format!(
            "{} is not a zip file or an export directory",
            path.display()
        ));
    }
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    for root in history_roots {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if real.starts_with(&root) {
            return invalid(format!(
                "{} is inside the session history store ({})",
                path.display(),
                root.display()
            ));
        }
    }
    Ok(())
}

/// What a source holds that an import reads, by layout name.
trait Source {
    fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>, ImportError>;
}

struct Budgeted<'a> {
    inner: &'a mut dyn Source,
    left: u64,
}

impl Source for Budgeted<'_> {
    fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>, ImportError> {
        let bytes = self.inner.read(name)?;
        let read = bytes.as_ref().map_or(0, |b| b.len() as u64);
        self.left = match self.left.checked_sub(read) {
            Some(left) => left,
            None => {
                return invalid(format!(
                    "the archive unpacks to more than {MAX_TOTAL_BYTES} bytes"
                ))
            }
        };
        Ok(bytes)
    }
}

fn bounded(mut from: impl Read, name: &str) -> Result<Vec<u8>, ImportError> {
    let mut bytes = Vec::new();
    from.by_ref()
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ImportError::Failed(format!("could not read {name}: {e}")))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return invalid(format!("{name} is larger than {MAX_FILE_BYTES} bytes"));
    }
    Ok(bytes)
}

struct ZipSource {
    archive: zip::ZipArchive<std::fs::File>,
}

impl ZipSource {
    fn open(path: &Path) -> Result<Self, ImportError> {
        let file = std::fs::File::open(path)
            .map_err(|e| ImportError::Failed(format!("could not open the archive: {e}")))?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|e| ImportError::Invalid(format!("not a readable zip archive: {e}")))?;
        if archive.len() > MAX_ENTRIES {
            return invalid(format!("the archive has more than {MAX_ENTRIES} entries"));
        }
        let mut total = 0u64;
        for i in 0..archive.len() {
            let entry = archive
                .by_index_raw(i)
                .map_err(|e| ImportError::Invalid(format!("unreadable zip entry: {e}")))?;
            if entry.enclosed_name().is_none() {
                return invalid(format!("zip entry `{}` leaves the archive", entry.name()));
            }
            if entry.is_symlink() {
                return invalid(format!("zip entry `{}` is a symbolic link", entry.name()));
            }
            total = total.saturating_add(entry.size());
        }
        if total > MAX_TOTAL_BYTES {
            return invalid(format!(
                "the archive unpacks to more than {MAX_TOTAL_BYTES} bytes"
            ));
        }
        Ok(Self { archive })
    }
}

impl Source for ZipSource {
    fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>, ImportError> {
        match self.archive.by_name(name) {
            Ok(file) => bounded(file, name).map(Some),
            Err(zip::result::ZipError::FileNotFound) => Ok(None),
            Err(e) => invalid(format!("unreadable zip entry `{name}`: {e}")),
        }
    }
}

struct DirSource {
    root: PathBuf,
}

impl DirSource {
    fn open(root: &Path) -> Result<Self, ImportError> {
        let mut count = 0usize;
        let mut total = 0u64;
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let entries = std::fs::read_dir(&dir).map_err(|e| {
                ImportError::Failed(format!("could not read {}: {e}", dir.display()))
            })?;
            for entry in entries.flatten() {
                count += 1;
                if count > MAX_ENTRIES {
                    return invalid(format!("the directory has more than {MAX_ENTRIES} entries"));
                }
                let meta = std::fs::symlink_metadata(entry.path())
                    .map_err(|e| ImportError::Failed(e.to_string()))?;
                if meta.file_type().is_symlink() {
                    return invalid(format!("{} is a symbolic link", entry.path().display()));
                }
                if meta.is_dir() {
                    pending.push(entry.path());
                } else {
                    total = total.saturating_add(meta.len());
                }
            }
        }
        if total > MAX_TOTAL_BYTES {
            return invalid(format!(
                "the directory holds more than {MAX_TOTAL_BYTES} bytes"
            ));
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }
}

impl Source for DirSource {
    fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>, ImportError> {
        let path = self.root.join(name);
        match std::fs::File::open(&path) {
            Ok(file) => bounded(file, name).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ImportError::Failed(format!("could not read {name}: {e}"))),
        }
    }
}

fn layout_name(session: &str, file: &str) -> Result<String, ImportError> {
    let name = format!("sessions/{session}/{file}");
    let clean = Path::new(&name)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if clean && SessionId::parse(session).is_ok() {
        Ok(name)
    } else {
        invalid(format!("`{session}` is not a session id of this layout"))
    }
}

fn saved_result_layout_name(name: &str) -> Result<String, ImportError> {
    if runtime::saved_results::is_saved_file_name(name) {
        Ok(format!("tool-results/{name}"))
    } else {
        invalid(format!("`{name}` is not a saved tool result's name"))
    }
}

fn saved_results_of(
    source: &mut dyn Source,
    section: &Value,
) -> Result<Option<SavedInArchive>, ImportError> {
    if !section.is_object() {
        return Ok(None);
    }
    let dir = section["dir"]
        .as_str()
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
        .ok_or_else(|| ImportError::Invalid("tool_results.dir is not an absolute path".into()))?;
    let (mut files, mut missing) = (Vec::new(), Vec::new());
    for row in section["files"].as_array().into_iter().flatten() {
        let name = row["name"].as_str().unwrap_or_default();
        let layout = saved_result_layout_name(name)?;
        let Some(bytes) = source.read(&layout)? else {
            missing.push(name.to_string());
            continue;
        };
        if row["sha256"].as_str() != Some(hex::encode(Sha256::digest(&bytes)).as_str()) {
            return invalid(format!("{layout} does not match its recorded checksum"));
        }
        files.push((name.to_string(), bytes));
    }
    Ok(Some(SavedInArchive {
        dir,
        files,
        missing,
    }))
}

fn manifest_of(source: &mut dyn Source) -> Result<Value, ImportError> {
    let bytes = source
        .read("manifest.json")?
        .ok_or_else(|| ImportError::Invalid("the archive has no manifest.json".into()))?;
    let manifest: Value = serde_json::from_slice(&bytes)
        .map_err(|e| ImportError::Invalid(format!("manifest.json is not JSON: {e}")))?;
    if manifest["format"] != FORMAT {
        return invalid(format!(
            "not a session export: its format is not `{FORMAT}`"
        ));
    }
    match manifest["format_version"].as_u64() {
        Some(v) if v == u64::from(FORMAT_VERSION) => {}
        Some(v) if v > u64::from(FORMAT_VERSION) => {
            return invalid(format!(
                "this archive is format version {v}, from a newer Agix; this one reads version {FORMAT_VERSION}"
            ))
        }
        _ => return invalid("the manifest has no readable format_version"),
    }
    if manifest["importable"] != true {
        return invalid(
            "this archive cannot be imported: it was exported without its attachments, or holds \
             transcript lines whose content is stored elsewhere",
        );
    }
    Ok(manifest)
}

fn session_of(source: &mut dyn Source, row: &Value) -> Result<ArchivedSession, ImportError> {
    let sid = row["session_id"].as_str().unwrap_or_default();
    let id = SessionId::parse(sid)
        .map_err(|_| ImportError::Invalid(format!("`{sid}` is not a session id")))?;
    let transcript_name = layout_name(sid, "transcript.jsonl")?;
    let raw = source
        .read(&transcript_name)?
        .ok_or_else(|| ImportError::Invalid(format!("{transcript_name} is missing")))?;
    let digest = hex::encode(Sha256::digest(&raw));
    if row["transcript_sha256"].as_str() != Some(digest.as_str()) {
        return invalid(format!(
            "{transcript_name} does not match its recorded checksum"
        ));
    }
    let text = String::from_utf8(raw)
        .map_err(|_| ImportError::Invalid(format!("{transcript_name} is not UTF-8")))?;
    let mut entries = Vec::new();
    for (n, line) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let entry: EnvelopedEntry = serde_json::from_str(line)
            .map_err(|e| ImportError::Invalid(format!("{transcript_name} line {}: {e}", n + 1)))?;
        if entry.v > CURRENT_SCHEMA_VERSION {
            return invalid(format!(
                "{transcript_name} line {} has schema version {}, newer than this Agix reads ({CURRENT_SCHEMA_VERSION})",
                n + 1,
                entry.v
            ));
        }
        entries.push(entry);
    }
    match entries.first().map(|e| &e.entry) {
        Some(LogEntry::Meta { schema_version, .. }) if *schema_version <= CURRENT_META_SCHEMA_VERSION => {}
        Some(LogEntry::Meta { schema_version, .. }) => {
            return invalid(format!(
                "{transcript_name} has meta schema version {schema_version}, newer than this Agix reads ({CURRENT_META_SCHEMA_VERSION})"
            ))
        }
        _ => return invalid(format!("{transcript_name} does not start with its Meta line")),
    }
    let memory = source
        .read(&layout_name(sid, "session_memory.md")?)?
        .and_then(|b| String::from_utf8(b).ok());
    Ok(ArchivedSession {
        id,
        entries,
        memory,
        message_count: row["message_count"].as_u64(),
    })
}

/// Read and check an archive; nothing outside the checks is done with it.
pub(crate) fn read_archive(path: &Path) -> Result<Archive, ImportError> {
    let mut source: Box<dyn Source> = if path.is_dir() {
        Box::new(DirSource::open(path)?)
    } else {
        Box::new(ZipSource::open(path)?)
    };
    read_from(source.as_mut(), MAX_TOTAL_BYTES)
}

/// [`read_archive`] from `source`, reading at most `budget` bytes in all —
/// the sizes an archive declares for its entries are its own to make up.
fn read_from(source: &mut dyn Source, budget: u64) -> Result<Archive, ImportError> {
    let mut source = Budgeted {
        inner: source,
        left: budget,
    };
    let manifest = manifest_of(&mut source)?;
    let rows = manifest["sessions"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return invalid("the manifest lists no sessions");
    }
    let mut listed = BTreeSet::new();
    if !rows
        .iter()
        .all(|row| listed.insert(row["session_id"].to_string()))
    {
        return invalid("the manifest lists a session twice");
    }
    let mut sessions = Vec::with_capacity(rows.len());
    for row in &rows {
        sessions.push(session_of(&mut source, row)?);
    }
    let root = manifest["root_session_id"]
        .as_str()
        .and_then(|s| SessionId::parse(s).ok())
        .ok_or_else(|| ImportError::Invalid("the manifest names no root session".into()))?;
    let saved = saved_results_of(&mut source, &manifest["tool_results"])?;
    let archive = Archive {
        root,
        sessions,
        saved,
    };
    verify_relations(&archive)?;
    Ok(archive)
}

fn kind_and_parent(archive: &Archive, id: SessionId) -> Option<(SessionKind, Option<String>)> {
    match archive.meta_of(id)? {
        LogEntry::Meta {
            session_kind,
            parent_session_id,
            ..
        } => Some((*session_kind, parent_session_id.clone())),
        _ => None,
    }
}

fn verify_relations(archive: &Archive) -> Result<(), ImportError> {
    let ids: BTreeSet<String> = archive.sessions.iter().map(|s| s.id.to_string()).collect();
    if ids.len() != archive.sessions.len() {
        return invalid("the manifest lists a session twice");
    }
    let root = archive.root.to_string();
    if !ids.contains(&root) {
        return invalid("the root session is not among the archived sessions");
    }
    for session in &archive.sessions {
        let Some((kind, parent)) = kind_and_parent(archive, session.id) else {
            return invalid("a session has no Meta line");
        };
        if session.id == archive.root {
            if kind != SessionKind::Primary {
                return invalid("the root session is not a primary session");
            }
            continue;
        }
        if kind == SessionKind::Sidechain && parent.as_ref().is_none_or(|p| !ids.contains(p)) {
            return invalid(format!(
                "sidechain {} names a parent that is not in the archive",
                session.id
            ));
        }
        let mut seen = BTreeSet::new();
        let mut at = session.id.to_string();
        while let Some((_, Some(parent))) = SessionId::parse(&at)
            .ok()
            .and_then(|id| kind_and_parent(archive, id))
        {
            if !seen.insert(at.clone()) {
                return invalid("the archived sessions' parents form a cycle");
            }
            at = parent;
            if !ids.contains(&at) {
                break;
            }
        }
    }
    Ok(())
}

/// What the rewritten sessions are placed under.
pub(crate) struct Target {
    pub(crate) project_root: Option<String>,
    pub(crate) cwd: String,
}

/// Old id to new id; the identity when `keep_ids`.
pub(crate) fn id_map(archive: &Archive, keep_ids: bool) -> HashMap<SessionId, SessionId> {
    archive
        .sessions
        .iter()
        .map(|s| (s.id, if keep_ids { s.id } else { SessionId::new() }))
        .collect()
}

/// Sessions as written, parents before children: the root, then by depth.
pub(crate) fn rewrite(
    archive: &Archive,
    ids: &HashMap<SessionId, SessionId>,
    target: &Target,
    warnings: &mut Vec<String>,
) -> Vec<(SessionId, Vec<EnvelopedEntry>)> {
    let text_ids: BTreeMap<String, String> = ids
        .iter()
        .map(|(old, new)| (old.to_string(), new.to_string()))
        .collect();
    let mut ordered: Vec<&ArchivedSession> = archive.sessions.iter().collect();
    ordered.sort_by_key(|s| depth_of(archive, s.id));
    ordered
        .into_iter()
        .map(|session| {
            let new = ids[&session.id];
            let entries = session
                .entries
                .iter()
                .cloned()
                .map(|mut env| {
                    match ids.get(&env.session_id) {
                        Some(mapped) => env.session_id = *mapped,
                        None => warnings.push(format!(
                            "a line of session {} names session {}, which is not in the archive",
                            session.id, env.session_id
                        )),
                    }
                    env.entry = rewrite_entry(env.entry, &text_ids, target);
                    env
                })
                .collect();
            (new, entries)
        })
        .collect()
}

fn depth_of(archive: &Archive, id: SessionId) -> usize {
    let mut depth = 0;
    let mut at = id;
    while let Some((_, Some(parent))) = kind_and_parent(archive, at) {
        match SessionId::parse(&parent) {
            Ok(p) if archive.sessions.iter().any(|s| s.id == p) && depth < 1024 => {
                depth += 1;
                at = p;
            }
            _ => break,
        }
    }
    depth
}

fn rewrite_entry(entry: LogEntry, ids: &BTreeMap<String, String>, target: &Target) -> LogEntry {
    match entry {
        LogEntry::Meta {
            started_at,
            model,
            permission_mode,
            engine_version,
            agix_version,
            parent_session_id,
            scene,
            session_kind,
            schema_version,
            ..
        } => LogEntry::Meta {
            cwd: target.cwd.clone(),
            started_at,
            model,
            permission_mode,
            engine_version,
            agix_version,
            parent_session_id: parent_session_id.map(|p| ids.get(&p).cloned().unwrap_or(p)),
            scene,
            project_root: target.project_root.clone(),
            session_kind,
            schema_version,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use history::entry::EnvelopedEntry;

    /// An archive in memory that counts what is read from it.
    struct Counting {
        files: HashMap<String, Vec<u8>>,
        read: u64,
    }

    impl Source for Counting {
        fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>, ImportError> {
            let bytes = self.files.get(name).cloned();
            self.read += bytes.as_ref().map_or(0, |b| b.len() as u64);
            Ok(bytes)
        }
    }

    /// One session, listed `times` times, with a memory file of `memory`
    /// bytes.
    fn listed(times: usize, memory: usize) -> Counting {
        let id = SessionId::new();
        let line =
            serde_json::to_string(&EnvelopedEntry::new(id, meta(None, SessionKind::Primary)))
                .unwrap();
        let transcript = format!("{line}\n").into_bytes();
        let row = serde_json::json!({
            "session_id": id.to_string(),
            "transcript_sha256": hex::encode(Sha256::digest(&transcript)),
        });
        let manifest = serde_json::json!({
            "format": FORMAT, "format_version": FORMAT_VERSION, "importable": true,
            "root_session_id": id.to_string(),
            "sessions": vec![row; times],
        });
        let mut files = HashMap::new();
        files.insert(
            "manifest.json".to_string(),
            manifest.to_string().into_bytes(),
        );
        files.insert(format!("sessions/{id}/transcript.jsonl"), transcript);
        files.insert(
            format!("sessions/{id}/session_memory.md"),
            vec![b'm'; memory],
        );
        Counting { files, read: 0 }
    }

    #[test]
    fn what_an_import_reads_is_bounded_in_total_not_per_read() {
        let mut twice = listed(2, 2_000);
        assert!(matches!(
            read_from(&mut twice, u64::MAX),
            Err(ImportError::Invalid(_))
        ));
        assert!(twice.read < 2_000, "a session listed twice was read twice");

        let mut big = listed(1, 4_000);
        assert!(
            matches!(read_from(&mut big, 2_000), Err(ImportError::Invalid(_))),
            "a memory file over the total was read whole"
        );
    }

    fn meta(parent: Option<&str>, kind: SessionKind) -> LogEntry {
        LogEntry::Meta {
            cwd: "/old".into(),
            started_at: time::OffsetDateTime::now_utc(),
            model: "m".into(),
            permission_mode: "default".into(),
            engine_version: "1".into(),
            agix_version: "1".into(),
            parent_session_id: parent.map(str::to_string),
            scene: Some("code".into()),
            project_root: Some("/old".into()),
            session_kind: kind,
            schema_version: CURRENT_META_SCHEMA_VERSION,
        }
    }

    fn user(session: SessionId, text: &str) -> EnvelopedEntry {
        EnvelopedEntry::new(
            session,
            LogEntry::User {
                content: vec![base::message::ContentBlock::Text {
                    text: text.into(),
                    cache_control: None,
                }],
            },
        )
    }

    #[test]
    fn rewrite_maps_sessions_and_parents_and_leaves_text_alone() {
        let (root, child) = (SessionId::new(), SessionId::new());
        let mention = format!("session {root} at /old");
        let archive = Archive {
            root,
            saved: None,
            sessions: vec![
                ArchivedSession {
                    id: child,
                    entries: vec![
                        EnvelopedEntry::new(
                            child,
                            meta(Some(&root.to_string()), SessionKind::Sidechain),
                        ),
                        user(child, "child"),
                    ],
                    memory: None,
                    message_count: None,
                },
                ArchivedSession {
                    id: root,
                    entries: vec![
                        EnvelopedEntry::new(root, meta(None, SessionKind::Primary)),
                        user(root, &mention),
                    ],
                    memory: None,
                    message_count: None,
                },
            ],
        };
        verify_relations(&archive).unwrap();
        let ids = id_map(&archive, false);
        let target = Target {
            project_root: Some("/new".into()),
            cwd: "/new".into(),
        };
        let written = rewrite(&archive, &ids, &target, &mut Vec::new());
        assert_eq!(written[0].0, ids[&root], "the root is written first");
        assert_eq!(written[1].0, ids[&child]);
        assert_ne!(ids[&root], root);
        let (_, child_lines) = &written[1];
        assert!(child_lines.iter().all(|e| e.session_id == ids[&child]));
        match &child_lines[0].entry {
            LogEntry::Meta {
                parent_session_id,
                cwd,
                project_root,
                ..
            } => {
                assert_eq!(
                    parent_session_id.as_deref(),
                    Some(ids[&root].to_string().as_str())
                );
                assert_eq!(cwd, "/new");
                assert_eq!(project_root.as_deref(), Some("/new"));
            }
            other => panic!("{other:?}"),
        }
        let (_, root_lines) = &written[0];
        assert_eq!(root_lines[1].id, archive.sessions[1].entries[1].id);
        match &root_lines[1].entry {
            LogEntry::User { content } => match &content[0] {
                base::message::ContentBlock::Text { text, .. } => assert_eq!(text, &mention),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_sidechain_whose_parent_is_missing_is_refused() {
        let (root, child) = (SessionId::new(), SessionId::new());
        let archive = Archive {
            root,
            saved: None,
            sessions: vec![
                ArchivedSession {
                    id: root,
                    entries: vec![EnvelopedEntry::new(root, meta(None, SessionKind::Primary))],
                    memory: None,
                    message_count: None,
                },
                ArchivedSession {
                    id: child,
                    entries: vec![EnvelopedEntry::new(
                        child,
                        meta(Some(&SessionId::new().to_string()), SessionKind::Sidechain),
                    )],
                    memory: None,
                    message_count: None,
                },
            ],
        };
        assert!(matches!(
            verify_relations(&archive),
            Err(ImportError::Invalid(_))
        ));
    }
}
