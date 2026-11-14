//! `HistoryStore` for `JsonlHistoryStore`.

use crate::entry::{EnvelopedEntry, LogEntry};
use crate::error::HistoryError;
use crate::path::{project_dir_for, session_file, session_sidecar_dir};
use crate::query::{SessionQuery, SessionScope};
use crate::transcript::{DefaultProjection, TranscriptProjection};
use async_trait::async_trait;
use base::session::SessionId;
use std::path::Path;

use super::super::externalize::externalize_if_bulky;
use super::super::{
    HistoryStore, LineageSnapshot, SessionClaim, SessionFacts, SessionStats, SessionSummary,
};
use super::files::*;
use super::JsonlHistoryStore;

#[async_trait]
impl HistoryStore for JsonlHistoryStore {
    fn projection(&self) -> &dyn TranscriptProjection {
        match &self.projection {
            Some(p) => &**p,
            None => &DefaultProjection,
        }
    }

    fn checkpoint_objects(
        &self,
        root: SessionId,
    ) -> Option<std::sync::Arc<dyn crate::blob::BlobStore>> {
        let dir = crate::path::session_checkpoint_dir(&self.sessions_root, &root).join("objects");
        Some(std::sync::Arc::new(
            crate::checkpoint::CheckpointObjects::new(dir),
        ))
    }

    fn environment(&self) -> &dyn base::interface::environment::Environment {
        match &self.environment {
            Some(e) => &**e,
            None => &base::interface::environment::SystemEnvironment,
        }
    }

    async fn append(&self, session: SessionId, entry: LogEntry) -> Result<(), HistoryError> {
        self.append_tagged(session, entry, crate::entry::EntryTags::default())
            .await
    }

    async fn append_tagged(
        &self,
        session: SessionId,
        entry: LogEntry,
        tags: crate::entry::EntryTags,
    ) -> Result<(), HistoryError> {
        self.refuse_if_deleted(session, &entry)?;
        let writer = self.writer_lock(session).await?;
        let mut log = self.log_of(session).lock_owned().await;
        // Checked again under the locks: a delete that ran while this append
        // waited for them has already removed what taking the writer lock
        // just made again.
        if let Err(refused) = self.refuse_if_deleted(session, &entry) {
            drop(log);
            let sidecar = session_sidecar_dir(&self.sessions_root, &session);
            let _ =
                blocking(move || writer.remove_sidecar(&sidecar).map_err(HistoryError::Io)).await;
            return Err(refused);
        }

        let home = self.home_recorded_by(session, &entry).await?;
        let project_history_dir = project_dir_for(&self.projects_root, home.as_deref());
        let path = session_file(&project_history_dir, &session);

        // Read before the entry can be externalized: an assistant message big
        // enough to move out to a blob would take its usage with it, and the
        // totals would quietly stop counting anything over the threshold.
        let counters = crate::project::CounterDelta::of(&entry);

        let entry = match self.blobs.clone() {
            Some(blobs) => blocking(move || externalize_if_bulky(entry, blobs.as_ref())).await?,
            None => entry,
        };

        let enveloped = EnvelopedEntry::new_in(self.environment(), session, entry).with_tags(tags);
        let mut line = serde_json::to_vec(&enveloped)?;
        line.push(b'\n');

        // The locks go into the write and come back out of it: a caller that
        // gives up while the line is going down drops only this future, and
        // the next append must not get in before the line is whole.
        let (_writer, _log) = blocking(move || {
            let known_len = log.len_after_last_append.take();
            log.len_after_last_append = Some(write_line(&path, &line, known_len)?);
            Ok((writer, log))
        })
        .await?;

        // After the log, and never in front of it. The transcript is the
        // record; these are a convenience computed from it, so a sidecar that
        // cannot be written costs a count and not the entry — still under
        // this session's log lock, which is what keeps two appends from
        // reading the same total and writing back the same sum.
        if let Err(e) = crate::project::add_counters_in(
            &self.sessions_root,
            home.as_deref().unwrap_or_else(|| Path::new("")),
            &project_history_dir,
            session,
            counters,
        )
        .await
        {
            tracing::warn!(error = %e, %session, "could not update this session's counters");
        }
        Ok(())
    }

    async fn load(&self, session: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError> {
        let path = self.session_path_of(session).await;
        let content = read_log(&path)
            .await?
            .ok_or_else(|| HistoryError::SessionNotFound(session.to_string()))?;
        parse_log(&content, self.blobs.as_deref())
    }

    async fn sync(&self, session: SessionId) -> Result<(), HistoryError> {
        let path = self.session_path_of(session).await;
        let dir_done = self.synced_dirs.lock().await.contains(&session);
        tokio::task::spawn_blocking(move || sync_log(&path, dir_done))
            .await
            .map_err(|e| HistoryError::Io(std::io::Error::other(e)))??;
        self.synced_dirs.lock().await.insert(session);
        Ok(())
    }

    async fn claim(&self, session: SessionId) -> Result<SessionClaim, HistoryError> {
        Ok(SessionClaim::new(self.writer_lock(session).await?))
    }

    /// Measures every log in the root's project directory and reads each one
    /// up to what was measured, taking no lock: a session whose append is
    /// slow does not hold up a snapshot of another. A sidechain's transcript
    /// goes to its parent's project, so one directory holds the whole
    /// lineage.
    async fn snapshot_lineage(&self, root: SessionId) -> Result<LineageSnapshot, HistoryError> {
        let dir = self.session_dir_of(root).await;
        let lengths = transcript_lengths_in(&dir).await?;
        self.read_lineage(root, &dir, lengths).await
    }

    /// The transcript goes, then the whole sidecar. The writer lock is held
    /// throughout so no other process can start writing in between.
    async fn delete(&self, session: SessionId) -> Result<(), HistoryError> {
        let writer = self.writer_lock(session).await?;
        let mut log = self.log_of(session).lock_owned().await;
        let jsonl_path = self.session_path_of(session).await;
        let sidecar = session_sidecar_dir(&self.sessions_root, &session);
        blocking(move || {
            match std::fs::remove_file(&jsonl_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(HistoryError::Io(e)),
            }
            log.len_after_last_append = None;
            writer.remove_sidecar(&sidecar).map_err(HistoryError::Io)
        })
        .await?;
        self.homes.lock().await.remove(&session);
        self.synced_dirs.lock().await.remove(&session);
        self.deleted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session);
        Ok(())
    }

