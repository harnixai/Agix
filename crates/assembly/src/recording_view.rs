//! 录像到协议形状的投影：`session.calls` 的摘要行、`session.call` 的全文、
//! `session.callBlob` 的引用解析，以及 `session.rerunCall` 的改写与差异（`rerun`）。
//! 录像的行格式与 blob 存储是 telemetry 的内部格式，宿主见到的只有这里产出的 `api` 类型；
//! 投影是协议契约，由同目录的黄金用例钉住。磁盘读全是同步的，调用方放到阻塞线程上跑。

use crate::session_pool::api::{
    BlobRef, CallBlob, CallDetail, CallError, CallInput, CallOutput, CallPart, CallRequest,
    CallResponse, CallStatus, CallSummary, CallTiming, CallUsage, MissingPart, RecordingCoverage,
    RecordingInfo, SystemBlockSize, SystemBlockView, ToolUseRef, ToolUseView,
};
use base::interface::model::{MessageRole, ModelEvent, Usage};
use base::prompt::{BlockOrigin, BlockRole, CacheStrategy, PromptBlock};
use base::text::truncate_chars;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use telemetry::recorder::blob::{BlobId, BlobStore};
use telemetry::recorder::format::Outcome;
use telemetry::recorder::reader::{self, ReadError, RecordedCall, Recording};

/// One part inlined in `session.call` at most this long (its JSON).
pub(crate) const INLINE_PART_MAX: usize = 64 * 1024;
/// All parts inlined in one `session.call` together at most this long.
pub(crate) const INLINE_TOTAL_MAX: usize = 4 * 1024 * 1024;
/// How much of an answer or a tool's arguments a summary row quotes.
const PREVIEW_CHARS: usize = 200;
/// A message blob larger than this is not read to tell whether a tool call
/// was answered with an error; the row leaves the flag out.
const TAIL_MESSAGE_MAX: u64 = 1024 * 1024;
/// A part `session.callBlob` refuses to answer with: it would not fit a frame.
pub(crate) const BLOB_MAX: usize = 15 * 1024 * 1024;

/// A recording opened for reading, with the blobs it points into.
pub(crate) struct Opened {
    pub(crate) recording: Recording,
    pub(crate) blobs: BlobStore,
}

