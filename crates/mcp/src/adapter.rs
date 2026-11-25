//! `McpToolAdapter` —— 把一条 MCP server 提供的工具包装成 agix `Tool`。
//!
//! 工具名按 `mcp__<server>__<tool>` 命名，与权限规则匹配语义一致
//! （见 agix-permissions::ruleset）；两段怎么从 server 名与工具名得来见 [`crate::naming`]。
//! 调用中 server 向用户提的问题经 [`ToolCallElicitor`] 走这次调用的 `ToolContext` 去问宿主。

use crate::client::{McpCallResult, McpClientHandle, McpContent, McpToolHints, McpToolMeta};
use crate::error::McpError;
use crate::output_cache::McpOutputCache;
use async_trait::async_trait;
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::{
    error::ToolError,
    tool::ProgressSender,
    tool::Tool,
    tool::ToolOrigin,
    tool::ToolResult,
    tool::ToolResultBlock,
    tool::ToolSpec,
    tool::ValidationResult,
    tool::{PromptContext, ToolContext},
};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Per-request cap on a server-supplied description. Some servers put pages of
/// documentation there; the full text stays reachable through `prompt()`, which
/// `ToolSearch` serves as the tool's guide whenever the two differ.
pub(crate) const MAX_ADVERTISED_DESCRIPTION_CHARS: usize = 2048;

#[derive(Clone)]
pub struct McpToolAdapter {
    full_name: String,
    tool_name: String,
    server_name: String,
    full_description: String,
    description: String,
    input_schema: Value,
    output_schema: Option<Value>,
    hints: McpToolHints,
    client: McpClientHandle,
    /// Shared output cache across all MCP adapters.
    cache: Arc<Mutex<McpOutputCache>>,
    record: Option<McpCallRecord>,
}

/// What came of the one call an adapter from [`McpToolAdapter::recording`]
/// was handed, as the server answered it: empty until the call reached the
/// server, so a call the pipeline refused leaves nothing here.
#[derive(Clone, Default)]
pub struct McpCallRecord(Arc<Recorded>);

#[derive(Default)]
struct Recorded {
    outcome: Mutex<Option<McpCallOutcome>>,
    sent: std::sync::atomic::AtomicBool,
}

