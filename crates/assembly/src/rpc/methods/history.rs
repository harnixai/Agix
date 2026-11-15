//! 会话日志方法（`session.history` / `appendEntry` / `entries` / `export` /
//! `import`）、录像读取（`calls` / `call` / `callBlob`）与内容搜索（`search`）的
//! params。

use super::session::ProjectRootParam;
use crate::session_pool::api::ArchiveFormat;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct HistoryParams {
    pub session_id: String,
    #[serde(default)]
    pub offset: u64,
    /// Clamped to 500 rather than refused; 100 when omitted.
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct AppendEntryParams {
    pub session_id: String,
    pub ns: String,
    pub event: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct EntriesParams {
    pub session_id: String,
    /// Only these namespaces; all of the host's when omitted.
    #[serde(default)]
    pub namespaces: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ImportParams {
    /// Absolute, on the daemon's machine: a zip written by `session.export`,
    /// or the directory of a `format: "dir"` export.
    pub path: PathBuf,
    /// Omitted: the root session's own; `null`: no project.
    #[serde(default, deserialize_with = "ProjectRootParam::present")]
    #[schemars(with = "Option<PathBuf>")]
    pub project_root: ProjectRootParam,
    /// Keep the archive's session ids instead of minting new ones.
    #[serde(default)]
    pub keep_ids: bool,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ExportParams {
    pub session_id: String,
    /// Absolute, on the daemon's machine; must not exist yet.
    pub path: PathBuf,
    #[serde(default)]
    pub format: ArchiveFormat,
    /// `sidechains`, `attachments`, `recording`, `checkpoints`, `tool_results`; the first two and `tool_results` when omitted.
    #[serde(default)]
    pub include: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct CallsParams {
    pub session_id: String,
    /// The call index to start from; 0 when omitted.
    #[serde(default)]
    pub from: u64,
    /// Clamped to 500 rather than refused; 200 when omitted.
    #[serde(default)]
    pub limit: Option<u64>,
    /// The `generation` an earlier read saw; a recording started over since
    /// is `INVALID_PARAMS` with the current one in `data.generation`.
    #[serde(default)]
    pub generation: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct CallParams {
    pub session_id: String,
    pub index: u64,
    #[serde(default)]
    pub generation: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct CallBlobParams {
    pub session_id: String,
    /// A `blob` reference from `session.call`.
    pub blob: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// Matched trimmed and case-insensitively; must not be empty.
    pub query: String,
    /// Only these sessions, in this order, sidechains included; omitted:
    /// `session.list`'s range, most recent first.
    #[serde(default)]
    pub session_ids: Option<Vec<String>>,
    /// Sessions with a hit; 20 when omitted, clamped to 100.
    #[serde(default)]
    pub limit: Option<u64>,
    /// Sessions read at most; 500 when omitted, clamped to 5000.
    #[serde(default)]
    pub max_sessions: Option<u64>,
    /// 2000 when omitted, clamped to 10000.
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    /// 120 when omitted, clamped to 500.
    #[serde(default)]
    pub snippet_chars: Option<u64>,
}
