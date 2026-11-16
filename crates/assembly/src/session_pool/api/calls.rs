//! 录像与内容搜索的领域结果类型：`session.calls` / `session.call` /
//! `session.callBlob` / `session.search`。这几个形状是协议契约（由
//! `recording_view` 的黄金用例钉住）；录像文件的行格式不是，它随引擎变。

use serde::{Deserialize, Serialize};

/// `session.calls`: one page of the calls a session's recording holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallList {
    pub session_id: String,
    /// `None` when the session records nothing, or nothing has been written yet.
    pub recording: Option<RecordingInfo>,
    pub calls: Vec<CallSummary>,
    /// Where the next read starts: past the last call returned, or at the
    /// first one still in flight, which a later read returns finished.
    pub next: usize,
    /// Calls in the whole recording.
    pub total: usize,
    /// Lines of the recording that could not be read and were skipped.
    pub damaged: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecordingInfo {
    pub name: String,
    /// When this run's recording began, RFC 3339.
    pub created_at: String,
    pub covers: RecordingCoverage,
    /// Opaque; changes when the recording is started over, which makes every
    /// index read before it meaningless.
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RecordingCoverage {
    /// The session's calls since the process now running it first called.
    LatestRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CallStatus {
    Ok,
    Cancelled,
    Error,
    /// The response is still streaming.
    InFlight,
    /// The recording ends inside this call: the process that made it is gone.
    Unfinished,
}

/// One model call, without its content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallSummary {
    /// Position in the recording; what `session.call` takes.
    pub index: usize,
    /// The session that made the call: the root or one of its sub-agents.
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub agent_type: Option<String>,
    pub turn_id: Option<String>,
    pub turn: u32,
    pub step: u32,
    /// `None` for a conversation call; `compact`, `title`, `memory`, … otherwise.
    pub purpose: Option<String>,
    /// RFC 3339.
    pub ts: String,
    pub provider: String,
    pub model: String,
    pub input: CallInput,
    pub output: CallOutput,
    pub status: CallStatus,
    pub stop_reason: Option<String>,
    pub usage: Option<CallUsage>,
    pub timing: CallTiming,
    pub error: Option<CallError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallInput {
    pub system: Vec<SystemBlockSize>,
    /// Tools offered; `None` when the recording no longer holds the table.
    pub tools: Option<usize>,
    /// The recorded tool table's blob id; equal between two calls when the
    /// table sent was the same.
    pub tools_digest: String,
    pub messages: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SystemBlockSize {
    /// The block's name (`identity`, `skills.catalog`, …).
    pub source: Option<String>,
    /// `None` when the recording no longer holds the block.
    pub chars: Option<usize>,
    /// The block's recorded blob id; equal between two calls when the block
    /// sent was the same.
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallOutput {
    pub chars: usize,
    pub thinking_chars: usize,
    pub tool_uses: Vec<ToolUseRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolUseRef {
    pub id: String,
    pub name: String,
}

/// §3.5's four counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallTiming {
    /// RFC 3339.
    pub sent_at: String,
    /// From sending to the first response event; `None` before one arrived.
    pub ttft_ms: Option<u64>,
    /// `None` while the call has not ended.
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallError {
    /// `rate_limited`, `overloaded`, `auth`, … as the model error kinds name them.
    pub kind: String,
    pub message: String,
    pub http_status: Option<u16>,
    pub retry_after_ms: Option<u64>,
}

/// `session.call`: one call in full.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallDetail {
    pub session_id: String,
    pub index: usize,
    pub generation: u64,
    pub request: CallRequest,
    pub response: CallResponse,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallRequest {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub agent_type: Option<String>,
    pub turn_id: Option<String>,
    pub turn: u32,
    pub step: u32,
    pub purpose: Option<String>,
    pub provider: String,
    pub model: String,
    pub max_tokens: u32,
    pub thinking_mode: base::interface::model::ThinkingMode,
    pub effort: Option<base::interface::model::Effort>,
    pub fallback_model: Option<String>,
    /// The assembled system blocks, in order, each as it was sent.
    pub system: Vec<CallPart<SystemBlockView>>,
    /// The tool table as sent: `name`, `description`, `input_schema`, `source`.
    pub tools: CallPart<serde_json::Value>,
    /// The messages as sent, images and documents reduced to their size.
    pub messages: Vec<CallPart<serde_json::Value>>,
    /// What the turn's user message is made of, as coordinates over `messages`.
    pub input_map: Option<serde_json::Value>,
}

/// A part of a request: inline, or — past the inline limits — a reference
/// `session.callBlob` resolves. `missing` when the recording no longer holds it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
#[schemars(rename = "CallPart_for_{T}")]
#[non_exhaustive]
pub enum CallPart<T> {
    Blob(BlobRef),
    Missing(MissingPart),
    Inline(T),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlobRef {
    /// `s.<id>` a system block, `m.<id>` a message, `t.<id>` the tool table.
    pub blob: String,
    /// Length of the JSON `session.callBlob` answers with.
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissingPart {
    /// The blob id the recording names but no longer holds.
    pub missing: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SystemBlockView {
    /// `system`, `user` or `assistant`.
    pub role: String,
    /// `ephemeral` / `global` cache marking, when the block had one.
    pub cache: Option<String>,
    /// The block's name.
    pub source: Option<String>,
    /// Who put it there: `kernel`, `config`, `plugin:<id>`, `script:<path>`.
    pub origin: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallResponse {
    pub status: CallStatus,
    /// The text blocks of the reply, joined.
    pub text: String,
    pub thinking_chars: usize,
    pub tool_uses: Vec<ToolUseView>,
    pub stop_reason: Option<String>,
    pub usage: Option<CallUsage>,
    pub ttft_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub error: Option<CallError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolUseView {
    pub id: String,
    pub name: String,
    /// The arguments; for a call whose arguments did not parse, the raw text.
    pub input: serde_json::Value,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub malformed: bool,
}

/// `session.callBlob`: one part `session.call` gave by reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallBlob {
    pub blob: String,
    pub bytes: usize,
    /// The part, shaped as `session.call` would have inlined it.
    pub value: serde_json::Value,
}

/// What `session.search` is asked, its bounds already clamped.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub session_ids: Option<Vec<String>>,
    pub limit: usize,
    pub max_sessions: usize,
    pub deadline: std::time::Duration,
    pub snippet_chars: usize,
}

/// `session.search`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    /// Sessions read through.
    pub scanned: usize,
    /// A bound stopped the search before every session in range was read.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchHit {
    pub session_id: String,
    pub name: Option<String>,
    /// The message's position, in `session.history`'s coordinates.
    pub seq: usize,
    pub origin: super::HistoryOrigin,
    /// RFC 3339.
    pub ts: String,
    pub snippet: String,
}