impl McpCallRecord {
    pub fn take(&self) -> Option<McpCallOutcome> {
        self.0
            .outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Whether the request was handed to the server, answered or not: from
    /// then on the server may have acted on it.
    pub fn reached_server(&self) -> bool {
        self.0.sent.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn put(&self, outcome: McpCallOutcome) {
        *self.0.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
    }

    fn sending(&self) {
        self.0.sent.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// One call's end as the server's side of it went.
#[derive(Debug, Clone)]
pub enum McpCallOutcome {
    /// The server answered with a tool result, `isError` or not.
    Answered(McpCallResult),
    /// The server did not answer within its tool timeout.
    TimedOut(String),
    /// The call was stopped before the server answered.
    Cancelled,
    /// The request went out and did not come back as a tool result.
    Failed(String),
}

impl McpToolAdapter {
    /// Construct a new instance.
    pub fn new(client: McpClientHandle, meta: McpToolMeta) -> Self {
        Self::with_cache(client, meta, Arc::new(Mutex::new(McpOutputCache::new())))
    }

    /// Construct with a shared output cache (all adapters from the same manager
    /// should share the same cache instance).
    pub fn with_cache(
        client: McpClientHandle,
        meta: McpToolMeta,
        cache: Arc<Mutex<McpOutputCache>>,
    ) -> Self {
        let server_name = client.server_name().to_string();
        let tool_name = meta.name.clone();
        let full_name = crate::naming::tool_name(&server_name, &tool_name);
        let full_description = meta
            .description
            .unwrap_or_else(|| format!("MCP tool: {tool_name}"));
        let description =
            base::text::truncate_chars(&full_description, MAX_ADVERTISED_DESCRIPTION_CHARS, "…");
        Self {
            full_name,
            tool_name,
            server_name,
            full_description,
            description,
            input_schema: object_schema(meta.input_schema),
            output_schema: meta.output_schema,
            hints: meta.hints,
            client,
            cache,
            record: None,
        }
    }

    /// What a server's tools are offered as: one adapter per tool, named among
    /// the server's others by [`crate::naming::tool_names`]. A name the server
    /// lists twice is offered once, as its first listing.
    pub fn for_server(
        client: &McpClientHandle,
        tools: Vec<McpToolMeta>,
        cache: &Arc<Mutex<McpOutputCache>>,
    ) -> Vec<Arc<Self>> {
        let mut seen = std::collections::HashSet::new();
        let tools: Vec<_> = tools
            .into_iter()
            .filter(|t| seen.insert(t.name.clone()))
            .collect();
        let names =
            crate::naming::tool_names(client.server_name(), tools.iter().map(|t| t.name.as_str()));
        tools
            .into_iter()
            .zip(names)
            .map(|(meta, name)| {
                Arc::new(Self::with_cache(client.clone(), meta, cache.clone()).named(name))
            })
            .collect()
    }

    /// This tool as a host calls it, keeping what its one call comes to in
    /// the record handed back beside it. It always asks the server: the
    /// shared cache answers the model's repeated reads, not a host polling a
    /// state.
    pub fn recording(&self) -> (Self, McpCallRecord) {
        let record = McpCallRecord::default();
        (self.recording_into(record.clone()), record)
    }

    /// [`Self::recording`] into a record the caller already holds.
    pub fn recording_into(&self, record: McpCallRecord) -> Self {
        Self {
            record: Some(record),
            ..self.clone()
        }
    }

    /// Go by `full_name` instead — what [`crate::naming::tool_names`] gave
    /// this tool among its server's others.
    pub fn named(mut self, full_name: String) -> Self {
        self.full_name = full_name;
        self
    }

    /// Server-qualified tool name, e.g. `mcp__{server}__{tool}`.
    pub fn full_name(&self) -> &str {
        &self.full_name
    }
    /// Server name this tool belongs to.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
    /// Tool name (without server prefix).
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// The server's description of the tool, whole.
    pub fn full_description(&self) -> &str {
        &self.full_description
    }

    pub fn input_schema_value(&self) -> &Value {
        &self.input_schema
    }

    pub fn output_schema(&self) -> Option<&Value> {
        self.output_schema.as_ref()
    }

    pub fn hints(&self) -> McpToolHints {
        self.hints
    }

    /// A repeated identical call may be answered from the cache only when the
    /// server declares the tool both read-only and idempotent.
    fn is_cacheable(&self) -> bool {
        self.hints.read_only == Some(true) && self.hints.idempotent == Some(true)
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn spec(&self) -> ToolSpec {
        // Read-only only on the server's `readOnlyHint`; a server that says
        // nothing is assumed to write.
        ToolSpec::new()
            .callable_from_code(true)
            .origin(ToolOrigin::mcp(&self.server_name))
            .read_only(self.hints.read_only == Some(true))
            .destructive(self.hints.destructive == Some(true))
    }
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        self.input_schema.clone()
    }

    /// The server's whole description, when the advertised one had to be cut.
    async fn guide(&self, _: &PromptContext) -> Option<String> {
        let full = self.full_description.trim();
        (!full.is_empty() && full != self.description.trim()).then(|| self.full_description.clone())
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        // 接受任意 object 输入 —— 实际 schema 校验由 server 端做
        if input.is_object() {
            ValidationResult::Ok
        } else {
            ValidationResult::err("MCP tool input must be a JSON object", 1)
        }
    }

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::Server)
            .with_question(format!("Allow MCP tool {} ?", self.full_name))
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let args = input.as_object().cloned().unwrap_or_default();
        let cacheable = self.is_cacheable();

        if cacheable && self.record.is_none() {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = cache.get(&self.server_name, &self.tool_name, &args) {
                return Ok(into_tool_result(cached));
            }
        }

        let listed = Arc::<std::sync::OnceLock<Arc<base::context::RunningTask>>>::default();
        let scope = crate::elicit::McpCallScope {
            elicitor: Some(Arc::new(ToolCallElicitor {
                ctx: ctx.clone(),
                listed: listed.clone(),
            })),
            tasks: Some(crate::tasks::McpTaskHost {
                session: ctx.session.clone(),
                tool: self.full_name.clone(),
                tool_use_id: ctx.tool_use_id.clone(),
                cancel: ctx.cancel.clone(),
                listed,
            }),
            ..Default::default()
        };
        let send = async {
            if let Some(record) = &self.record {
                record.sending();
            }
            self.client
                .call_tool(&self.tool_name, args.clone(), &scope)
                .await
        };
        let result = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                if let Some(record) = &self.record {
                    record.put(McpCallOutcome::Cancelled);
                }
                return Err(ToolError::Cancelled);
            }
            r = send => r,
        };
        self.keep(&result);

        let result = result.map_err(|e: McpError| match e {
            McpError::TaskStopped { .. } => ToolError::Cancelled,
            e => ToolError::Execution(anyhow::Error::new(e)),
        })?;

        if cacheable {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.put(&self.server_name, &self.tool_name, &args, result.clone());
        }

        Ok(into_tool_result(result))
    }
}

