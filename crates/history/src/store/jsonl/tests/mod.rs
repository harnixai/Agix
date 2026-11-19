//! JSONL backend tests: behavior in `basic`, large content in `blobs`,
//! lineage snapshots in `lineage`, query equivalence in `query`, crash
//! and concurrency in `crash`.

use super::*;
use crate::entry::EnvelopedEntry;
use crate::project::SessionMetadata;
use crate::query::{SessionQuery, SessionScope};
use crate::store::{
    HistoryStore, InMemoryHistoryStore, SessionFacts, SessionStats, SessionSummary,
};
use crate::transcript::{preview_messages, project_messages_with};
use async_trait::async_trait;
use tempfile::TempDir;

async fn make_store() -> (JsonlHistoryStore, TempDir, TempDir) {
    let cwd_tmp = TempDir::new().unwrap();
    let projects_tmp = TempDir::new().unwrap();
    let store =
        JsonlHistoryStore::with_roots(cwd_tmp.path(), HistoryRoots::under(projects_tmp.path()))
            .await
            .unwrap();
    (store, cwd_tmp, projects_tmp)
}

impl JsonlHistoryStore {
    async fn path_of(&self, session: SessionId) -> PathBuf {
        self.session_path_of(session, Access::Write)
            .await
            .unwrap()
            .0
    }

    async fn dir_of(&self, session: SessionId) -> PathBuf {
        let home = self.home_of(session, Access::Write).await.unwrap();
        self.homes.transcript_dir(&home)
    }

    async fn measure(
        &self,
        root: SessionId,
    ) -> (HashMap<SessionId, (PathBuf, Home)>, HashMap<SessionId, u64>) {
        let home = self.home_of(root, Access::Read).await.unwrap();
        let logs = self.beside(&home, Access::Read).await.unwrap();
        let lengths = logs
            .iter()
            .map(|(id, (path, _))| (*id, std::fs::metadata(path).unwrap().len()))
            .collect();
        (logs, lengths)
    }
}

mod basic;
mod blobs;
pub(in crate::store) mod crash;
mod lineage;
mod project;
mod query;
