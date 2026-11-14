//! `OpenAIResponsesModel` — OpenAI's **Responses API** (`POST <base>/v1/responses`)
//! as a third protocol implementation of [`base::interface::model::Model`].
//!
//! It is what `api_type: "openai_responses"` builds. The HTTP layer, retry
//! ladder, first-byte timeout and error classification are the Chat
//! Completions ones in the parent module; what differs is the body, which is a
//! flat list of typed items instead of role-keyed messages, and the stream,
//! which names every step (`response.output_text.delta`,
//! `response.output_item.done`, `response.completed`) instead of leaving the
//! end of a tool call to be inferred.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base::interface::backoff::{BackoffPolicy, LadderBackoff};
use base::interface::exec::Network;
use base::interface::model::{
    MessageRole, Model, ModelContentBlock, ModelError, ModelEvent, ModelMessage, ModelStream,
    StreamParams, ToolDef, Usage,
};
use base::prompt::PromptBlock;
use base::provider::ApiType;
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures::stream::{Stream, StreamExt};
use platform::LocalNetwork;
use serde::Serialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::stream::chunk_error;
use super::{exec_error, send_with_retry, Inner, STREAM_IDLE_TIMEOUT};

#[derive(Debug, Clone, Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    pub stream: bool,
    /// The engine owns the transcript; nothing is kept server-side to chain
    /// to with `previous_response_id`.
    pub store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

pub fn build_request(
    model: String,
    prompt_blocks: Vec<PromptBlock>,
    tools: Vec<ToolDef>,
    messages: Vec<ModelMessage>,
    max_output_tokens: Option<u32>,
) -> ResponsesRequest {
    let instructions: String = prompt_blocks
        .into_iter()
        .map(|b| b.content)
        .filter(|c| !c.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut input = Vec::with_capacity(messages.len());
    for message in messages {
        push_items(&mut input, message);
    }
    ResponsesRequest {
        model,
        instructions: (!instructions.is_empty()).then_some(instructions),
        input,
        tools: tools
            .into_iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                    "strict": false,
                })
            })
            .collect(),
        stream: true,
        store: false,
        max_output_tokens,
    }
}

/// One `ModelMessage` becomes up to three kinds of item. Tool outputs go
/// first: each has to follow the `function_call` it answers with nothing
/// between, and an incoming user message may put text ahead of its results.
fn push_items(out: &mut Vec<Value>, message: ModelMessage) {
    let assistant = matches!(message.role, MessageRole::Assistant);
    let mut parts: Vec<Value> = Vec::new();
    let mut calls: Vec<Value> = Vec::new();
    let mut outputs: Vec<Value> = Vec::new();

    for block in message.content {
        match block {
            ModelContentBlock::Text { text } => parts.push(json!({
                "type": if assistant { "output_text" } else { "input_text" },
                "text": text,
            })),
            ModelContentBlock::Image { media_type, data } => parts.push(json!({
                "type": "input_image",
                "image_url": format!("data:{media_type};base64,{data}"),
            })),
            ModelContentBlock::Document {
                media_type,
                data,
                title,
            } => parts.push(json!({
                "type": "input_file",
                "filename": title.unwrap_or_else(|| "document.pdf".into()),
                "file_data": format!("data:{media_type};base64,{data}"),
            })),
            ModelContentBlock::ToolUse { id, name, input } => calls.push(json!({
                "type": "function_call",
                "call_id": id,
                "name": name,
                "arguments": input.to_string(),
            })),
            ModelContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => outputs.push(json!({
                "type": "function_call_output",
                "call_id": tool_use_id,
                "output": if is_error.unwrap_or(false) {
                    format!("Error: {content}")
                } else {
                    content
                },
            })),
            ModelContentBlock::Thinking { .. } | ModelContentBlock::RedactedThinking { .. } => {}
        }
    }

    out.append(&mut outputs);
    if !parts.is_empty() {
        out.push(json!({
            "role": if assistant { "assistant" } else { "user" },
            "content": parts,
        }));
    }
    out.append(&mut calls);
}

#[derive(Debug, Default)]
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// Turns the named events of a Responses stream into [`ModelEvent`]s.
#[derive(Debug, Default)]
pub struct EventAccumulator {
    pending: BTreeMap<u32, PendingCall>,
    called: bool,
    stop_reason: Option<String>,
    usage: Usage,
    finished: bool,
}