impl McpToolAdapter {
    fn keep(&self, result: &Result<McpCallResult, McpError>) {
        let Some(record) = &self.record else {
            return;
        };
        record.put(match result {
            Ok(answer) => McpCallOutcome::Answered(answer.clone()),
            Err(e @ McpError::Timeout { .. }) => McpCallOutcome::TimedOut(e.to_string()),
            Err(McpError::TaskStopped { .. }) => McpCallOutcome::Cancelled,
            Err(e) => McpCallOutcome::Failed(e.to_string()),
        });
    }
}

/// Puts what a server asks during one call to whoever the call's session
/// asks, for as long as the session waits on a permission prompt. Nobody to
/// ask, no answer in time, or an answer that does not read is `cancel`.
pub struct ToolCallElicitor {
    pub ctx: ToolContext,
    /// The task the call was turned into, once it is listed: it is marked
    /// waiting while its question is open.
    pub listed: Arc<std::sync::OnceLock<Arc<base::context::RunningTask>>>,
}

#[async_trait]
impl crate::elicit::McpElicitor for ToolCallElicitor {
    async fn elicit(
        &self,
        server: &str,
        message: String,
        mode: base::interface::elicitation::McpElicitMode,
        limited: bool,
    ) -> base::interface::elicitation::McpElicitationAnswer {
        use base::interface::elicitation::{ElicitKind, ElicitRequest, McpElicitationAnswer};
        let ctx = &self.ctx;
        let asker = match (&ctx.elicitation, &ctx.attendance) {
            (Some(asker), base::interface::permission::Attendance::Attended) => asker,
            _ => {
                tracing::debug!(server, "nobody to ask an MCP server's question; cancelled");
                return McpElicitationAnswer::Cancel;
            }
        };
        let secs = ctx.config.permission_prompt_timeout_secs;
        let request = ElicitRequest {
            agent_session_id: None,
            id: uuid::Uuid::new_v4().to_string(),
            kind: ElicitKind::McpServer {
                server: server.to_string(),
                mode,
                tool_use_id: Some(ctx.tool_use_id.clone()).filter(|id| !id.is_empty()),
                limited,
            },
            message,
            options: Vec::new(),
        };
        let prompt_id = request.id.clone();
        if let Some(task) = self.listed.get() {
            task.begin_waiting(&prompt_id);
        }
        let asked = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => None,
            asked = asker.ask_within(request, (secs > 0).then(|| Duration::from_secs(secs))) => asked,
        };
        if let Some(task) = self.listed.get() {
            task.end_waiting(&prompt_id);
        }
        match asked {
            Some(outcome) => outcome.answer_as().unwrap_or_else(|| {
                tracing::debug!(server, reason = ?outcome.decline_reason(), "MCP server's question not answered");
                McpElicitationAnswer::Cancel
            }),
            None => McpElicitationAnswer::Cancel,
        }
    }
}

/// A tool's input schema as a model API takes it: an object schema at the
/// top. A server that sends none, or one without `type`, still gets its tool
/// offered rather than the whole request refused.
fn object_schema(schema: Value) -> Value {
    match schema {
        Value::Object(mut map) => {
            if map.get("type").and_then(Value::as_str) != Some("object") {
                map.insert("type".into(), Value::String("object".into()));
            }
            Value::Object(map)
        }
        _ => serde_json::json!({ "type": "object" }),
    }
}

