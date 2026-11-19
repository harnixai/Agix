//! The JSONL backend: one append-only log per session, many sessions and
//! projects behind one store, large content moved out to the blob store. A
//! session is kept globally or in its project; `homes` is where that is
//! answered, `listing` how sessions are found across both.

pub(super) mod files;
mod homes;
mod listing;
mod store_impl;

use crate::blob::{BlobStore, PasteStore};
use crate::entry::{EnvelopedEntry, LogEntry, SessionKind};
use crate::error::HistoryError;
use crate::path::{canonicalize_cwd, project_dir, session_file, session_sidecar_dir, HistoryRoots};
use crate::placement::{ProjectHistory, SidecarLocator};
use crate::transcript::TranscriptProjection;
use base::session::SessionId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::LineageSnapshot;
use files::*;
use homes::{Access, Home, Homes, Kept};

/// Writes a session's log to `<projects_root>/<sanitize(project root)>/<session>.jsonl`,
/// or, for a session kept in its project, to that project's
/// `state/history/<session>/<session>.jsonl`.
pub struct JsonlHistoryStore {
    /// Where each session's files are, for every read and write. The durable
    /// copy is the session's index or sidecar; this holds what was read of it
    /// for the life of the process, which is why an append doesn't read a
    /// file to find out where to write.
    homes: Arc<Homes>,
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
            homes: Arc::new(Homes::new(roots.projects, roots.sessions, canonical)),
            logs: std::sync::Mutex::new(HashMap::new()),
            synced_dirs: Mutex::new(std::collections::HashSet::new()),
            deleted: std::sync::Mutex::new(std::collections::HashSet::new()),
            blobs: None,
            projection: None,
            environment: None,
        })
    }

    /// Keep new sessions in their projects where `projects` says to, and read
    /// the ones an index places there. Without it every session is kept
    /// globally, and one an index places in a project is not read.
    pub fn with_project_history(mut self, projects: Arc<dyn ProjectHistory>) -> Self {
        let homes = Homes::new(
            self.homes.projects_root.clone(),
            self.homes.sessions_root.clone(),
            self.homes.canonical_cwd.clone(),
        );
        self.homes = Arc::new(homes.with_projects(projects));
        self
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
        project_dir(&self.homes.projects_root, &self.homes.canonical_cwd)
    }

    /// Where a session that never recorded a project of its own is written —
    /// not where an arbitrary session is. This one cannot ask the disk, and
    /// so cannot answer that.
    pub fn session_file_path(&self, session: &SessionId) -> PathBuf {
        session_file(&self.project_dir_path(), session)
    }

    pub fn canonical_cwd(&self) -> &Path {
        &self.homes.canonical_cwd
    }

    async fn home_of(&self, session: SessionId, access: Access) -> Result<Home, HistoryError> {
        let homes = self.homes.clone();
        blocking(move || homes.locate(session, access)).await
    }

    async fn session_path_of(
        &self,
        session: SessionId,
        access: Access,
    ) -> Result<(PathBuf, Home), HistoryError> {
        let home = self.home_of(session, access).await?;
        Ok((self.homes.transcript(&home, session), home))
    }

    /// `session`'s sidecar directory, found again on every call.
    fn locator(&self, session: SessionId) -> SidecarLocator {
        let homes = self.homes.clone();
        SidecarLocator::new(move || {
            let home = homes.locate(session, Access::Read)?;
            Ok(homes.sidecar_dir(&home, session))
        })
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

    /// The lock lives in the global `sessions/<id>/` whichever way the
    /// session is kept: it is taken before an append knows where the session
    /// goes.
    async fn writer_lock(
        &self,
        session: SessionId,
    ) -> Result<crate::lock::WriterLock, HistoryError> {
        const PATIENCE: std::time::Duration = std::time::Duration::from_secs(1);
        const STEP: std::time::Duration = std::time::Duration::from_millis(10);
        let dir = session_sidecar_dir(&self.homes.sessions_root, &session);
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
    /// A sidechain records its parent's project; one that records none (its
    /// parent has no project, or the log predates the field) inherits its
    /// parent's home rather than landing in the no-project bucket: its
    /// transcript is anchored to the parent's — cascade delete and the
    /// parent-child walk both look for it there.
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
        let fallback = Some(self.homes.canonical_cwd.clone());
        match parent_session_id.as_deref() {
            Some(parent) => Some(match SessionId::parse(parent) {
                Ok(parent) => self
                    .home_of(parent, Access::Write)
                    .await
                    .map_or(fallback, |home| home.project),
                // A parent id this store cannot resolve to a session: keep the
                // child where a store that served one project would have put
                // it, which is also where `child_sessions` looks for it.
                Err(_) => fallback,
            }),
            None => Some(None),
        }
    }

    /// Where `session` writes, recording it first when `entry` is the `Meta`
    /// that says.
    ///
    /// The index and sidecar are written *before* the log line, which is the
    /// opposite order from the counters an append updates after it and for
    /// the opposite reason: the counters are a convenience computed from a
    /// log that already exists, while these say where the log is. A crash
    /// between the two leaves an empty index entry, which is recoverable; the
    /// other order leaves a transcript nothing can find. A session already
    /// kept in a project stays there.
    async fn home_recorded_by(
        &self,
        session: SessionId,
        entry: &LogEntry,
    ) -> Result<Home, HistoryError> {
        let Some(project) = self.home_from_meta(entry).await else {
            return self.home_of(session, Access::Write).await;
        };
        let kept = match self.home_of(session, Access::Write).await?.kept {
            Kept::InProject(index) => Kept::InProject(index),
            Kept::Global => {
                let (recorded, parent, kind) = match entry {
                    LogEntry::Meta {
                        project_root,
                        parent_session_id,
                        session_kind,
                        ..
                    } => (
                        project_root.as_ref().map(PathBuf::from),
                        parent_session_id
                            .as_deref()
                            .and_then(|p| SessionId::parse(p).ok()),
                        *session_kind,
                    ),
                    _ => (None, None, SessionKind::Primary),
                };
                let (homes, root) = (self.homes.clone(), project.clone());
                blocking(move || {
                    Ok(homes.place(session, root.as_deref(), recorded.as_deref(), parent, kind))
                })
                .await?
            }
        };
        if let Kept::InProject(index) = &kept {
            if let Some(root) = &project {
                self.homes.note_project(root);
            }
            let (index, sessions_root) = (index.clone(), self.homes.sessions_root.clone());
            blocking(move || index.write(&sessions_root)).await?;
        }
        let home = Home { project, kept };
        crate::project::ensure_session_sidecar_in(
            self.homes.sidecar_root(&home),
            home.cwd(),
            &self.homes.transcript_dir(&home),
            session,
        )
        .await?;
        self.homes.remember(session, home.clone());
        Ok(home)
    }

    /// The transcripts kept beside `home`'s, by session: a global project
    /// directory, or the indexed sessions of a project's history directory.
    /// A lineage is kept in one of these. `access` is how the project's
    /// sessions are located.
    async fn beside(
        &self,
        home: &Home,
        access: Access,
    ) -> Result<HashMap<SessionId, (PathBuf, Home)>, HistoryError> {
        let homes = self.homes.clone();
        match &home.kept {
            Kept::Global => {
                let dir = homes.transcript_dir(home);
                Ok(session_ids_in(&dir)
                    .await?
                    .into_iter()
                    .map(|id| (id, (session_file(&dir, &id), home.clone())))
                    .collect())
            }
            Kept::InProject(index) => {
                let dir = index.history_dir().to_path_buf();
                blocking(move || {
                    Ok(homes
                        .indexed_in(&dir, access)
                        .into_iter()
                        .map(|(id, home)| (id, (homes.transcript(&home, id), home)))
                        .collect())
                })
                .await
            }
        }
    }

    /// The second half of [`HistoryStore::snapshot_lineage`]: the logs read
    /// up to the lengths measured for them, each cut back to the last whole
    /// line — a length can land inside a line being appended.
    async fn read_lineage(
        &self,
        root: SessionId,
        logs: &HashMap<SessionId, (PathBuf, Home)>,
        lengths: HashMap<SessionId, u64>,
    ) -> Result<LineageSnapshot, HistoryError> {
        if !lengths.contains_key(&root) {
            return Err(HistoryError::SessionNotFound(root.to_string()));
        }
        let mut facts = HashMap::with_capacity(lengths.len());
        for (&session, &len) in &lengths {
            let Some((path, _)) = logs.get(&session) else {
                continue;
            };
            match head_facts(path, len).await {
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
            let Some((path, home)) = logs.get(&session) else {
                continue;
            };
            match read_prefix(path, len).await? {
                Some(content) => {
                    let mut entries = parse_log(&content, self.blobs.as_deref())?;
                    rebase_all(home, &mut entries);
                    sessions.push((session, entries));
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

    /// Every directory this store keeps sessions in that `path` could be
    /// inside: the global roots, and the history directory of each project
    /// `path` is in, as spelled or as its nearest existing ancestor resolves
    /// — a path about to be written does not exist yet.
    fn dirs_around(&self, path: &Path) -> Vec<PathBuf> {
        let mut dirs = vec![
            self.homes.projects_root.clone(),
            self.homes.sessions_root.clone(),
        ];
        if let Some(projects) = self.homes.projects() {
            let real = path.ancestors().find_map(|dir| dir.canonicalize().ok());
            let ancestors = path
                .ancestors()
                .chain(real.iter().flat_map(|p| p.ancestors()));
            for found in ancestors.filter_map(|dir| projects.history_of(dir)) {
                if !dirs.contains(&found.dir) {
                    dirs.push(found.dir);
                }
            }
        }
        dirs
    }
}

fn rebase_all(home: &Home, entries: &mut [EnvelopedEntry]) {
    for env in entries {
        home.rebase(&mut env.entry);
    }
}

#[cfg(test)]
mod tests;