impl EventAccumulator {
    /// `Err` for a failure the provider reported inside a 200.
    pub fn push(&mut self, event: &Value) -> Result<Vec<ModelEvent>, ModelError> {
        let mut out = Vec::new();
        let kind = event["type"].as_str().unwrap_or_default();
        let index = event["output_index"].as_u64().unwrap_or(0) as u32;
        match kind {
            "response.output_text.delta" => {
                if let Some(text) = non_empty(&event["delta"]) {
                    out.push(ModelEvent::TextDelta { text });
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(text) = non_empty(&event["delta"]) {
                    out.push(ModelEvent::ThinkingDelta { text });
                }
            }
            "response.output_item.added" if event["item"]["type"] == "function_call" => {
                let item = &event["item"];
                let call = PendingCall {
                    call_id: item["call_id"].as_str().unwrap_or_default().to_string(),
                    name: item["name"].as_str().unwrap_or_default().to_string(),
                    arguments: String::new(),
                };
                out.push(ModelEvent::ToolUseStart {
                    id: call.call_id.clone(),
                    name: call.name.clone(),
                });
                self.pending.insert(index, call);
            }
            "response.function_call_arguments.delta" => {
                if let (Some(call), Some(delta)) =
                    (self.pending.get_mut(&index), non_empty(&event["delta"]))
                {
                    call.arguments.push_str(&delta);
                    out.push(ModelEvent::ToolArgsDelta {
                        id: call.call_id.clone(),
                        partial_json: delta,
                    });
                }
            }
            "response.output_item.done" if event["item"]["type"] == "function_call" => {
                if let Some(mut call) = self.pending.remove(&index) {
                    if let Some(whole) = event["item"]["arguments"].as_str() {
                        call.arguments = whole.to_string();
                    }
                    self.called = true;
                    out.push(crate::tool_use_from_arguments(
                        call.call_id,
                        call.name,
                        call.arguments,
                    ));
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = &event["response"];
                self.usage = usage_of(&response["usage"]);
                self.stop_reason = Some(match response["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => "max_tokens".to_string(),
                    Some(other) => other.to_string(),
                    None if self.called => "tool_use".to_string(),
                    None => "end_turn".to_string(),
                });
            }
            "response.failed" => {
                return Err(chunk_error(&event["response"]["error"]));
            }
            "error" => return Err(chunk_error(event)),
            _ => {}
        }
        Ok(out)
    }

    pub fn saw_terminal(&self) -> bool {
        self.stop_reason.is_some()
    }

    pub fn finish(&mut self) -> Vec<ModelEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out: Vec<ModelEvent> = std::mem::take(&mut self.pending)
            .into_values()
            .map(|c| ModelEvent::MalformedToolUse {
                id: c.call_id,
                name: c.name,
                raw_input: c.arguments,
            })
            .collect();
        out.push(ModelEvent::EndTurn {
            stop_reason: self
                .stop_reason
                .clone()
                .unwrap_or_else(|| "end_turn".into()),
            usage: self.usage,
        });
        out
    }
}

fn non_empty(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// `input_tokens` counts the cached part; `Usage` keeps the two apart.
fn usage_of(usage: &Value) -> Usage {
    let cached = usage["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0) as u32;
    Usage {
        input_tokens: (usage["input_tokens"].as_u64().unwrap_or(0) as u32).saturating_sub(cached),
        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0) as u32,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
    }
}

/// SSE bytes → [`ModelEvent`] stream, ending at the terminal `response.*`
/// event; a stream that closes before one is reported as truncated.
pub fn events_from_bytes<S>(
    byte_stream: S,
    idle_timeout: Duration,
) -> impl Stream<Item = Result<ModelEvent, ModelError>> + Send
where
    S: Stream<Item = Result<Bytes, ModelError>> + Send + 'static,
{
    async_stream::stream! {
        let mut events = Box::pin(byte_stream.eventsource());
        let mut acc = EventAccumulator::default();

        loop {
            let next = match tokio::time::timeout(idle_timeout, events.next()).await {
                Ok(next) => next,
                Err(_elapsed) => {
                    yield Err(ModelError::timeout(format!(
                        "stream idle for {}s",
                        idle_timeout.as_secs()
                    )));
                    return;
                }
            };
            let Some(item) = next else { break };
            let event = match item {
                Ok(event) => event,
                Err(e) => {
                    yield Err(ModelError::transport(e.to_string()));
                    return;
                }
            };
            let data = event.data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let value = match serde_json::from_str::<Value>(data) {
                Ok(value) => value,
                Err(e) => {
                    tracing::warn!(error = %e, raw = %data, "skipping unparseable responses event");
                    continue;
                }
            };
            match acc.push(&value) {
                Ok(events) => {
                    for e in events {
                        yield Ok(e);
                    }
                }
                Err(e) => {
                    yield Err(e);
                    return;
                }
            }
            if acc.saw_terminal() {
                break;
            }
        }

        if !acc.saw_terminal() {
            tracing::warn!("responses stream closed before response.completed");
            yield Ok(ModelEvent::Truncated);
        }
        for e in acc.finish() {
            yield Ok(e);
        }
    }
}

/// An OpenAI-Responses-speaking [`Model`].
#[derive(Clone)]
pub struct OpenAIResponsesModel {
    inner: Arc<Inner>,
}

impl OpenAIResponsesModel {
    pub fn new(
        base_url: &str,
        api_key: impl Into<String>,
        model_default: impl Into<String>,
    ) -> Result<Self, ModelError> {
        Ok(Self {
            inner: Arc::new(Inner {
                net: Arc::new(LocalNetwork::default()),
                endpoint: responses_url(base_url)?,
                api_key: api_key.into(),
                default_model: model_default.into(),
                backoff: Arc::new(LadderBackoff::new()),
                provenance: None,
                reasoning_passback: false,
                thinking_toggle: None,
            }),
        })
    }

    pub fn with_backoff(self, backoff_ms: Vec<u64>) -> Self {
        self.with_backoff_policy(Arc::new(LadderBackoff::from_millis(backoff_ms)))
    }

    pub fn with_backoff_policy(mut self, backoff: Arc<dyn BackoffPolicy>) -> Self {
        Arc::make_mut(&mut self.inner).backoff = backoff;
        self
    }

    pub fn with_provenance(mut self, provider_id: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.inner).provenance = Some(provider_id.into());
        self
    }

    pub fn with_network(mut self, net: Arc<dyn Network>) -> Self {
        Arc::make_mut(&mut self.inner).net = net;
        self
    }

    pub fn endpoint(&self) -> &Url {
        &self.inner.endpoint
    }
}

/// Same spellings of a base URL as [`super::chat_completions_url`] accepts.
fn responses_url(base_url: &str) -> Result<Url, ModelError> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(ModelError::invalid_request("base_url is empty"));
    }
    let full = if trimmed.ends_with("/responses") {
        trimmed.to_string()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}/responses")
    } else {
        format!("{trimmed}/v1/responses")
    };
    Url::parse(&full)
        .map_err(|e| ModelError::invalid_request(format!("invalid base_url '{base_url}': {e}")))
}

