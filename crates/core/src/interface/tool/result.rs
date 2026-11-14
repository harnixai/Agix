//! What a running tool call reports and returns: progress, validation, result blocks.

use super::*;

/// One progress report of a running tool call. `progress` and `total` are in
/// whatever unit the tool counts (lines, bytes, steps); either may be absent.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct ProgressUpdate {
    pub message: String,
    pub progress: Option<f64>,
    pub total: Option<f64>,
}

impl ProgressUpdate {
    pub fn text(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }
    pub fn with_progress(mut self, progress: f64) -> Self {
        self.progress = Some(progress);
        self
    }
    pub fn with_total(mut self, total: f64) -> Self {
        self.total = Some(total);
        self
    }
}

/// What a tool call reports progress through. The engine hands every call
/// one that reaches the session's event stream as `AgentEvent::ToolProgress`,
/// rate-limited per call; reports after the call returns are dropped.
#[derive(Clone)]
pub struct ProgressSender {
    tool_use_id: String,
    callback: Option<Arc<dyn ProgressCallback>>,
}
pub trait ProgressCallback: Send + Sync {
    fn on_progress(&self, tool_use_id: &str, data: &str);
    fn on_update(&self, tool_use_id: &str, update: ProgressUpdate) {
        self.on_progress(tool_use_id, &update.message);
    }
}
impl ProgressSender {
    pub fn noop(id: impl Into<String>) -> Self {
        Self {
            tool_use_id: id.into(),
            callback: None,
        }
    }
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            tool_use_id: id.into(),
            callback: None,
        }
    }
    pub fn with_callback(id: impl Into<String>, cb: Arc<dyn ProgressCallback>) -> Self {
        Self {
            tool_use_id: id.into(),
            callback: Some(cb),
        }
    }
    pub fn send(&self, data: &str) {
        if let Some(ref cb) = self.callback {
            cb.on_progress(&self.tool_use_id, data);
        }
    }
    pub fn update(&self, update: ProgressUpdate) {
        if let Some(ref cb) = self.callback {
            cb.on_update(&self.tool_use_id, update);
        }
    }
    pub fn tool_use_id(&self) -> &str {
        &self.tool_use_id
    }
}

#[derive(Debug, Clone)]
pub enum ValidationResult {
    Ok,
    Err(String, i32),
}
impl ValidationResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
    pub fn is_err(&self) -> bool {
        matches!(self, Self::Err(..))
    }
    pub fn err(m: impl Into<String>, c: i32) -> Self {
        Self::Err(m.into(), c)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[non_exhaustive]
pub struct ToolResult {
    pub content: ToolResultContent,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_meta: Option<McpMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_messages: Option<Vec<Value>>,
    /// The files this call wrote, for the host; the model never sees it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_changes: Vec<FileChange>,
}

/// One file a tool call wrote, as `tool_result` reports it to the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[non_exhaustive]
pub struct FileChange {
    /// The resolved write target, absolute.
    pub path: String,
    pub op: FileOp,
    /// `sha256:<hex>` of the content before; `None` when there was none or
    /// it was too large to hash.
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub before_bytes: Option<u64>,
    pub after_bytes: u64,
    pub added: u64,
    pub removed: u64,
    /// `--- a/<path>` / `+++ b/<path>`, three lines of context.
    pub unified_diff: Option<String>,
    /// The diff stops at a hunk boundary short of the whole change.
    pub truncated: bool,
    /// Why there is no diff, when there is none.
    pub omitted: Option<DiffOmitted>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FileOp {
    Create,
    Modify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiffOmitted {
    TooLarge,
    Binary,
    /// A result transformer rewrote the call's text, and the diff would
    /// carry the file content it may have removed.
    Redacted,
}

impl FileChange {
    pub fn new(path: impl Into<String>, op: FileOp, after_bytes: u64) -> Self {
        Self {
            path: path.into(),
            op,
            before_hash: None,
            after_hash: None,
            before_bytes: None,
            after_bytes,
            added: 0,
            removed: 0,
            unified_diff: None,
            truncated: false,
            omitted: None,
        }
    }

    /// What may go to a host that must not see the file's content.
    pub fn without_content(mut self) -> Self {
        if self.unified_diff.take().is_some() || self.omitted.is_none() {
            self.omitted = Some(DiffOmitted::Redacted);
        }
        self.truncated = false;
        self
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpMeta {
    #[serde(default)]
    pub server_name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultBlock>),
}
impl Default for ToolResultContent {
    fn default() -> Self {
        Self::Text(String::new())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: Option<String>,
    pub source: Option<Value>,
}

/// Largest image, decoded, that goes to the model as one image block. The API
/// refuses an image over 5 MB of base64 — 3.75 MB decoded — and the refused
/// request would carry it again on every later turn; rounded down to leave
/// room for the rest of the request.
pub const MAX_IMAGE_BYTES: u64 = 3_500_000;

/// Constructors/accessors for the two block shapes the model layer understands.
///
/// The `source` object of an image block is the Anthropic `image.source` shape
/// (`{type: "base64", media_type, data}`) verbatim — MCP servers already hand us
/// that shape, and the runtime's tool-result renderer reads it back out with
/// [`ToolResultBlock::as_image`]. Building it through these two functions keeps
/// the producer (`Read`, the MCP adapter) and the consumer from drifting apart.
impl ToolResultBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            block_type: "text".into(),
            text: Some(s.into()),
            source: None,
        }
    }
    pub fn image(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            block_type: "image".into(),
            text: None,
            source: Some(serde_json::json!({
                "type": "base64",
                "media_type": media_type.into(),
                "data": data.into(),
            })),
        }
    }
    /// `(media_type, base64_data)` when this is a well-formed image block.
    pub fn as_image(&self) -> Option<(&str, &str)> {
        if self.block_type != "image" {
            return None;
        }
        let source = self.source.as_ref()?;
        Some((
            source.get("media_type")?.as_str()?,
            source.get("data")?.as_str()?,
        ))
    }
}

impl ToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: ToolResultContent::Text(s.into()),
            is_error: false,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
            file_changes: Vec::new(),
        }
    }
    pub fn error_text(s: impl Into<String>) -> Self {
        Self {
            content: ToolResultContent::Text(s.into()),
            is_error: true,
            structured_content: None,
            mcp_meta: None,
            new_messages: None,
            file_changes: Vec::new(),
        }
    }
    pub fn blocks(blocks: Vec<ToolResultBlock>) -> Self {
        Self {
            content: ToolResultContent::Blocks(blocks),
            ..Self::default()
        }
    }
    pub fn with_error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }
    pub fn with_structured(mut self, v: Value) -> Self {
        self.structured_content = Some(v);
        self
    }
    pub fn with_mcp_meta(mut self, m: McpMeta) -> Self {
        self.mcp_meta = Some(m);
        self
    }
    pub fn with_new_messages(mut self, m: Vec<Value>) -> Self {
        self.new_messages = Some(m);
        self
    }
    pub fn with_file_change(mut self, change: FileChange) -> Self {
        self.file_changes.push(change);
        self
    }
}
