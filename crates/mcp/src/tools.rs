//! MCP 协议特性的工具包装：`ListMcpResources` / `ReadMcpResource`。
//!
//! 放在 mcp crate 里，tools 就不必依赖 mcp。CLI 直接从 `agix_mcp::tools::{...}` 装配。
//!
//! 设计取舍：
//! - **不绑定具体 server name**：列资源时枚举所有 connected server 并合并
//! - **read 不带 server 时按列出来的 uri 找 server**：只有恰好一个 server 的资源列表里有这个
//!   uri 才去读；没有或多于一个都报错，不广播给所有 server
//! - **不缓存**：每次 list 实时调 server（list_resources 通常很快）

use crate::client::{McpClientHandle, McpContent};
use async_trait::async_trait;
use base::interface::permission::{PermissionFacts, Reach, Relays, Subject};
use base::{
    error::ToolError, tool::ProgressSender, tool::PromptContext, tool::Tool, tool::ToolContext,
    tool::ToolResult, tool::ToolSpec, tool::ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
/// `facts` of a call that relays `server`'s words, and for a named server is
/// also a call on `mcp__<server>`, so a deny rule for that server holds.
fn named_for(facts: PermissionFacts, server: Option<String>) -> PermissionFacts {
    let facts = match &server {
        Some(server) => facts.also_named(rule_name_of(server)),
        None => facts,
    };
    facts.relaying(Relays::McpServer(server))
}

/// The name a deny rule for `server` is written with (`mcp__<server>`).
fn rule_name_of(server: &str) -> String {
    format!("mcp__{}", crate::naming::server_segment(server))
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct ListMcpResourcesInput {
    /// Optional server name to filter by; None = all connected servers
    #[serde(default)]
    pub server: Option<String>,
}

pub struct ListMcpResourcesTool {
    clients: Vec<McpClientHandle>,
}

impl ListMcpResourcesTool {
    /// Construct a new instance.
    pub fn new(clients: Vec<McpClientHandle>) -> Self {
        Self { clients }
    }
}

#[async_trait]
impl Tool for ListMcpResourcesTool {
    fn name(&self) -> &str {
        "ListMcpResources"
    }
    fn description(&self) -> &str {
        "List the resources (files, URIs, schemas) that connected MCP servers expose; pass `server` to list one server's."
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().concurrency_safe(true).read_only(true)
    }
    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        serde_json::to_value(schemars::schema_for!(ListMcpResourcesInput)).expect("schema")
    }
    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(
            "List resources exposed by connected MCP servers (files, URIs, db schemas, \
         etc — non-tool readable objects). Pass `server` to filter to one server. \
         Returns array of {server, uri, name, description, mime_type}."
                .into(),
        )
    }
    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }

    async fn permission_facts(&self, input: &Value, _: &ToolContext) -> PermissionFacts {
        let server = serde_json::from_value::<ListMcpResourcesInput>(input.clone())
            .ok()
            .and_then(|i| i.server);
        named_for(PermissionFacts::new(Subject::None, Reach::Operator), server)
    }
    async fn call(
        &self,
        input: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: ListMcpResourcesInput = serde_json::from_value(input).unwrap_or_default();
        let mut all = Vec::new();
        for client in &self.clients {
            if let Some(filter) = &input.server {
                if client.server_name() != filter {
                    continue;
                }
            }
            match client.list_resources().await {
                Ok(resources) => {
                    for r in resources {
                        all.push(json!({
                            "server": client.server_name(),
                            "uri": r.uri,
                            "name": r.name,
                            "description": r.description,
                            "mime_type": r.mime_type,
                        }));
                    }
                }
                Err(e) => {
                    // 某个 server 失败不阻塞整体；继续
                    tracing::warn!(server = %client.server_name(), error = %e, "list_resources failed");
                }
            }
        }
        let body = if all.is_empty() {
            "(no MCP resources available — either no servers connected or none expose resources)"
                .to_string()
        } else {
            format!(
                "{} resource(s):\n{}",
                all.len(),
                all.iter()
                    .filter_map(|r| {
                        let server = r.get("server")?.as_str()?;
                        let uri = r.get("uri")?.as_str()?;
                        let name = r.get("name")?.as_str()?;
                        Some(format!("  · [{server}] {uri} — {name}"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        Ok(ToolResult::text(body).with_structured(json!({"resources": all})))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadMcpResourceInput {
    /// Resource URI to read (from ListMcpResources output)
    pub uri: String,
    /// Server name; without it the one server that lists this URI is used
    #[serde(default)]
    pub server: Option<String>,
}

pub struct ReadMcpResourceTool {
    clients: Vec<McpClientHandle>,
}

impl ReadMcpResourceTool {
    /// Construct a new instance.
    pub fn new(clients: Vec<McpClientHandle>) -> Self {
        Self { clients }
    }

    /// The server named in `input`, or else the one server whose resource
    /// list holds the URI: a URI is never sent to a server that did not
    /// declare it.
    async fn target(&self, input: &ReadMcpResourceInput) -> Result<&McpClientHandle, String> {
        if let Some(name) = &input.server {
            return self
                .clients
                .iter()
                .find(|c| c.server_name() == name)
                .ok_or_else(|| format!("No matching MCP server (filter: {name:?})"));
        }
        let mut listing = Vec::new();
        for client in &self.clients {
            let lists_it = client
                .list_resources()
                .await
                .is_ok_and(|resources| resources.iter().any(|r| r.uri == input.uri));
            if lists_it {
                listing.push(client);
            }
        }
        match listing.as_slice() {
            [only] => Ok(only),
            [] => Err(format!(
                "no connected MCP server lists the resource '{}'; ListMcpResources shows what \
                 exists, and `server` names the one to ask",
                input.uri
            )),
            several => Err(format!(
                "the resource '{}' is listed by several MCP servers ({}); name one with `server`",
                input.uri,
                several
                    .iter()
                    .map(|c| c.server_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

#[async_trait]
impl Tool for ReadMcpResourceTool {
    fn name(&self) -> &str {
        "ReadMcpResource"
    }
    fn description(&self) -> &str {
        "Read one MCP resource by its URI, as ListMcpResources lists it."
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().concurrency_safe(true).read_only(true)
    }
    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        serde_json::to_value(schemars::schema_for!(ReadMcpResourceInput)).expect("schema")
    }
    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(
            "Read a single MCP resource by URI. Returns text content (or base64 blob for \
         binary). Use ListMcpResources first to find available URIs."
                .into(),
        )
    }
    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<ReadMcpResourceInput>(input.clone()) {
            Ok(p) if p.uri.trim().is_empty() => ValidationResult::err("uri must not be empty", 1),
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }

    async fn permission_facts(&self, input: &Value, _: &ToolContext) -> PermissionFacts {
        let server = match serde_json::from_value::<ReadMcpResourceInput>(input.clone()) {
            Ok(input) => self
                .target(&input)
                .await
                .ok()
                .map(|c| c.server_name().to_string()),
            Err(_) => None,
        };
        named_for(PermissionFacts::new(Subject::None, Reach::Operator), server)
    }
    async fn call(
        &self,
        input: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: ReadMcpResourceInput = serde_json::from_value(input)?;
        let client = match self.target(&input).await {
            Ok(client) => client,
            Err(message) => return Ok(ToolResult::error_text(message)),
        };
        match client.read_resource(&input.uri).await {
            Ok(contents) => {
                let text = contents
                    .iter()
                    .filter_map(|c| match c {
                        McpContent::Text(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let body = if text.is_empty() {
                    format!("[binary or empty resource: {} blocks]", contents.len())
                } else {
                    text
                };
                Ok(ToolResult::text(body)
                    .with_structured(json!({"server": client.server_name(), "uri": input.uri})))
            }
            Err(e) => Ok(ToolResult::error_text(format!(
                "could not read resource '{}': {e}",
                input.uri
            ))),
        }
    }
}

// 辅助：让 ListMcpResourcesTool::new 在没 client 时也能实例化（便于测试）
impl Default for ListMcpResourcesTool {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl Default for ReadMcpResourceTool {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

// DispatchMcpTool — introspection tool: lists which MCP servers have registered which tools.
// Because Agix flat-mounts each server's tools under `mcp__<server>__<tool>`
// names, this tool is used for discovery rather than dispatching calls.

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct DispatchMcpInput {
    /// Optional server name to filter
    #[serde(default)]
    pub server: Option<String>,
}

pub struct DispatchMcpTool {
    clients: Vec<McpClientHandle>,
}

impl DispatchMcpTool {
    /// Construct a new instance.
    pub fn new(clients: Vec<McpClientHandle>) -> Self {
        Self { clients }
    }
}

#[async_trait]
impl Tool for DispatchMcpTool {
    fn name(&self) -> &str {
        "MCP"
    }
    fn description(&self) -> &str {
        "List the tools each connected MCP server provides, under the `mcp__<server>__<tool>` names they are called by."
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().concurrency_safe(true).read_only(true)
    }
    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        serde_json::to_value(schemars::schema_for!(DispatchMcpInput)).expect("schema")
    }
    async fn guide(&self, _: &PromptContext) -> Option<String> {
        Some(
            "Introspect MCP server tools. Agix flat-mounts each MCP server's tools \
         under `mcp__<server>__<tool>` names — call them directly, no dispatcher \
         needed. This tool just lists what's available so you know the names."
                .into(),
        )
    }
    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }

    async fn permission_facts(&self, input: &Value, _: &ToolContext) -> PermissionFacts {
        let server = serde_json::from_value::<DispatchMcpInput>(input.clone())
            .ok()
            .and_then(|i| i.server);
        named_for(PermissionFacts::new(Subject::None, Reach::Operator), server)
    }
    async fn call(
        &self,
        input: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: DispatchMcpInput = serde_json::from_value(input).unwrap_or_default();
        let mut all = Vec::new();
        for client in &self.clients {
            if let Some(filter) = &input.server {
                if client.server_name() != filter {
                    continue;
                }
            }
            match client.list_tools().await {
                Ok(tools) => {
                    let names = crate::naming::tool_names(
                        client.server_name(),
                        tools.iter().map(|t| t.name.as_str()),
                    );
                    for (t, name) in tools.into_iter().zip(names) {
                        all.push(json!({
                            "server": client.server_name(),
                            "tool_name": name,
                            "raw_name": t.name,
                            "description": t.description,
                        }));
                    }
                }
                Err(e) => {
                    tracing::warn!(server = %client.server_name(), error = %e, "list_tools failed");
                }
            }
        }
        let body = if all.is_empty() {
            "(no MCP tools available)".to_string()
        } else {
            format!(
                "{} MCP tool(s):\n{}",
                all.len(),
                all.iter()
                    .filter_map(|t| {
                        let name = t.get("tool_name")?.as_str()?;
                        let desc = t.get("description")?.as_str().unwrap_or("");
                        let desc = base::text::truncate_chars(
                            desc,
                            crate::adapter::MAX_ADVERTISED_DESCRIPTION_CHARS,
                            "…",
                        );
                        Some(format!("  · {name} — {desc}"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        Ok(ToolResult::text(body).with_structured(json!({"tools": all})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{McpCallResult, McpClient, McpContent, McpResourceMeta, McpToolMeta};
    use crate::error::McpError;
    use std::sync::Arc;

    /// 测试用 mock client
    struct MockClient {
        name: String,
        resources: Vec<McpResourceMeta>,
        read_responses: std::sync::Mutex<std::collections::HashMap<String, Vec<McpContent>>>,
    }

    #[async_trait]
    impl McpClient for MockClient {
        fn server_name(&self) -> &str {
            &self.name
        }
        async fn list_tools(&self) -> Result<Vec<McpToolMeta>, McpError> {
            Ok(Vec::new())
        }
        async fn call_tool(
            &self,
            _: &str,
            _: serde_json::Map<String, Value>,
            _: &crate::elicit::McpCallScope,
        ) -> Result<McpCallResult, McpError> {
            Ok(McpCallResult::default())
        }
        async fn list_resources(&self) -> Result<Vec<McpResourceMeta>, McpError> {
            Ok(self.resources.clone())
        }
        async fn read_resource(&self, uri: &str) -> Result<Vec<McpContent>, McpError> {
            self.read_responses
                .lock()
                .unwrap()
                .get(uri)
                .cloned()
                .ok_or_else(|| McpError::Transport(anyhow::anyhow!("no such uri: {uri}")))
        }
    }

    fn mk(name: &str, resources: Vec<(&str, &str, &str)>) -> Arc<dyn McpClient> {
        let resources = resources
            .into_iter()
            .map(|(uri, name, desc)| McpResourceMeta {
                uri: uri.into(),
                name: name.into(),
                description: Some(desc.into()),
                mime_type: None,
            })
            .collect();
        Arc::new(MockClient {
            name: name.into(),
            resources,
            read_responses: std::sync::Mutex::new(std::collections::HashMap::new()),
        }) as Arc<dyn McpClient>
    }

    #[tokio::test]
    async fn list_aggregates_across_servers() {
        let c1 = mk("fs", vec![("file:///a", "a.txt", "desc a")]);
        let c2 = mk(
            "github",
            vec![("github://issues/1", "issue 1", "first issue")],
        );
        let tool = ListMcpResourcesTool::new(vec![c1, c2]);
        let r = tool
            .call(
                json!({}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    "/tmp".into(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("2 resource"));
                assert!(s.contains("[fs]"));
                assert!(s.contains("[github]"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn list_filters_by_server() {
        let c1 = mk("fs", vec![("file:///a", "a", "x")]);
        let c2 = mk("github", vec![("github://1", "1", "y")]);
        let tool = ListMcpResourcesTool::new(vec![c1, c2]);
        let r = tool
            .call(
                json!({"server": "fs"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    "/tmp".into(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("[fs]"));
                assert!(!s.contains("[github]"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn list_with_no_servers_returns_friendly_message() {
        let tool = ListMcpResourcesTool::new(vec![]);
        let r = tool
            .call(
                json!({}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    "/tmp".into(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("no MCP resources"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn read_unknown_uri_returns_error() {
        let c1 = mk("fs", vec![]);
        let tool = ReadMcpResourceTool::new(vec![c1]);
        let r = tool
            .call(
                json!({"uri": "file:///nowhere"}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    "/tmp".into(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        assert!(r.is_error);
    }

    fn serving(name: &str, uri: &str, text: &str) -> Arc<dyn McpClient> {
        Arc::new(MockClient {
            name: name.into(),
            resources: vec![McpResourceMeta {
                uri: uri.into(),
                name: "n".into(),
                description: None,
                mime_type: None,
            }],
            read_responses: std::sync::Mutex::new(
                [(uri.to_string(), vec![McpContent::Text(text.into())])].into(),
            ),
        })
    }

    #[tokio::test]
    async fn a_read_without_a_server_goes_to_the_one_server_that_lists_the_uri() {
        let tool = ReadMcpResourceTool::new(vec![
            mk("liar", vec![]),
            serving("honest", "db://schema", "REAL"),
        ]);
        let ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            "/tmp".into(),
        );
        let input = json!({"uri": "db://schema"});
        let facts = tool.permission_facts(&input, &ctx).await;
        assert_eq!(facts.also_named.as_deref(), Some("mcp__honest"));
        let r = tool
            .call(input, ctx, ProgressSender::noop("t"))
            .await
            .unwrap();
        assert!(!r.is_error);
        assert_eq!(r.structured_content.unwrap()["server"], "honest");
    }

    #[tokio::test]
    async fn a_read_nobody_lists_or_several_list_needs_a_server() {
        let tool = ReadMcpResourceTool::new(vec![
            serving("a", "db://x", "A"),
            serving("b", "db://x", "B"),
        ]);
        let ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            "/tmp".into(),
        );
        for (uri, wanted) in [("db://x", "several"), ("db://y", "no connected MCP server")] {
            let r = tool
                .call(
                    json!({ "uri": uri }),
                    ctx.clone(),
                    ProgressSender::noop("t"),
                )
                .await
                .unwrap();
            assert!(r.is_error, "{uri}");
            let base::tool::ToolResultContent::Text(text) = r.content else {
                panic!()
            };
            assert!(text.contains(wanted), "{text}");
        }
    }

    #[tokio::test]
    async fn read_validates_empty_uri() {
        let tool = ReadMcpResourceTool::new(vec![]);
        let r = tool
            .validate_input(
                &json!({"uri": "  "}),
                &session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    "/tmp".into(),
                ),
            )
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn mcp_dispatch_lists_zero_when_no_servers() {
        let tool = DispatchMcpTool::new(vec![]);
        let r = tool
            .call(
                json!({}),
                session_state::testing::tool_context_for_test(
                    platform::local_providers(),
                    "/tmp".into(),
                ),
                ProgressSender::noop("t"),
            )
            .await
            .unwrap();
        match r.content {
            base::tool::ToolResultContent::Text(s) => {
                assert!(s.contains("(no MCP tools available)"));
            }
            _ => panic!(),
        }
    }

    async fn states_a_contained_call(tool: &dyn base::tool::Tool, input: serde_json::Value) {
        use base::interface::permission::Reach;
        let ctx = session_state::testing::tool_context_for_test(
            platform::local_providers(),
            std::path::PathBuf::from("/tmp"),
        );
        let stated = tool.permission_facts(&input, &ctx).await;
        assert!(
            matches!(
                stated.reach,
                Reach::SessionOnly | Reach::Delegates | Reach::Operator | Reach::Inside
            ),
            "{}: {stated:?}",
            tool.name()
        );
        assert_eq!(stated.runs, None, "{}", tool.name());
    }

    #[tokio::test]
    async fn the_resource_and_introspection_tools_reach_only_what_the_operator_configured() {
        for (tool, input) in [
            (
                Box::new(ListMcpResourcesTool::new(vec![])) as Box<dyn Tool>,
                json!({}),
            ),
            (
                Box::new(ReadMcpResourceTool::new(vec![])),
                json!({ "uri": "file:///x" }),
            ),
            (Box::new(DispatchMcpTool::new(vec![])), json!({})),
        ] {
            let ctx = session_state::testing::tool_context_for_test(
                platform::local_providers(),
                std::path::PathBuf::from("/tmp"),
            );
            let facts = tool.permission_facts(&input, &ctx).await;
            assert_eq!(facts.reach, base::interface::permission::Reach::Operator);
            states_a_contained_call(tool.as_ref(), input).await;
        }
    }

    #[test]
    fn every_mcp_feature_tool_describes_itself() {
        let tools: [Box<dyn Tool>; 3] = [
            Box::new(ListMcpResourcesTool::new(vec![])),
            Box::new(ReadMcpResourceTool::new(vec![])),
            Box::new(DispatchMcpTool::new(vec![])),
        ];
        for tool in tools {
            assert!(!tool.description().is_empty(), "{}", tool.name());
        }
    }
}
