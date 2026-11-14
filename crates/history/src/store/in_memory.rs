//! `HistoryStore` kept in memory: the contract and nothing else, for tests
//! and for checking a new backend against.

use crate::entry::{EnvelopedEntry, LogEntry};
use crate::error::HistoryError;
use async_trait::async_trait;
use base::session::SessionId;
use std::collections::HashMap;

use super::{lineage_of, HistoryStore, LineageSnapshot, SessionFacts, SessionStats};

/// Keeps every session's log in memory and forgets it when dropped.
///
/// The second implementation, and the one that makes [`HistoryStore`] a
/// contract rather than one type's interface. Useful in its own right — a
/// host embedding the engine for a throwaway conversation has no reason to
/// touch the disk — and useful as the thing a new backend is checked against:
/// it does nothing but honor the contract, so a test that passes here and
/// fails elsewhere is testing the backend, not the engine.
#[derive(Default)]
pub struct InMemoryHistoryStore {
    sessions: std::sync::Mutex<std::collections::HashMap<SessionId, Vec<EnvelopedEntry>>>,
    titles: std::sync::Mutex<std::collections::HashMap<SessionId, String>>,
    deleted: std::sync::Mutex<std::collections::HashSet<SessionId>>,
    checkpoints: std::sync::Mutex<
        HashMap<SessionId, std::sync::Arc<crate::checkpoint::InMemoryCheckpointObjects>>,
    >,
}

impl InMemoryHistoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl HistoryStore for InMemoryHistoryStore {
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
        {
            let mut deleted = self.deleted.lock().unwrap_or_else(|e| e.into_inner());
            if deleted.contains(&session) {
                if !matches!(entry, LogEntry::Meta { .. }) {
                    return Err(HistoryError::SessionDeleted(session.to_string()));
                }
                deleted.remove(&session);
            }
        }
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session)
            .or_default()
            .push(EnvelopedEntry::new_in(self.environment(), session, entry).with_tags(tags));
        Ok(())
    }

    async fn append_enveloped(
        &self,
        session: SessionId,
        entries: Vec<EnvelopedEntry>,
    ) -> Result<(), HistoryError> {
        if entries.is_empty() {
            return Ok(());
        }
        self.deleted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session);
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session)
            .or_default()
            .extend(entries.into_iter().map(|mut e| {
                e.session_id = session;
                e
            }));
        Ok(())
    }

    async fn load(&self, session: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session)
            .cloned()
            // Absent is not empty: a caller resuming a session it believes in
            // needs to hear that it is not here.
            .ok_or_else(|| HistoryError::SessionNotFound(session.to_string()))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError> {
        Ok(self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect())
    }

    async fn delete(&self, session: SessionId) -> Result<(), HistoryError> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session);
        self.titles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session);
        self.checkpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session);
        self.deleted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session);
        Ok(())
    }

    /// Taken under the lock every append takes, so the logs are cut at one
    /// instant.
    async fn snapshot_lineage(&self, root: SessionId) -> Result<LineageSnapshot, HistoryError> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if !sessions.contains_key(&root) {
            return Err(HistoryError::SessionNotFound(root.to_string()));
        }
        let facts: HashMap<SessionId, SessionFacts> = sessions
            .iter()
            .map(|(session, entries)| {
                let facts = entries
                    .iter()
                    .find_map(|env| SessionFacts::of(&env.entry))
                    .unwrap_or_default();
                (*session, facts)
            })
            .collect();
        let (members, forks) = lineage_of(root, &facts);
        Ok(LineageSnapshot {
            sessions: members
                .into_iter()
                .filter_map(|s| sessions.get(&s).map(|entries| (s, entries.clone())))
                .collect(),
            forks,
        })
    }

    fn checkpoint_objects(
        &self,
        root: SessionId,
    ) -> Option<std::sync::Arc<dyn crate::blob::BlobStore>> {
        let objects = self
            .checkpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(root)
            .or_default()
            .clone();
        Some(objects)
    }

    async fn set_session_title(
        &self,
        session: SessionId,
        title: Option<String>,
    ) -> Result<(), HistoryError> {
        let mut titles = self.titles.lock().unwrap_or_else(|e| e.into_inner());
        match title {
            Some(t) => titles.insert(session, t),
            None => titles.remove(&session),
        };
        Ok(())
    }

    async fn session_stats(&self, session: SessionId) -> Result<SessionStats, HistoryError> {
        Ok(SessionStats {
            title: self
                .titles
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&session)
                .cloned(),
            ..SessionStats::default()
        })
    }
}