    async fn set_session_title(
        &self,
        session: SessionId,
        title: Option<String>,
    ) -> Result<(), HistoryError> {
        if self.was_deleted(session) {
            return Err(HistoryError::SessionDeleted(session.to_string()));
        }
        let _writer = self.writer_lock(session).await?;
        let log = self.log_of(session);
        let _log = log.lock().await;
        let home = self.home_of(session).await;
        crate::project::set_session_title_in(
            &self.sessions_root,
            home.as_deref().unwrap_or_else(|| Path::new("")),
            &project_dir_for(&self.projects_root, home.as_deref()),
            session,
            title,
        )
        .await
    }

    async fn session_stats(&self, session: SessionId) -> Result<SessionStats, HistoryError> {
        crate::project::session_stats_in(&self.sessions_root, session).await
    }

    /// Reads only as far as the `Meta` entry, which the engine writes first.
    /// The default would parse — and hydrate every paste of — an entire
    /// transcript to reach the fields on its first line.
    async fn session_facts(&self, session: SessionId) -> Result<SessionFacts, HistoryError> {
        let path = self.session_path_of(session).await;
        let Some(content) = read_log(&path).await? else {
            return Ok(SessionFacts::default());
        };
        for (i, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let env = serde_json::from_str::<EnvelopedEntry>(line)
                .map_err(|error| HistoryError::Parse { line: i + 1, error })?;
            if let Some(facts) = SessionFacts::of(&env.entry) {
                return Ok(facts);
            }
        }
        Ok(SessionFacts::default())
    }

    /// The sessions of the project this store was built for — not every
    /// session it can reach, which is what `find_sessions` is for.
    async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError> {
        session_ids_in(&self.project_dir_path()).await
    }

    /// Looks in the parent's project rather than this store's own, which is
    /// the same directory the child's transcript went to (`home_from_meta`
    /// gives a sidechain its parent's project). The default walks
    /// `list_sessions`, and would find nothing for a parent belonging to any
    /// project but this store's — taking the cascade delete in
    /// `session.close` down with it.
    async fn child_sessions(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<SessionId>, HistoryError> {
        let dir = match SessionId::parse(parent_session_id) {
            Ok(parent) => self.session_dir_of(parent).await,
            Err(_) => self.project_dir_path(),
        };
        let mut out = Vec::new();
        for sid in session_ids_in(&dir).await? {
            let facts = match self.session_facts(sid).await {
                Ok(facts) => facts,
                Err(e @ HistoryError::Parse { .. }) => {
                    tracing::warn!(%sid, error = %e, "unreadable log left out of a child lookup");
                    continue;
                }
                Err(e) => return Err(e),
            };
            if facts.parent_session_id.as_deref() == Some(parent_session_id) {
                out.push(sid);
            }
        }
        Ok(out)
    }

    /// Orders by the file's modification time and narrows by directory before
    /// reading anything, neither of which the default can do.
    ///
    /// A recency query still opens each transcript to build its preview,
    /// unless the caller asked for [`SummaryDetail::IdsOnly`] — which is what
    /// a listing wants, and what keeps `session.list` a directory read rather
    /// than a parse of every session in the project.
    ///
    /// [`SummaryDetail::IdsOnly`]: crate::query::SummaryDetail::IdsOnly
    async fn find_sessions(
        &self,
        query: &SessionQuery,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        match (&query.scope, query.needle()) {
            (SessionScope::CurrentProject, None) => {
                self.recent_summaries(&self.project_dir_path(), query.limit, query.detail)
                    .await
            }
            (SessionScope::CurrentProject, Some(needle)) => {
                self.search_one_project(&self.project_dir_path(), needle, query.limit)
                    .await
            }
            (SessionScope::Project(home), needle) => {
                // Resolved the same way the project a session records is, or
                // a caller naming the project it just created a session in
                // would miss it over a symlink (`/tmp` against
                // `/private/tmp`, every macOS temp directory).
                let dir = self.project_dir_of_root(home.as_deref()).await;
                match needle {
                    None => self.recent_summaries(&dir, query.limit, query.detail).await,
                    Some(needle) => self.search_one_project(&dir, needle, query.limit).await,
                }
            }
            (SessionScope::AllProjects, _) => {
                self.search_project_dirs(&query.text, query.limit, ProjectDirFilter::All)
                    .await
            }
            (SessionScope::Under(root), _) => {
                self.search_project_dirs(
                    &query.text,
                    query.limit,
                    ProjectDirFilter::UnderPath(root.clone()),
                )
                .await
            }
        }
    }
}
