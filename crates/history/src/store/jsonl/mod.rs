//! The JSONL backend: one append-only log per session under a project
//! directory, many sessions and projects behind one store, large content
//! moved out to the blob store.

pub(super) mod files;
mod store_impl;

use crate::blob::{BlobStore, PasteStore};
use crate::entry::LogEntry;
use crate::error::HistoryError;
use crate::path::{
    canonicalize_cwd, project_dir, project_dir_for, session_file, session_sidecar_dir, HistoryRoots,
};
use crate::project::SessionMetadata;
use crate::transcript::{
    messages_match_query, preview_messages, project_messages_with, TranscriptProjection,
};
use base::session::SessionId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::{HistoryStore, LineageSnapshot, SessionSummary};
use files::*;

/// Writes to `<projects_root>/<sanitize(project root)>/<session>.jsonl`.
pub struct JsonlHistoryStore {
    projects_root: PathBuf,
    /// Sidecar root — see [`HistoryRoots`]; carried rather than re-derived so
    /// the two halves of a session's state cannot land in different trees.
    sessions_root: PathBuf,
    /// The project a session with nothing recorded about it belongs to: the
    /// cwd this store was built with. Sessions created since transcripts were
    /// partitioned all record their own (see [`JsonlHistoryStore::home_of`]),
    /// so this answers for the ones written before that — and for a caller
    /// that asks about the "current" project without naming one.
    canonical_cwd: PathBuf,
    /// Which project each session belongs to, for the life of the process —
    /// `None` meaning a session with no project at all. The durable copy is
    /// the session's own sidecar; this is the cache in front of it, and the
    /// reason an append doesn't read a file to find out where to write.
    homes: Mutex<HashMap<SessionId, Option<PathBuf>>>,
    /// One per session: serializes that session's appends and sidecar
    /// read-modify-writes, and nobody else's.
    logs: std::sync::Mutex<HashMap<SessionId, Arc<Mutex<LogState>>>>,
    /// Sessions whose log's directory entry this process has already synced.
    synced_dirs: Mutex<std::collections::HashSet<SessionId>>,
    /// Sessions this store deleted. An engine that was still stopping when
    /// its session was deleted would otherwise write its last lines into a
    /// fresh log that no `Meta` places in any project; only a `Meta`, which
    /// starts the session again, is taken.
    deleted: std::sync::Mutex<std::collections::HashSet<SessionId>>,
    /// Where content too big to keep inline goes. Without one, everything
    /// stays in the JSONL.
    blobs: Option<Arc<dyn BlobStore>>,
    /// How this store's logs become messages. `None` is the engine's rules.
    projection: Option<Arc<dyn TranscriptProjection>>,
    /// Where entry ids and timestamps come from. `None` is the machine's.
    environment: Option<Arc<dyn base::interface::environment::Environment>>,
}

impl JsonlHistoryStore {
    /// 两个根都由调用方给：transcript 落 `roots.projects`，sidecar 落
    /// `roots.sessions`。
    pub async fn with_roots(cwd: &Path, roots: HistoryRoots) -> Result<Self, HistoryError> {
        let canonical = canonicalize_cwd(cwd).await?;
        Ok(Self {
            projects_root: roots.projects,
            sessions_root: roots.sessions,
            canonical_cwd: canonical,
            homes: Mutex::new(HashMap::new()),
            logs: std::sync::Mutex::new(HashMap::new()),
            synced_dirs: Mutex::new(std::collections::HashSet::new()),
            deleted: std::sync::Mutex::new(std::collections::HashSet::new()),
            blobs: None,
            projection: None,
            environment: None,
        })
    }

    /// Move large content and images out of the JSONL and into `blobs`.
    ///
    /// Written entries that qualify are replaced by a [`LogEntry::Blob`]
    /// naming this store; a load with the same store attached puts them back.
    /// Without a blob store nothing is externalized, and a log that has
    /// references in it still loads — with a gap where the content was.
    pub fn with_blob_store(mut self, blobs: Arc<dyn BlobStore>) -> Self {
        self.blobs = Some(blobs);
        self
    }