/// 把 MCP `CallToolResult` 转成我们的 `ToolResult`。
///
/// 单个 **文本** 内容块 → `Text`；其它一律 → `Blocks` 逐块保留。
/// `is_error` 一并传播。
///
/// Only a lone text block takes the `Text` shortcut; any result containing an
/// image goes through `Blocks` so the runtime can lift it into a real
/// `ModelContentBlock::Image` — a single-image result is the *common* shape
/// for screenshot/diagram MCP servers.
fn into_tool_result(result: McpCallResult) -> ToolResult {
    let structured = result.structured.clone();
    let r = content_into_tool_result(result);
    match structured {
        Some(v) => r.with_structured(v),
        None => r,
    }
}

fn content_into_tool_result(result: McpCallResult) -> ToolResult {
    let is_error = result.is_error;
    let has_image = result
        .content
        .iter()
        .any(|b| matches!(b, McpContent::Image { .. }));
    if !has_image && result.content.len() <= 1 {
        let text = result
            .content
            .iter()
            .map(|b| match b {
                McpContent::Text(t) => t.clone(),
                McpContent::Image { .. } => "[image]".to_string(),
                McpContent::Other(v) => format!("[mcp content: {v}]"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = if text.is_empty() {
            "(empty response)".to_string()
        } else {
            text
        };
        let mut r = ToolResult::text(text);
        r.is_error = is_error;
        return r;
    }
    let blocks = into_tool_result_blocks(result.content);
    ToolResult::blocks(blocks).with_error(is_error)
}

const MAX_IMAGE_BYTES: usize = base::tool::MAX_IMAGE_BYTES as usize;

fn into_tool_result_blocks(blocks: Vec<McpContent>) -> Vec<ToolResultBlock> {
    blocks
        .into_iter()
        .map(|b| match b {
            McpContent::Text(t) => ToolResultBlock::text(t),
            McpContent::Image { data, media_type } if data.len() / 4 * 3 > MAX_IMAGE_BYTES => {
                ToolResultBlock::text(format!(
                    "[image omitted: {media_type}, about {} bytes, over the {MAX_IMAGE_BYTES}-byte \
                     limit for one image]",
                    data.len() / 4 * 3
                ))
            }
            McpContent::Image { data, media_type } => ToolResultBlock::image(media_type, data),
            McpContent::Other(v) => ToolResultBlock::text(format!("[mcp content: {v}]")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MockMcpClient;
    use base::tool::ToolResultContent;
    use serde_json::json;
    use std::sync::Arc;

    fn meta(name: &str) -> McpToolMeta {
        McpToolMeta {
            name: name.into(),
            description: Some(format!("desc for {name}")),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            hints: Default::default(),
        }
    }

    fn ctx() -> ToolContext {
        session_state::testing::tool_context_for_test(
            platform::local_providers(),
            std::path::PathBuf::from("/tmp"),
        )
    }

    #[tokio::test]
    async fn name_is_namespaced_with_server() {
        let client = Arc::new(MockMcpClient::new("github", vec![]));
        let adapter = McpToolAdapter::new(client, meta("create_issue"));
        assert_eq!(adapter.name(), "mcp__github__create_issue");
        assert_eq!(adapter.server_name(), "github");
        assert_eq!(adapter.tool_name(), "create_issue");
    }

    /// The `mcp__` prefix already implies the answer, but a naming convention
    /// is not a declaration — it holds only until someone registers a builtin
    /// whose name starts that way. The adapter says which server it fronts.
    #[tokio::test]
    async fn the_server_description_is_what_the_model_is_shown() {
        let client = Arc::new(MockMcpClient::new("github", vec![]));
        let adapter = McpToolAdapter::new(client, meta("create_issue"));
        assert_eq!(adapter.description(), "desc for create_issue");
        assert!(adapter.guide(&PromptContext::default()).await.is_none());
    }

    #[tokio::test]
    async fn an_oversized_description_is_cut_and_the_whole_text_becomes_the_guide() {
        let client = Arc::new(MockMcpClient::new("docs", vec![]));
        let mut m = meta("search");
        let long = "x".repeat(MAX_ADVERTISED_DESCRIPTION_CHARS + 10);
        m.description = Some(long.clone());
        let adapter = McpToolAdapter::new(client, m);
        assert_eq!(
            adapter.description().chars().count(),
            MAX_ADVERTISED_DESCRIPTION_CHARS + 1
        );
        assert_eq!(adapter.guide(&PromptContext::default()).await, Some(long));
    }

    #[tokio::test]
    async fn source_names_the_server_it_fronts() {
        let client = Arc::new(MockMcpClient::new("github", vec![]));
        let adapter = McpToolAdapter::new(client, meta("create_issue"));
        assert_eq!(adapter.spec().origin, ToolOrigin::mcp("github"));
    }

    #[tokio::test]
    async fn input_schema_passes_through() {
        let client = Arc::new(MockMcpClient::new("github", vec![]));
        let mut m = meta("x");
        m.input_schema = json!({"type": "object", "properties": {"q": {"type": "string"}}});
        let adapter = McpToolAdapter::new(client, m);
        let s = adapter.input_schema(base::tool::PathStyle::Absolute);
        assert_eq!(s["properties"]["q"]["type"], "string");
    }

    #[tokio::test]
    async fn a_schema_without_an_object_type_is_offered_as_an_object() {
        let client: McpClientHandle = Arc::new(MockMcpClient::new("github", vec![]));
        for (sent, properties) in [
            (json!({"properties": {"q": {}}}), json!({"q": {}})),
            (json!(null), Value::Null),
            (json!(true), Value::Null),
        ] {
            let mut m = meta("x");
            m.input_schema = sent;
            let s = McpToolAdapter::new(client.clone(), m)
                .input_schema(base::tool::PathStyle::Absolute);
            assert_eq!(s["type"], "object", "{s}");
            assert_eq!(s["properties"], properties, "{s}");
        }
    }

    #[tokio::test]
    async fn an_odd_server_name_still_makes_a_tool_name_the_api_takes() {
        let client = Arc::new(MockMcpClient::new("my server.v2", vec![]));
        let adapter = McpToolAdapter::new(client, meta("create.issue"));
        assert_eq!(
            adapter.name(),
            crate::naming::tool_name("my server.v2", "create.issue")
        );
        assert!(adapter.name().starts_with("mcp__my_server_v2_"));
        assert!(adapter.name().ends_with("__create_issue"));
        assert_eq!(adapter.server_name(), "my server.v2");
        assert_eq!(adapter.tool_name(), "create.issue");
    }

    #[tokio::test]
    async fn calls_underlying_client_and_converts_text_result() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("echo")]));
        client.push_response(
            "echo",
            McpCallResult {
                content: vec![McpContent::Text("hi".into())],
                is_error: false,
                meta: None,
                structured: None,
                raw_content: Vec::new(),
            },
        );
        let adapter = McpToolAdapter::new(client.clone(), meta("echo"));
        let r = adapter
            .call(json!({"text": "hi"}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(!r.is_error);
        match r.content {
            ToolResultContent::Text(s) => assert_eq!(s, "hi"),
            _ => panic!("expected Text"),
        }
        let calls = client.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "echo");
        assert_eq!(calls[0].1.get("text"), Some(&json!("hi")));
    }

    #[tokio::test]
    async fn structured_content_rides_beside_the_text() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("put")]));
        let mut answer = text("{\"id\":1}", false);
        answer.structured = Some(json!({"id": 1}));
        client.push_response("put", answer);
        let adapter = McpToolAdapter::new(client, meta("put"));
        let r = adapter
            .call(json!({}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert_eq!(r.structured_content, Some(json!({"id": 1})));
        assert!(matches!(r.content, ToolResultContent::Text(ref s) if s == "{\"id\":1}"));
    }

    #[tokio::test]
    async fn is_error_propagates() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("fail")]));
        client.push_response(
            "fail",
            McpCallResult {
                content: vec![McpContent::Text("oops".into())],
                is_error: true,
                meta: None,
                structured: None,
                raw_content: Vec::new(),
            },
        );
        let adapter = McpToolAdapter::new(client, meta("fail"));
        let r = adapter
            .call(json!({}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(r.is_error);
    }

    fn text(t: &str, is_error: bool) -> McpCallResult {
        McpCallResult {
            content: vec![McpContent::Text(t.into())],
            is_error,
            meta: None,
            structured: None,
            raw_content: Vec::new(),
        }
    }

    async fn call_twice(hints: McpToolHints, first: McpCallResult) -> (Vec<ToolResult>, usize) {
        let mut m = meta("state");
        m.hints = hints;
        let client = Arc::new(MockMcpClient::new("s", vec![m.clone()]));
        client.push_response("state", first);
        client.push_response("state", text("second", false));
        let adapter = McpToolAdapter::new(client.clone(), m);
        let mut results = Vec::new();
        for _ in 0..2 {
            results.push(
                adapter
                    .call(json!({"id": 1}), ctx(), ProgressSender::noop("t"))
                    .await
                    .unwrap(),
            );
        }
        let calls = client.calls.lock().unwrap().len();
        (results, calls)
    }

    #[tokio::test]
    async fn a_tool_without_hints_is_called_every_time() {
        let (results, calls) = call_twice(McpToolHints::default(), text("first", false)).await;
        assert_eq!(calls, 2, "a state-reading tool must not get a stale answer");
        assert!(matches!(&results[1].content, ToolResultContent::Text(s) if s == "second"));
    }

    #[tokio::test]
    async fn a_read_only_idempotent_tool_is_served_from_the_cache() {
        let hints = McpToolHints {
            read_only: Some(true),
            idempotent: Some(true),
            ..Default::default()
        };
        let (results, calls) = call_twice(hints, text("first", false)).await;
        assert_eq!(calls, 1);
        assert!(matches!(&results[1].content, ToolResultContent::Text(s) if s == "first"));
    }

    #[tokio::test]
    async fn an_error_result_is_not_replayed_from_the_cache() {
        let hints = McpToolHints {
            read_only: Some(true),
            idempotent: Some(true),
            ..Default::default()
        };
        let (results, calls) = call_twice(hints, text("failed", true)).await;
        assert_eq!(calls, 2);
        assert!(!results[1].is_error);
    }

    #[tokio::test]
    async fn read_only_comes_from_the_servers_hint() {
        let client = Arc::new(MockMcpClient::new("s", vec![]));
        assert!(
            !McpToolAdapter::new(client.clone(), meta("x"))
                .call_traits(&json!({}))
                .read_only
        );
        let mut m = meta("x");
        m.hints.read_only = Some(true);
        assert!(
            McpToolAdapter::new(client, m)
                .call_traits(&json!({}))
                .read_only
        );
    }

    #[tokio::test]
    async fn multi_block_result_keeps_blocks() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("multi")]));
        client.push_response(
            "multi",
            McpCallResult {
                content: vec![
                    McpContent::Text("part1".into()),
                    McpContent::Text("part2".into()),
                ],
                is_error: false,
                meta: None,
                structured: None,
                raw_content: Vec::new(),
            },
        );
        let adapter = McpToolAdapter::new(client, meta("multi"));
        let r = adapter
            .call(json!({}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        match r.content {
            ToolResultContent::Blocks(b) => assert_eq!(b.len(), 2),
            _ => panic!("expected Blocks"),
        }
    }

    #[tokio::test]
    async fn image_content_converts_to_image_block() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("img")]));
        client.push_response(
            "img",
            McpCallResult {
                content: vec![McpContent::Image {
                    data: "base64data".into(),
                    media_type: "image/png".into(),
                }],
                is_error: false,
                meta: None,
                structured: None,
                raw_content: Vec::new(),
            },
        );
        let adapter = McpToolAdapter::new(client, meta("img"));
        let r = adapter
            .call(json!({}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        // A lone image block must survive as a real image block.
        match r.content {
            ToolResultContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                let (media_type, data) = blocks[0].as_image().expect("expected an image block");
                assert_eq!(media_type, "image/png");
                assert_eq!(data, "base64data");
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    /// An image the API would refuse is not passed on: it would sink this
    /// request and every one after it that still carries it.
    #[tokio::test]
    async fn an_image_over_the_api_limit_is_replaced_by_a_note() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("shot")]));
        client.push_response(
            "shot",
            McpCallResult {
                content: vec![
                    McpContent::Image {
                        data: "A".repeat(6_000_000),
                        media_type: "image/png".into(),
                    },
                    McpContent::Image {
                        data: "A".repeat(4_000_000),
                        media_type: "image/png".into(),
                    },
                ],
                is_error: false,
                meta: None,
                structured: None,
                raw_content: Vec::new(),
            },
        );
        let adapter = McpToolAdapter::new(client, meta("shot"));
        let r = adapter
            .call(json!({}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        let ToolResultContent::Blocks(blocks) = r.content else {
            panic!("expected Blocks");
        };
        assert!(blocks[0].as_image().is_none());
        assert!(
            blocks[0]
                .text
                .as_deref()
                .is_some_and(|t| t.starts_with("[image omitted: image/png, about 4500000 bytes")),
            "{:?}",
            blocks[0].text
        );
        assert!(blocks[1].as_image().is_some(), "one under the limit stays");
    }

    #[tokio::test]
    async fn cancel_aborts() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("hang")]));
        // mock 不会挂起（没有 push_response 时同步返回错误），所以先触发 cancel，
        // 让 select! biased 命中 cancel 分支。
        let adapter = McpToolAdapter::new(client, meta("hang"));
        let ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            std::path::PathBuf::from("/tmp"),
        );
        ctx.cancel.cancel();
        let err = adapter
            .call(json!({}), ctx, ProgressSender::noop("t"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[tokio::test]
    async fn a_recording_copy_keeps_what_the_server_answered_as_it_answered() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("put")]));
        let answer = McpCallResult {
            content: vec![],
            is_error: true,
            meta: None,
            structured: Some(json!({"id": 1})),
            raw_content: Vec::new(),
        };
        client.push_response("put", answer.clone());
        let adapter = McpToolAdapter::new(client, meta("put"));
        let (tool, record) = adapter.recording();
        assert!(record.take().is_none(), "nothing before the call");
        let r = tool
            .call(json!({}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(matches!(r.content, ToolResultContent::Text(ref s) if s == "(empty response)"));
        match record.take() {
            Some(McpCallOutcome::Answered(kept)) => {
                assert!(kept.content.is_empty() && kept.is_error);
                assert_eq!(kept.structured, answer.structured);
            }
            other => panic!("{other:?}"),
        }
    }

    /// A host's call (`mcp.callTool`) is handed what the server answered to
    /// it: a status poll must reach the server every time, not get a
    /// session's answer from the shared cache.
    #[tokio::test]
    async fn a_recording_copy_is_not_answered_from_the_shared_cache() {
        let mut m = meta("status");
        m.hints = McpToolHints {
            read_only: Some(true),
            idempotent: Some(true),
            ..Default::default()
        };
        let client = Arc::new(MockMcpClient::new("s", vec![m.clone()]));
        client.push_response("status", text("pending", false));
        client.push_response("status", text("approved", false));
        let adapter = McpToolAdapter::new(client.clone(), m);
        adapter
            .call(json!({"id": 1}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        let (host, record) = adapter.recording();
        host.call(json!({"id": 1}), ctx(), ProgressSender::noop("t"))
            .await
            .unwrap();
        assert_eq!(
            client.calls.lock().unwrap().len(),
            2,
            "the server was not asked"
        );
        match record.take() {
            Some(McpCallOutcome::Answered(kept)) => {
                assert!(matches!(&kept.content[..], [McpContent::Text(t)] if t == "approved"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_recording_copy_keeps_a_failure_and_a_cancel() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("gone")]));
        let (tool, record) = McpToolAdapter::new(client.clone(), meta("gone")).recording();
        assert!(tool
            .call(json!({}), ctx(), ProgressSender::noop("t"))
            .await
            .is_err());
        assert!(matches!(record.take(), Some(McpCallOutcome::Failed(_))));
        let (tool, record) = McpToolAdapter::new(client, meta("gone")).recording();
        let stopped = ctx();
        stopped.cancel.cancel();
        assert!(tool
            .call(json!({}), stopped, ProgressSender::noop("t"))
            .await
            .is_err());
        assert!(matches!(record.take(), Some(McpCallOutcome::Cancelled)));
    }

    #[tokio::test]
    async fn non_object_input_validates_err() {
        let client = Arc::new(MockMcpClient::new("test", vec![meta("x")]));
        let adapter = McpToolAdapter::new(client, meta("x"));
        let r = adapter.validate_input(&json!("string"), &ctx()).await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn an_mcp_tool_is_its_servers() {
        let client = Arc::new(MockMcpClient::new("github", vec![]));
        let adapter = McpToolAdapter::new(client, meta("create_issue"));
        let facts = adapter.permission_facts(&json!({}), &ctx()).await;
        assert_eq!(facts.reach, base::interface::permission::Reach::Server);
    }
}