/// The recording in `dir`, or `None` when nothing has been written there.
pub(crate) fn open(dir: &Path) -> Result<Option<Opened>, String> {
    match reader::load(dir) {
        Ok(recording) => Ok(Some(Opened {
            recording,
            blobs: BlobStore::new(dir),
        })),
        Err(ReadError::NotFound(_) | ReadError::MissingHeader) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

impl Opened {
    pub(crate) fn generation(&self) -> u64 {
        self.recording.header.created_at
    }

    pub(crate) fn info(&self) -> RecordingInfo {
        RecordingInfo {
            name: self.recording.header.name.clone(),
            created_at: rfc3339(self.recording.header.created_at),
            covers: RecordingCoverage::LatestRun,
            generation: self.generation(),
        }
    }

    /// Up to `limit` calls from `from`, and where the next read starts. A
    /// call still `writing` holds `next` back to itself.
    pub(crate) fn summaries(
        &self,
        from: usize,
        limit: usize,
        writing: bool,
    ) -> (Vec<CallSummary>, usize) {
        let calls = &self.recording.calls;
        let start = from.min(calls.len());
        let end = start.saturating_add(limit).min(calls.len());
        let mut sizes = Sizes::new(&self.blobs);
        let mut results = ResultFlags::new(&self.blobs);
        let next_conversation = self.next_conversation_calls();
        let mut next = end;
        let mut rows = Vec::with_capacity(end - start);
        for (index, call) in calls.iter().enumerate().take(end).skip(start) {
            let status = status_of(call, writing);
            if status == CallStatus::InFlight {
                next = next.min(index);
            }
            let follow_up = next_conversation[index].map(|i| &calls[i]);
            rows.push(self.summary(index, call, status, follow_up, &mut sizes, &mut results));
        }
        (rows, next)
    }

    fn summary(
        &self,
        index: usize,
        call: &RecordedCall,
        status: CallStatus,
        follow_up: Option<&RecordedCall>,
        sizes: &mut Sizes<'_>,
        results: &mut ResultFlags<'_>,
    ) -> CallSummary {
        let request = &call.request;
        let response = Response::of(&call.response);
        let flags = match follow_up {
            Some(next) if status == CallStatus::Ok && !response.tool_uses.is_empty() => results
                .of(next)
                .as_ref()
                .filter(|tail| tail.continues(&response.tool_uses))
                .map(|tail| &tail.is_error),
            _ => None,
        };
        CallSummary {
            index,
            session_id: self.session_of(call),
            parent_session_id: request.parent_session_id.clone(),
            agent_type: request.agent_type.clone(),
            turn_id: request.turn_id.clone(),
            turn: request.turn,
            step: request.step,
            purpose: request.purpose.clone(),
            ts: rfc3339(request.ts),
            provider: request.provider.clone(),
            model: request.params.model.clone(),
            input: CallInput {
                system: request.system.iter().map(|id| sizes.block(id)).collect(),
                tools: sizes.tools(&request.tools),
                tools_digest: request.tools.0.clone(),
                messages: request.messages.len(),
            },
            output: CallOutput {
                chars: response.text.chars().count(),
                text_preview: truncate_chars(&response.text, PREVIEW_CHARS, "…"),
                thinking_chars: response.thinking_chars,
                tool_uses: response
                    .tool_uses
                    .iter()
                    .map(|t| ToolUseRef {
                        id: t.id.clone(),
                        name: t.name.clone(),
                        input_preview: truncate_chars(
                            &preview_source(&t.input),
                            PREVIEW_CHARS,
                            "…",
                        ),
                        result_is_error: flags.and_then(|f| f.get(&t.id).copied()),
                    })
                    .collect(),
            },
            status,
            stop_reason: call.end.as_ref().and_then(|e| e.stop_reason.clone()),
            usage: usage_of(call),
            timing: CallTiming {
                sent_at: rfc3339(request.ts),
                ttft_ms: ttft_of(call),
                duration_ms: call.end.as_ref().map(|e| e.duration_ms),
            },
            error: error_of(call),
        }
    }

    /// For each call, the next call of its session that is not an auxiliary
    /// one: the one whose messages carry the results of the tools it asked for.
    fn next_conversation_calls(&self) -> Vec<Option<usize>> {
        let calls = &self.recording.calls;
        let mut latest: HashMap<&str, usize> = HashMap::new();
        let mut next = vec![None; calls.len()];
        for (index, call) in calls.iter().enumerate().rev() {
            let session = call
                .request
                .session_id
                .as_deref()
                .unwrap_or(&self.recording.header.session_id);
            next[index] = latest.get(session).copied();
            if call.request.purpose.is_none() {
                latest.insert(session, index);
            }
        }
        next
    }

    /// The call at `index` in full, or `None` past the end.
    pub(crate) fn detail(
        &self,
        session_id: &str,
        index: usize,
        writing: bool,
    ) -> Option<CallDetail> {
        let call = self.recording.calls.get(index)?;
        let request = &call.request;
        let (system, tools, messages) = self.request_parts(request);
        let response = Response::of(&call.response);
        Some(CallDetail {
            session_id: session_id.to_string(),
            index,
            generation: self.generation(),
            request: CallRequest {
                session_id: self.session_of(call),
                parent_session_id: request.parent_session_id.clone(),
                agent_type: request.agent_type.clone(),
                turn_id: request.turn_id.clone(),
                turn: request.turn,
                step: request.step,
                purpose: request.purpose.clone(),
                provider: request.provider.clone(),
                model: request.params.model.clone(),
                max_tokens: request.params.max_tokens,
                thinking: request.params.thinking_mode.clone(),
                effort: request.params.effort,
                system,
                tools,
                messages,
                input_map: request
                    .input_map
                    .as_ref()
                    .and_then(|m| serde_json::to_value(m).ok()),
            },
            response: CallResponse {
                status: status_of(call, writing),
                text: response.text,
                thinking_chars: response.thinking_chars,
                tool_uses: response.tool_uses,
                stop_reason: call.end.as_ref().and_then(|e| e.stop_reason.clone()),
                usage: usage_of(call),
                ttft_ms: ttft_of(call),
                duration_ms: call.end.as_ref().map(|e| e.duration_ms),
                error: error_of(call),
            },
        })
    }

    /// The system blocks, tool table and messages of `request`, inlined in
    /// order until a part or the running total passes its limit.
    fn request_parts(
        &self,
        request: &telemetry::recorder::format::CallRecord,
    ) -> (
        Vec<CallPart<SystemBlockView>>,
        CallPart<Value>,
        Vec<CallPart<Value>>,
    ) {
        let mut budget = INLINE_TOTAL_MAX;
        let mut inline = |kind: PartKind, id: &BlobId| -> CallPart<Value> {
            match self.part(kind, id) {
                None => CallPart::Missing(MissingPart {
                    missing: id.0.clone(),
                }),
                Some((_, bytes)) if bytes > INLINE_PART_MAX || bytes > budget => {
                    CallPart::Blob(BlobRef {
                        blob: kind.handle(id),
                        bytes,
                    })
                }
                Some((value, bytes)) => {
                    budget -= bytes;
                    CallPart::Inline(value)
                }
            }
        };
        let system = request
            .system
            .iter()
            .map(|id| match inline(PartKind::System, id) {
                CallPart::Inline(value) => serde_json::from_value(value)
                    .map(CallPart::Inline)
                    .unwrap_or_else(|_| {
                        CallPart::Missing(MissingPart {
                            missing: id.0.clone(),
                        })
                    }),
                CallPart::Blob(r) => CallPart::Blob(r),
                CallPart::Missing(m) => CallPart::Missing(m),
            })
            .collect();
        let tools = inline(PartKind::Tools, &request.tools);
        let messages = request
            .messages
            .iter()
            .map(|id| inline(PartKind::Message, id))
            .collect();
        (system, tools, messages)
    }

    /// The part `handle` names, as `session.call` would have inlined it.
    pub(crate) fn blob(&self, handle: &str) -> Result<CallBlob, BlobLookup> {
        let (kind, id) = PartKind::parse(handle).ok_or(BlobLookup::Malformed)?;
        let (value, bytes) = self.part(kind, &id).ok_or(BlobLookup::Absent)?;
        if bytes > BLOB_MAX {
            return Err(BlobLookup::TooLarge(bytes));
        }
        Ok(CallBlob {
            blob: handle.to_string(),
            bytes,
            value,
        })
    }

    /// A blob projected for the host, with the length of its JSON.
    fn part(&self, kind: PartKind, id: &BlobId) -> Option<(Value, usize)> {
        let raw = self.blobs.get_raw(id).ok()??;
        let value = match kind {
            PartKind::System => {
                serde_json::to_value(block_view(&serde_json::from_str(&raw).ok()?)).ok()?
            }
            PartKind::Message => {
                let mut message: Value = serde_json::from_str(&raw).ok()?;
                shrink_media(&mut message);
                message
            }
            PartKind::Tools => serde_json::from_str(&raw).ok()?,
        };
        let bytes = serde_json::to_string(&value).map(|s| s.len()).ok()?;
        Some((value, bytes))
    }

    fn session_of(&self, call: &RecordedCall) -> String {
        call.request
            .session_id
            .clone()
            .unwrap_or_else(|| self.recording.header.session_id.clone())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BlobLookup {
    Malformed,
    Absent,
    TooLarge(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartKind {
    System,
    Message,
    Tools,
}

impl PartKind {
    fn handle(self, id: &BlobId) -> String {
        let prefix = match self {
            Self::System => 's',
            Self::Message => 'm',
            Self::Tools => 't',
        };
        format!("{prefix}.{}", id.0)
    }

    /// `<s|m|t>.<16 lowercase hex>` and nothing else: the id becomes a file
    /// name under the recording's `blobs/`.
    fn parse(handle: &str) -> Option<(Self, BlobId)> {
        let (prefix, id) = handle.split_once('.')?;
        let kind = match prefix {
            "s" => Self::System,
            "m" => Self::Message,
            "t" => Self::Tools,
            _ => return None,
        };
        let hex = id.len() == 16
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        hex.then(|| (kind, BlobId(id.to_string())))
    }
}

fn block_view(block: &PromptBlock) -> SystemBlockView {
    SystemBlockView {
        role: match block.role {
            BlockRole::System => "system",
            BlockRole::User => "user",
            BlockRole::Assistant => "assistant",
        }
        .to_string(),
        cache: block.cache_strategy.map(|c| {
            match c {
                CacheStrategy::Ephemeral => "ephemeral",
                CacheStrategy::Global => "global",
            }
            .to_string()
        }),
        source: block.name.clone(),
        origin: match &block.origin {
            BlockOrigin::Kernel => "kernel".to_string(),
            BlockOrigin::Config => "config".to_string(),
            BlockOrigin::Plugin(id) => format!("plugin:{id}"),
            BlockOrigin::Script(path) => format!("script:{path}"),
            BlockOrigin::Host(name) => format!("host:{name}"),
            BlockOrigin::ConfigEntry { config } => format!("config:{config}"),
            other => format!("{other:?}").to_lowercase(),
        },
        text: block.content.clone(),
    }
}

/// Image and document blocks keep their type and media type, and the
/// decoded size in place of the base64.
fn shrink_media(message: &mut Value) {
    let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in blocks {
        let kind = block.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("image" | "document")) {
            continue;
        }
        let Some(object) = block.as_object_mut() else {
            continue;
        };
        if let Some(Value::String(data)) = object.remove("data") {
            let padding = data.bytes().rev().take_while(|b| *b == b'=').count();
            object.insert(
                "bytes".into(),
                (data.len() / 4 * 3).saturating_sub(padding.min(2)).into(),
            );
        }
    }
}

/// System block sizes and tool counts, each blob read once per request.
struct Sizes<'a> {
    blobs: &'a BlobStore,
    blocks: HashMap<BlobId, SystemBlockSize>,
    tools: HashMap<BlobId, Option<usize>>,
}

impl<'a> Sizes<'a> {
    fn new(blobs: &'a BlobStore) -> Self {
        Self {
            blobs,
            blocks: HashMap::new(),
            tools: HashMap::new(),
        }
    }

    fn block(&mut self, id: &BlobId) -> SystemBlockSize {
        let blobs = self.blobs;
        self.blocks
            .entry(id.clone())
            .or_insert_with(|| match blobs.get::<PromptBlock>(id) {
                Ok(Some(block)) => SystemBlockSize {
                    source: block.name,
                    chars: Some(block.content.chars().count()),
                    digest: id.0.clone(),
                },
                _ => SystemBlockSize {
                    source: None,
                    chars: None,
                    digest: id.0.clone(),
                },
            })
            .clone()
    }

    fn tools(&mut self, id: &BlobId) -> Option<usize> {
        let blobs = self.blobs;
        *self.tools.entry(id.clone()).or_insert_with(|| {
            blobs
                .get::<Vec<serde::de::IgnoredAny>>(id)
                .ok()
                .flatten()
                .map(|t| t.len())
        })
    }
}

/// What a conversation call's trailing messages say about the call before it:
/// which tool uses the last assistant message made, and whether each result
/// was an error. Each call's messages are read once however many rows ask.
struct ResultFlags<'a> {
    blobs: &'a BlobStore,
    by_call: HashMap<u64, Option<Tail>>,
}

impl<'a> ResultFlags<'a> {
    fn new(blobs: &'a BlobStore) -> Self {
        Self {
            blobs,
            by_call: HashMap::new(),
        }
    }

    fn of(&mut self, call: &RecordedCall) -> &Option<Tail> {
        let blobs = self.blobs;
        self.by_call
            .entry(call.request.seq)
            .or_insert_with(|| Tail::read(blobs, &call.request.messages))
    }
}

struct Tail {
    tool_use_ids: HashSet<String>,
    is_error: HashMap<String, bool>,
}

impl Tail {
    fn continues(&self, tool_uses: &[ToolUseView]) -> bool {
        self.tool_use_ids.len() == tool_uses.len()
            && tool_uses.iter().all(|t| self.tool_use_ids.contains(&t.id))
    }

    /// `None` when an assistant message is not reached through readable,
    /// reasonably sized blobs.
    fn read(blobs: &BlobStore, messages: &[BlobId]) -> Option<Self> {
        let mut is_error = HashMap::new();
        for id in messages.iter().rev() {
            if blobs.len(id)? > TAIL_MESSAGE_MAX {
                return None;
            }
            let message: SlimMessage = blobs.get(id).ok()??;
            if message.role == MessageRole::Assistant {
                let tool_use_ids = message
                    .content
                    .into_iter()
                    .filter(|b| b.kind == "tool_use")
                    .filter_map(|b| b.id)
                    .collect();
                return Some(Self {
                    tool_use_ids,
                    is_error,
                });
            }
            for block in message.content {
                if let (true, Some(id)) = (block.kind == "tool_result", block.tool_use_id) {
                    is_error.insert(id, block.is_error.unwrap_or(false));
                }
            }
        }
        None
    }
}

/// A message with only what [`Tail`] needs; tool outputs and image data are
/// skipped by the parser instead of being built.
#[derive(Deserialize)]
struct SlimMessage {
    role: MessageRole,
    content: Vec<SlimBlock>,
}

#[derive(Deserialize)]
struct SlimBlock {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    tool_use_id: Option<String>,
    is_error: Option<bool>,
}

fn preview_source(input: &Value) -> String {
    match input {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

/// What came back, folded from the recorded events.
struct Response {
    text: String,
    thinking_chars: usize,
    tool_uses: Vec<ToolUseView>,
}

impl Response {
    fn of(events: &[ModelEvent]) -> Self {
        let mut out = Self {
            text: String::new(),
            thinking_chars: 0,
            tool_uses: Vec::new(),
        };
        for event in events {
            match event {
                ModelEvent::TextDelta { text } => out.text.push_str(text),
                ModelEvent::ThinkingDelta { text } => out.thinking_chars += text.chars().count(),
                ModelEvent::ToolUse { id, name, input } => out.tool_uses.push(ToolUseView {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    malformed: false,
                }),
                ModelEvent::MalformedToolUse {
                    id,
                    name,
                    raw_input,
                    ..
                } => out.tool_uses.push(ToolUseView {
                    id: id.clone(),
                    name: name.clone(),
                    input: Value::String(raw_input.clone()),
                    malformed: true,
                }),
                _ => {}
            }
        }
        out
    }
}

fn status_of(call: &RecordedCall, writing: bool) -> CallStatus {
    match call.end.as_ref().map(|e| &e.outcome) {
        Some(Outcome::Ok) => CallStatus::Ok,
        Some(Outcome::Cancelled) => CallStatus::Cancelled,
        Some(Outcome::Error { .. }) => CallStatus::Error,
        None if writing => CallStatus::InFlight,
        None => CallStatus::Unfinished,
    }
}

fn usage_of(call: &RecordedCall) -> Option<CallUsage> {
    call.end.as_ref()?.usage.map(call_usage)
}

fn call_usage(usage: Usage) -> CallUsage {
    CallUsage {
        input_tokens: usage.input_tokens.into(),
        output_tokens: usage.output_tokens.into(),
        cache_creation_input_tokens: usage.cache_creation_input_tokens.into(),
        cache_read_input_tokens: usage.cache_read_input_tokens.into(),
    }
}

fn ttft_of(call: &RecordedCall) -> Option<u64> {
    call.first_event_at
        .map(|first| first.saturating_sub(call.request.ts))
}

fn error_of(call: &RecordedCall) -> Option<CallError> {
    let Outcome::Error { error } = &call.end.as_ref()?.outcome else {
        return None;
    };
    Some(call_error(error))
}

fn call_error(error: &telemetry::recorder::format::RecordedError) -> CallError {
    CallError {
        kind: serde_json::to_value(error.error)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        message: error.message.clone(),
        http_status: error.http_status,
        retry_after_ms: error.retry_after_ms,
    }
}

fn rfc3339(ms: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

mod rerun;
pub(crate) use rerun::{apply_edits, end_usage, rerun_result, RerunOf};

#[cfg(test)]
mod tests;
