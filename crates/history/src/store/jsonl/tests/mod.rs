//! JSONL backend tests: behavior in `basic`, large content in `blobs`,
//! lineage snapshots in `lineage`, query equivalence in `query`, crash
//! and concurrency in `crash`.

use super::*;
use crate::entry::EnvelopedEntry;
use crate::query::{SessionQuery, SessionScope};
use crate::store::{
    HistoryStore, InMemoryHistoryStore, SessionFacts, SessionStats, SessionSummary,
};
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

mod basic;
mod blobs;
pub(in crate::store) mod crash;
mod lineage;
mod query;