#[async_trait]
impl Model for OpenAIResponsesModel {
    fn api_type(&self) -> ApiType {
        ApiType::OpenAIResponses
    }

    fn provenance(&self) -> Option<&str> {
        self.inner.provenance.as_deref()
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        if params.provider_search.is_some() {
            return Err(ModelError::unsupported(
                "this provider has no web search of its own; route `task_models.web_search` \
                 to one that has",
            ));
        }
        let model = if params.model.is_empty() {
            self.inner.default_model.clone()
        } else {
            params.model
        };
        let req = build_request(
            model,
            prompt_blocks,
            tools,
            messages,
            Some(params.max_tokens),
        );

        let resp = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ModelError::cancelled()),
            resp = send_with_retry(&self.inner, &req) => resp?,
        };
        let bytes = resp
            .body
            .map(|c| c.map(bytes::Bytes::from).map_err(exec_error));
        let events = events_from_bytes(bytes, STREAM_IDLE_TIMEOUT);
        Ok(Box::new(Box::pin(crate::cancel::until_cancelled(
            events, cancel,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: MessageRole, content: Vec<ModelContentBlock>) -> ModelMessage {
        ModelMessage { role, content }
    }

    fn text(s: &str) -> ModelContentBlock {
        ModelContentBlock::Text { text: s.into() }
    }

    #[test]
    fn a_conversation_with_a_tool_round_becomes_typed_items() {
        let req = build_request(
            "gpt-x".into(),
            vec![PromptBlock::system("you are helpful")],
            vec![ToolDef::new("Read", "reads", json!({"type": "object"}))],
            vec![
                msg(MessageRole::User, vec![text("hi")]),
                msg(
                    MessageRole::Assistant,
                    vec![
                        text("reading"),
                        ModelContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "Read".into(),
                            input: json!({"file_path": "a"}),
                        },
                    ],
                ),
                msg(
                    MessageRole::User,
                    vec![
                        text("and hurry"),
                        ModelContentBlock::ToolResult {
                            tool_use_id: "call_1".into(),
                            content: "nope".into(),
                            is_error: Some(true),
                        },
                    ],
                ),
            ],
            Some(100),
        );
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({
                "model": "gpt-x",
                "instructions": "you are helpful",
                "input": [
                    {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                    {"role": "assistant", "content": [{"type": "output_text", "text": "reading"}]},
                    {"type": "function_call", "call_id": "call_1", "name": "Read",
                     "arguments": "{\"file_path\":\"a\"}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": "Error: nope"},
                    {"role": "user", "content": [{"type": "input_text", "text": "and hurry"}]},
                ],
                "tools": [{"type": "function", "name": "Read", "description": "reads",
                           "parameters": {"type": "object"}, "strict": false}],
                "stream": true,
                "store": false,
                "max_output_tokens": 100,
            })
        );
    }

    #[test]
    fn images_and_pdfs_are_data_uris_in_input_parts() {
        let req = build_request(
            "m".into(),
            vec![],
            vec![],
            vec![msg(
                MessageRole::User,
                vec![
                    ModelContentBlock::Image {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                    ModelContentBlock::Document {
                        media_type: "application/pdf".into(),
                        data: "JVBERg==".into(),
                        title: Some("spec.pdf".into()),
                    },
                ],
            )],
            None,
        );
        let v = serde_json::to_value(&req).unwrap();
        let parts = &v["input"][0]["content"];
        assert_eq!(parts[0]["image_url"], "data:image/png;base64,AAAA");
        assert_eq!(parts[1]["type"], "input_file");
        assert_eq!(parts[1]["filename"], "spec.pdf");
        assert_eq!(
            parts[1]["file_data"],
            "data:application/pdf;base64,JVBERg=="
        );
        assert!(v.get("instructions").is_none());
    }

    fn run(events: &[Value]) -> Vec<ModelEvent> {
        let mut acc = EventAccumulator::default();
        let mut out = Vec::new();
        for e in events {
            out.extend(acc.push(e).expect("no failure in the fixture"));
        }
        out.extend(acc.finish());
        out
    }

    #[test]
    fn a_text_reply_streams_and_reports_usage_with_the_cached_part_apart() {
        let events = run(&[
            json!({"type": "response.created", "response": {}}),
            json!({"type": "response.output_text.delta", "output_index": 0, "delta": "Hel"}),
            json!({"type": "response.output_text.delta", "output_index": 0, "delta": "lo"}),
            json!({"type": "response.completed", "response": {
                "usage": {"input_tokens": 100, "output_tokens": 7,
                          "input_tokens_details": {"cached_tokens": 40}}}}),
        ]);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        let Some(ModelEvent::EndTurn { stop_reason, usage }) = events.last() else {
            panic!("{events:?}");
        };
        assert_eq!(stop_reason, "end_turn");
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens
            ),
            (60, 7, 40)
        );
    }

    #[test]
    fn a_function_call_streams_its_arguments_and_ends_the_turn_as_tool_use() {
        let events = run(&[
            json!({"type": "response.output_item.added", "output_index": 1,
                   "item": {"type": "function_call", "call_id": "call_9", "name": "Read",
                            "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 1,
                   "delta": "{\"file_"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 1,
                   "delta": "path\":\"a\"}"}),
            json!({"type": "response.output_item.done", "output_index": 1,
                   "item": {"type": "function_call", "call_id": "call_9", "name": "Read",
                            "arguments": "{\"file_path\":\"a\"}"}}),
            json!({"type": "response.completed", "response": {"usage": {}}}),
        ]);
        assert!(matches!(&events[0], ModelEvent::ToolUseStart { id, name }
            if id == "call_9" && name == "Read"));
        assert!(events
            .iter()
            .any(|e| matches!(e, ModelEvent::ToolUse { id, input, .. }
            if id == "call_9" && input == &json!({"file_path": "a"}))));
        assert!(
            matches!(events.last(), Some(ModelEvent::EndTurn { stop_reason, .. })
            if stop_reason == "tool_use")
        );
    }

    #[test]
    fn an_incomplete_response_maps_its_reason_and_a_failed_one_is_an_error() {
        let events = run(&[json!({"type": "response.incomplete", "response": {
            "incomplete_details": {"reason": "max_output_tokens"}, "usage": {}}})]);
        assert!(
            matches!(events.last(), Some(ModelEvent::EndTurn { stop_reason, .. })
            if stop_reason == "max_tokens")
        );

        let mut acc = EventAccumulator::default();
        let err = acc
            .push(&json!({"type": "response.failed", "response": {"error": {
                "code": "rate_limit_exceeded", "message": "slow down"}}}))
            .unwrap_err();
        assert_eq!(
            err.kind,
            base::interface::model::ModelErrorKind::RateLimited
        );
    }

    #[tokio::test]
    async fn a_stream_that_ends_before_completed_is_truncated() {
        let sse = "event: response.output_text.delta\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"half\"}\n\n";
        let bytes = futures::stream::iter(vec![Ok(Bytes::from(sse))]);
        let events: Vec<_> = events_from_bytes(bytes, Duration::from_secs(5))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert!(matches!(events[0], ModelEvent::TextDelta { .. }));
        assert!(matches!(events[1], ModelEvent::Truncated));
        assert!(matches!(events[2], ModelEvent::EndTurn { .. }));
    }

    #[test]
    fn base_url_spellings_land_on_the_responses_endpoint() {
        for base in [
            "https://api.openai.com",
            "https://api.openai.com/v1/",
            "https://api.openai.com/v1/responses",
        ] {
            assert_eq!(
                responses_url(base).unwrap().as_str(),
                "https://api.openai.com/v1/responses"
            );
        }
        assert!(responses_url(" ").is_err());
    }
}
