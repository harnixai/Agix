//! Append observers and the decorator that notifies them.

use crate::entry::{EnvelopedEntry, LogEntry};
use crate::error::HistoryError;
use crate::query::SessionQuery;
use crate::transcript::TranscriptProjection;
use async_trait::async_trait;
use base::session::SessionId;
use std::sync::Arc;

use super::{
    HistoryStore, LineageSnapshot, SessionClaim, SessionFacts, SessionStats, SessionSummary,
};

/// Watches entries go into the log.
///
/// Read-only, and read-only *in the types* rather than by agreement: the entry
/// arrives behind a shared reference and nothing is returned, so there is no
/// expression an observer can write that changes what was appended.
///
/// That is not caution, it is the append-only guarantee. Every part of the
/// engine that resumes, forks or replays a session reads position as meaning;
/// an observer that could rewrite an entry on its way past would make the log
/// a record of what something decided to record rather than of what happened.
///
/// Observers run *after* the append has succeeded, so the entry they are shown
/// is already in the log. An observer that panics still takes its caller down —
/// it just cannot un-write anything first.
pub trait AppendObserver: Send + Sync {
    fn observed(&self, session: SessionId, entry: &LogEntry);
}

/// Any store, with observers on its appends.
///
/// A decorator rather than a method on the trait, so a backend does not have
/// to implement — or remember to call — anything to be observable, and so the
/// observation cannot be forgotten by whoever writes the next backend.
pub struct ObservedHistoryStore {
    inner: Arc<dyn HistoryStore>,
    observers: Vec<Arc<dyn AppendObserver>>,
}

impl ObservedHistoryStore {
    pub fn new(inner: Arc<dyn HistoryStore>, observers: Vec<Arc<dyn AppendObserver>>) -> Self {
        Self { inner, observers }
    }
}

#[async_trait]
impl HistoryStore for ObservedHistoryStore {
    async fn append(&self, session: SessionId, entry: LogEntry) -> Result<(), HistoryError> {
        // Written first. An observer sees what is on the record, not what was
        // proposed — a failed append is not an event.
        self.inner.append(session, entry.clone()).await?;
        for observer in &self.observers {
            observer.observed(session, &entry);
        }
        Ok(())
    }

    async fn append_tagged(
        &self,
        session: SessionId,
        entry: LogEntry,
        tags: crate::entry::EntryTags,
    ) -> Result<(), HistoryError> {
        self.inner
            .append_tagged(session, entry.clone(), tags)
            .await?;
        for observer in &self.observers {
            observer.observed(session, &entry);
        }
        Ok(())
    }

    async fn sync(&self, session: SessionId) -> Result<(), HistoryError> {
        self.inner.sync(session).await
    }

    async fn claim(&self, session: SessionId) -> Result<SessionClaim, HistoryError> {
        self.inner.claim(session).await
    }

    async fn load(&self, session: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError> {
        self.inner.load(session).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError> {
        self.inner.list_sessions().await
    }

    async fn delete(&self, session: SessionId) -> Result<(), HistoryError> {
        self.inner.delete(session).await
    }

    async fn load_messages(
        &self,
        session: SessionId,
    ) -> Result<Vec<base::message::Message>, HistoryError> {
        self.inner.load_messages(session).await
    }

    async fn session_facts(&self, session: SessionId) -> Result<SessionFacts, HistoryError> {
        self.inner.session_facts(session).await
    }

    async fn set_session_title(
        &self,
        session: SessionId,
        title: Option<String>,
    ) -> Result<(), HistoryError> {
        self.inner.set_session_title(session, title).await
    }

    async fn session_stats(&self, session: SessionId) -> Result<SessionStats, HistoryError> {
        self.inner.session_stats(session).await
    }

    async fn session_parent(&self, session: SessionId) -> Result<Option<String>, HistoryError> {
        self.inner.session_parent(session).await
    }

    fn projection(&self) -> &dyn TranscriptProjection {
        self.inner.projection()
    }

    fn checkpoint_objects(&self, root: SessionId) -> Option<Arc<dyn crate::blob::BlobStore>> {
        self.inner.checkpoint_objects(root)
    }

    fn environment(&self) -> &dyn base::interface::environment::Environment {
        self.inner.environment()
    }

    async fn child_sessions(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<SessionId>, HistoryError> {
        self.inner.child_sessions(parent_session_id).await
    }

    async fn snapshot_lineage(&self, root: SessionId) -> Result<LineageSnapshot, HistoryError> {
        self.inner.snapshot_lineage(root).await
    }

    async fn find_sessions(
        &self,
        query: &SessionQuery,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        self.inner.find_sessions(query).await
    }
}