    /// Read this store's logs under someone else's rules.
    ///
    /// The rules must be reconstructible from the log alone — see
    /// [`crate::transcript::model_visible_content_is_reconstructible`]. A
    /// projection that reads anything else produces a session that cannot be
    /// reopened.
    pub fn with_projection(mut self, projection: Arc<dyn TranscriptProjection>) -> Self {
        self.projection = Some(projection);
        self
    }

    /// Take entry ids and timestamps from somewhere other than the machine.
    pub fn with_environment(
        mut self,
        environment: Arc<dyn base::interface::environment::Environment>,
    ) -> Self {
        self.environment = Some(environment);
        self
    }

    /// [`with_blob_store`](Self::with_blob_store) with the shipped default.
    pub fn with_paste_store(self, paste_store: PasteStore) -> Self {
        self.with_blob_store(Arc::new(paste_store))
    }

    pub fn project_dir_path(&self) -> PathBuf {
        project_dir(&self.projects_root, &self.canonical_cwd)
    }

    /// Where a session that never recorded a project of its own is written —
    /// not where an arbitrary session is. Use [`Self::session_path_of`] for
    /// that; this one cannot ask the disk, and so cannot answer.
    pub fn session_file_path(&self, session: &SessionId) -> PathBuf {
        session_file(&self.project_dir_path(), session)
    }

    /// The project this session belongs to: `Some(root)`, or `None` for a
    /// session created with no project at all.
    ///
    /// Three answers, in order. The in-memory map, filled in when the session
    /// wrote its `Meta` line and kept for as long as this process lives. The
    /// session's own sidecar, which is indexed by session id alone and is
    /// therefore reachable without already knowing the project — this is the
    /// tier that survives a restart, and why `Meta` writes the sidecar before
    /// it writes the log. And finally the cwd this store was built with,
    /// which is where every session lived back when a store served one
    /// project, and so is the right answer for transcripts written then.
    async fn home_of(&self, session: SessionId) -> Option<PathBuf> {
        if let Some(home) = self.homes.lock().await.get(&session) {
            return home.clone();
        }
        let resolved = match load_session_metadata(&self.sessions_root, session).await {
            // An empty `canonical_cwd` is how a no-project session records
            // itself; `SessionMetadata::new` is given an empty path for it.
            Some(meta) if meta.canonical_cwd.is_empty() => None,
            Some(meta) => Some(PathBuf::from(meta.canonical_cwd)),
            None => Some(self.canonical_cwd.clone()),
        };
        self.homes.lock().await.insert(session, resolved.clone());
        resolved
    }

    /// The transcript directory of the project rooted at `home`, resolving
    /// the root the same way a session's own `Meta` is resolved.
    async fn project_dir_of_root(&self, home: Option<&Path>) -> PathBuf {
        let canonical = match home {
            Some(root) => Some(
                canonicalize_cwd(root)
                    .await
                    .unwrap_or_else(|_| root.to_path_buf()),
            ),
            None => None,
        };
        project_dir_for(&self.projects_root, canonical.as_deref())
    }

    /// The directory holding this session's transcript.
    async fn session_dir_of(&self, session: SessionId) -> PathBuf {
        project_dir_for(&self.projects_root, self.home_of(session).await.as_deref())
    }

    /// The path of this session's transcript.
    async fn session_path_of(&self, session: SessionId) -> PathBuf {
        session_file(&self.session_dir_of(session).await, &session)
    }

    /// [`HistoryError::SessionDeleted`] for a line other than `Meta` to a
    /// session this store deleted; a `Meta` lifts the tombstone.
    fn refuse_if_deleted(&self, session: SessionId, entry: &LogEntry) -> Result<(), HistoryError> {
        let mut deleted = self.deleted.lock().unwrap_or_else(|e| e.into_inner());
        if !deleted.contains(&session) {
            return Ok(());
        }
        if matches!(entry, LogEntry::Meta { .. }) {
            deleted.remove(&session);
            return Ok(());
        }
        Err(HistoryError::SessionDeleted(session.to_string()))
    }

