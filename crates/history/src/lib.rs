//! Session persistence: the `HistoryStore` contract, the two implementations
//! that ship with it (JSONL on disk, in memory), path sanitization, transcript
//! projection, the session-query vocabulary, content search, the blob store
//! large content is moved into, the observers that watch appends go past, the
//! cross-process lock that keeps one writer per session log, and the file
//! checkpoints a host rewinds to; where a session is kept — globally or in its
//! project — is `placement`.

pub mod blob;
pub mod checkpoint;
pub mod entry;
pub mod error;
mod lock;
pub mod observers;
pub mod own_file;
pub mod path;
pub mod placement;
pub mod project;
pub mod query;
pub mod search;
pub mod store;
pub mod transcript;
