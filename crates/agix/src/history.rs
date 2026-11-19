//! 会话日志：存储契约与它的错误、本机 JSONL 存储与它的目录布局、会话放进项目时宿主回答的 `ProjectHistory`、条目类型；`Transcripts::In` 收的就是这里的存储。

pub use ::base::message::ContentBlock;
pub use ::history::entry::EnvelopedEntry;
pub use ::history::entry::LogEntry;
pub use ::history::entry::MessageOrigin;
pub use ::history::entry::SessionKind;
pub use ::history::entry::CURRENT_META_SCHEMA_VERSION;
pub use ::history::entry::CURRENT_SCHEMA_VERSION;
pub use ::history::error::HistoryError;
pub use ::history::path::project_dir;
pub use ::history::path::session_file;
pub use ::history::path::session_metadata_file;
pub use ::history::path::HistoryRoots;
pub use ::history::placement::ProjectHistory;
pub use ::history::placement::ProjectHistoryDir;
pub use ::history::placement::SessionIndex;
pub use ::history::placement::SidecarLocator;
pub use ::history::project::SessionMetadata;
pub use ::history::store::HistoryStore;
pub use ::history::store::JsonlHistoryStore;
