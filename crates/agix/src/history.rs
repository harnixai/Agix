//! 会话日志：存储契约、本机 JSONL 存储与它的目录布局、条目类型；`Transcripts::In` 收的就是这里的存储。

pub use ::history::store::HistoryStore;
pub use ::history::store::JsonlHistoryStore;
pub use ::history::path::HistoryRoots;
pub use ::history::path::project_dir;
pub use ::history::path::session_file;
pub use ::history::path::session_metadata_file;
pub use ::history::entry::LogEntry;
pub use ::history::entry::EnvelopedEntry;
pub use ::history::entry::MessageOrigin;
pub use ::history::entry::SessionKind;
pub use ::history::entry::CURRENT_SCHEMA_VERSION;
pub use ::history::entry::CURRENT_META_SCHEMA_VERSION;
pub use ::history::project::SessionMetadata;
pub use ::base::message::ContentBlock;