    fn was_deleted(&self, session: SessionId) -> bool {
        self.deleted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&session)
    }

    fn log_of(&self, session: SessionId) -> Arc<Mutex<LogState>> {
        self.logs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session)
            .or_default()
            .clone()
    }

    async fn writer_lock(
        &self,
        session: SessionId,
    ) -> Result<crate::lock::WriterLock, HistoryError> {
        const PATIENCE: std::time::Duration = std::time::Duration::from_secs(1);
        const STEP: std::time::Duration = std::time::Duration::from_millis(10);
        let dir = session_sidecar_dir(&self.sessions_root, &session);
        let id = session.to_string();
        let mut waited = std::time::Duration::ZERO;
        loop {
            let (dir, id) = (dir.clone(), id.clone());
            match blocking(move || crate::lock::hold(&dir, &id)).await {
                // A child this process forks inherits the lock file's open
                // descriptor until it execs, so a lock this process just let
                // go of can still read as held for a moment; a lock another
                // daemon holds stays held.
                Err(HistoryError::Locked(_)) if waited < PATIENCE => {
                    tokio::time::sleep(STEP).await;
                    waited += STEP;
                }
                held => return held,
            }
        }
    }

    /// The project a session is about to be created under, read off the
    /// `Meta` line that creates it.
    ///
    /// A sidechain inherits its parent's project rather than landing in the
    /// no-project bucket: it carries no `project_root` of its own (the engine
    /// spawning it has no concept of one), but its transcript is anchored to
    /// the parent's — cascade delete and the parent-child walk both look for
    /// it there.
    async fn home_from_meta(&self, entry: &LogEntry) -> Option<Option<PathBuf>> {
        let LogEntry::Meta {
            project_root,
            parent_session_id,
            ..
        } = entry
        else {
            return None;
        };
        if let Some(root) = project_root {
            let root = Path::new(root);
            return Some(Some(
                canonicalize_cwd(root)
                    .await
                    .unwrap_or_else(|_| root.to_path_buf()),
            ));
        }
        match parent_session_id.as_deref() {
            Some(parent) => Some(match SessionId::parse(parent) {
                Ok(parent) => self.home_of(parent).await,
                // A parent id this store cannot resolve to a session: keep the
                // child where a store that served one project would have put
                // it, which is also where `child_sessions` looks for it.
                Err(_) => Some(self.canonical_cwd.clone()),
            }),
            None => Some(None),
        }
    }

    /// The project `session` writes into, recording it first when `entry`
    /// is the `Meta` that says.
    ///
    /// The sidecar is written *before* the log line, which is the opposite
    /// order from the counters an append updates after it and for the
    /// opposite reason: the counters are a convenience computed from a log
    /// that already exists, while this is the index that says where the log
    /// is. A crash between the two leaves an empty index entry, which is
    /// recoverable; the other order leaves a transcript nothing can find.
    async fn home_recorded_by(
        &self,
        session: SessionId,
        entry: &LogEntry,
    ) -> Result<Option<PathBuf>, HistoryError> {
        let Some(home) = self.home_from_meta(entry).await else {
            return Ok(self.home_of(session).await);
        };
        let dir = project_dir_for(&self.projects_root, home.as_deref());
        crate::project::ensure_session_sidecar_in(
            &self.sessions_root,
            home.as_deref().unwrap_or_else(|| Path::new("")),
            &dir,
            session,
        )
        .await?;
        self.homes.lock().await.insert(session, home.clone());
        Ok(home)
    }

    pub fn canonical_cwd(&self) -> &Path {
        &self.canonical_cwd
    }

    /// The second half of [`HistoryStore::snapshot_lineage`]: the logs in
    /// `dir` read up to the lengths measured for them, each cut back to the
    /// last whole line — a length can land inside a line being appended.
    async fn read_lineage(
        &self,
        root: SessionId,
        dir: &Path,
        lengths: HashMap<SessionId, u64>,
    ) -> Result<LineageSnapshot, HistoryError> {
        if !lengths.contains_key(&root) {
            return Err(HistoryError::SessionNotFound(root.to_string()));
        }
        let mut facts = HashMap::with_capacity(lengths.len());
        for (&session, &len) in &lengths {
            match head_facts(&session_file(dir, &session), len).await {
                Ok(Some(found)) => {
                    facts.insert(session, found);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(%session, error = %e, "unreadable log left out of a lineage walk");
                }
            }
        }
        let (members, forks) = lineage_of(root, &facts);
        let mut sessions = Vec::with_capacity(members.len());
        for session in members {
            let len = lengths.get(&session).copied().unwrap_or(0);
            match read_prefix(&session_file(dir, &session), len).await? {
                Some(content) => {
                    sessions.push((session, parse_log(&content, self.blobs.as_deref())?));
                }
                None if session == root => {
                    return Err(HistoryError::SessionNotFound(root.to_string()));
                }
                None => {
                    tracing::warn!(%session, "a sidechain's log disappeared while it was being read");
                }
            }
        }
        Ok(LineageSnapshot { sessions, forks })
    }

    async fn recent_summaries(
        &self,
        dir: &Path,
        max: usize,
        detail: crate::query::SummaryDetail,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        let files = self.session_files_by_mtime(dir, max).await?;
        let mut out = Vec::new();
        for (session_id, _path, mtime) in files {
            match detail {
                // The directory listing already answered the question. Opening
                // each transcript to fill in a preview nobody asked for turns
                // a listing into a full parse of every session in the project.
                //
                // The sidecar is a different matter and is read: it is one
                // small JSON document per session, and it holds the things
                // that exist nowhere else — the name a person gave this
                // session, and the counters. Leaving it out would bring a
                // renamed session back unnamed.
                crate::query::SummaryDetail::IdsOnly => {
                    let metadata = load_session_metadata(&self.sessions_root, session_id).await;
                    out.push(self.summary_from_parts(session_id, mtime, 0, &[], metadata))
                }
                crate::query::SummaryDetail::Full => {
                    if let Some(summary) = self.session_summary(session_id, mtime).await? {
                        out.push(summary);
                    }
                }
            }
        }
        Ok(out)
    }

    async fn search_one_project(
        &self,
        dir: &Path,
        needle: &str,
        max: usize,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        let files = self.session_files_by_mtime(dir, usize::MAX).await?;
        let mut out = Vec::new();
        for (session_id, _path, mtime) in files {
            let entries = match self.load(session_id).await {
                Ok(entries) => entries,
                Err(HistoryError::SessionNotFound(_)) => continue,
                Err(e) => {
                    skip_unreadable(session_id, &e);
                    continue;
                }
            };
            let messages = project_messages_with(&entries, self.projection());
            if session_id.to_string().contains(needle) || messages_match_query(&messages, needle) {
                let metadata = load_session_metadata(&self.sessions_root, session_id).await;
                out.push(self.summary_from_parts(
                    session_id,
                    mtime,
                    entries.len(),
                    &messages,
                    metadata,
                ));
            }
            if out.len() >= max {
                break;
            }
        }
        Ok(out)
    }

    async fn search_project_dirs(
        &self,
        query: &str,
        max: usize,
        filter: ProjectDirFilter,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let query = query.trim().to_lowercase();
        let mut project_dirs = match tokio::fs::read_dir(&self.projects_root).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(HistoryError::Io(e)),
        };
        let mut candidates = Vec::new();
        while let Some(entry) = project_dirs.next_entry().await? {
            let Ok(ft) = entry.file_type().await else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let mut files = collect_session_files(entry.path()).await?;
            candidates.append(&mut files);
        }
        candidates.sort_by_key(|p| std::cmp::Reverse(p.2));

        let mut out = Vec::new();
        for (session_id, path, mtime) in candidates {
            let metadata = load_session_metadata(&self.sessions_root, session_id).await;
            if !filter.matches(metadata.as_ref()) {
                continue;
            }
            let entries = match read_log(&path).await {
                Ok(Some(content)) => parse_log(&content, self.blobs.as_deref()),
                Ok(None) => continue,
                Err(e) => Err(e),
            };
            let entries = match entries {
                Ok(entries) => entries,
                Err(e) => {
                    skip_unreadable(session_id, &e);
                    continue;
                }
            };
            let messages = project_messages_with(&entries, self.projection());
            let haystack = format!(
                "{}\n{}\n{}\n{}",
                session_id,
                metadata
                    .as_ref()
                    .and_then(|m| m.title.as_deref())
                    .unwrap_or_default(),
                metadata
                    .as_ref()
                    .map(|m| m.canonical_cwd.as_str())
                    .unwrap_or_default(),
                crate::transcript::render_search_text(&messages)
            )
            .to_lowercase();
            if query.is_empty() || haystack.contains(&query) {
                out.push(self.summary_from_parts(
                    session_id,
                    mtime,
                    entries.len(),
                    &messages,
                    metadata,
                ));
            }
            if out.len() >= max {
                break;
            }
        }
        Ok(out)
    }

    /// `None` for a session whose log cannot be read back. A half-written or
    /// hand-edited transcript is a property of that one session; letting it
    /// fail the call would hide every other session in the directory behind
    /// it.
    async fn session_summary(
        &self,
        session_id: SessionId,
        mtime: std::time::SystemTime,
    ) -> Result<Option<SessionSummary>, HistoryError> {
        let entries = match self.load(session_id).await {
            Ok(entries) => entries,
            Err(HistoryError::SessionNotFound(_)) | Err(HistoryError::Parse { .. }) => {
                return Ok(None)
            }
            Err(e) => return Err(e),
        };
        let messages = project_messages_with(&entries, self.projection());
        let metadata = load_session_metadata(&self.sessions_root, session_id).await;
        Ok(Some(self.summary_from_parts(
            session_id,
            mtime,
            entries.len(),
            &messages,
            metadata,
        )))
    }

    fn summary_from_parts(
        &self,
        session_id: SessionId,
        mtime: std::time::SystemTime,
        entry_count: usize,
        messages: &[base::message::Message],
        metadata: Option<SessionMetadata>,
    ) -> SessionSummary {
        SessionSummary {
            session_id,
            last_modified: format_mtime(mtime),
            entry_count,
            message_count: messages.len(),
            preview: metadata
                .as_ref()
                .and_then(|m| m.latest_summary.clone())
                .unwrap_or_else(|| preview_messages(messages, 140)),
            canonical_cwd: metadata.as_ref().map(|m| m.canonical_cwd.clone()),
            title: metadata.as_ref().and_then(|m| m.title.clone()),
            total_input_tokens: metadata.as_ref().and_then(|m| m.total_input_tokens),
            total_output_tokens: metadata.as_ref().and_then(|m| m.total_output_tokens),
            compact_count: metadata.as_ref().map(|m| m.compact_count).unwrap_or(0),
        }
    }

    async fn session_files_by_mtime(
        &self,
        dir: &Path,
        max: usize,
    ) -> Result<Vec<(SessionId, PathBuf, std::time::SystemTime)>, HistoryError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
        };
        let mut found: Vec<(SessionId, PathBuf, std::time::SystemTime)> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            // 文件名 `<id>.jsonl`
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(id) = SessionId::parse(stem) else {
                continue;
            };
            let mtime = match entry.metadata().await {
                Ok(m) => m.modified().unwrap_or(std::time::UNIX_EPOCH),
                Err(_) => std::time::UNIX_EPOCH,
            };
            found.push((id, path, mtime));
        }
        // 倒序按 mtime
        found.sort_by_key(|p| std::cmp::Reverse(p.2));
        found.truncate(max);
        Ok(found)
    }
}

#[cfg(test)]
mod tests;
