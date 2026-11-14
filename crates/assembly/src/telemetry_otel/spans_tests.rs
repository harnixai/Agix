use std::collections::HashMap;

use opentelemetry::trace::{SpanId, Status, TracerProvider as _};
use opentelemetry::Value;
use opentelemetry_sdk::export::trace::{ExportResult, SpanData, SpanExporter};
use telemetry::{
    AgentCompletedPayload, AgentSpawnedPayload, ApiErrorPayload, ApiRequestPayload,
    CompactActionPayload, CompactKind, HookRunPayload, SessionEndPayload, SessionStartPayload,
    TelemetryEvent, ToolExecutionPayload, ToolOutcome, ToolStartPayload, TurnCompletePayload,
    TurnStartPayload,
};

use super::spans::SpanTree;

#[derive(Clone, Debug, Default)]
struct InMemorySpanExporter(std::sync::Arc<std::sync::Mutex<Vec<SpanData>>>);

impl SpanExporter for InMemorySpanExporter {
    fn export(
        &mut self,
        batch: Vec<SpanData>,
    ) -> futures::future::BoxFuture<'static, ExportResult> {
        self.0.lock().unwrap().extend(batch);
        Box::pin(std::future::ready(Ok(())))
    }
}

impl InMemorySpanExporter {
    fn get_finished_spans(&self) -> Result<Vec<SpanData>, std::convert::Infallible> {
        Ok(self.0.lock().unwrap().clone())
    }
}

fn tree() -> (SpanTree, InMemorySpanExporter) {
    let exporter = InMemorySpanExporter::default();
    let provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    (SpanTree::new(provider.tracer("test")), exporter)
}

fn session_start(sid: &str) -> TelemetryEvent {
    TelemetryEvent::session_start(
        sid,
        0,
        None,
        SessionStartPayload {
            scene: "coder".into(),
            permission_mode: "default".into(),
            model: "m-1".into(),
            max_tokens: 1,
            thinking_mode: false,
            sandbox_enabled: true,
            resume_from: None,
            auth_modes: vec![],
            mcp_server_count: 0,
            plugin_count: 0,
            skill_count: 0,
            output_format: "jsonrpc".into(),
            started_at_ms: 0,
        },
    )
}

fn at(mut e: TelemetryEvent, ms: i64) -> TelemetryEvent {
    e.timestamp_ms = ms;
    e
}

fn turn_start(sid: &str, turn: &str, ms: i64) -> TelemetryEvent {
    at(
        TelemetryEvent::turn_start(
            sid,
            0,
            Some(turn.into()),
            TurnStartPayload {
                resumed: false,
                is_retry: false,
            },
        ),
        ms,
    )
}

fn turn_complete(sid: &str, turn: &str, ms: i64, stop_reason: &str) -> TelemetryEvent {
    at(
        TelemetryEvent::turn_complete(
            sid,
            0,
            Some(turn.into()),
            TurnCompletePayload {
                stop_reason: stop_reason.into(),
                api_calls: 1,
                tool_calls: 1,
                permission_denials: 0,
                last_tool_name: None,
                last_tool_was_error: false,
                turn_duration_ms: 0,
            },
        ),
        ms,
    )
}

fn api_request(sid: &str, turn: Option<&str>, ms: i64) -> TelemetryEvent {
    at(
        TelemetryEvent::api_request(
            sid,
            0,
            turn.map(str::to_string),
            ApiRequestPayload {
                model: "m-1".into(),
                input_tokens: 11,
                output_tokens: 7,
                cache_creation_tokens: 3,
                cache_read_tokens: 5,
                latency_ms: 50,
                stop_reason: "end_turn".into(),
                input_message_count: 2,
                tool_count: 1,
                default_model: true,
                purpose: turn.is_none().then(|| "title".to_string()),
                truncated: false,
            },
        ),
        ms,
    )
}

fn tool_start(sid: &str, turn: &str, tool: &str, id: &str, ms: i64) -> TelemetryEvent {
    at(
        TelemetryEvent::tool_start(
            sid,
            0,
            Some(turn.into()),
            ToolStartPayload {
                tool_name: tool.into(),
                tool_use_id: id.into(),
                input_json_size: 2,
                destructive: false,
                deferred: false,
                extension: false,
            },
        ),
        ms,
    )
}

fn tool_execution(
    sid: &str,
    turn: &str,
    tool: &str,
    id: &str,
    outcome: ToolOutcome,
    ms: i64,
) -> TelemetryEvent {
    at(
        TelemetryEvent::tool_execution(
            sid,
            0,
            Some(turn.into()),
            ToolExecutionPayload {
                tool_name: tool.into(),
                tool_use_id: id.into(),
                outcome,
                is_error: !matches!(outcome, ToolOutcome::Succeeded),
                error_message: None,
                latency_ms: 10,
                input_json_size: 2,
                result_content_size: 4,
                user_approved: false,
            },
        ),
        ms,
    )
}

fn agent_spawned(sid: &str, turn: &str, child: &str, tool_use_id: Option<&str>) -> TelemetryEvent {
    TelemetryEvent::agent_spawned(
        sid,
        0,
        Some(turn.into()),
        AgentSpawnedPayload {
            agent_id: child.into(),
            role: "explorer".into(),
            model: None,
            stage_name: None,
            background: true,
            parent_tool_use_id: tool_use_id.map(str::to_string),
        },
    )
}

fn agent_completed(sid: &str, child: &str, ms: i64) -> TelemetryEvent {
    at(
        TelemetryEvent::agent_completed(
            sid,
            0,
            None,
            AgentCompletedPayload {
                agent_id: child.into(),
                role: "explorer".into(),
                had_error: false,
                duration_ms: 1,
            },
        ),
        ms,
    )
}

fn by_name(exporter: &InMemorySpanExporter) -> HashMap<String, SpanData> {
    exporter
        .get_finished_spans()
        .unwrap()
        .into_iter()
        .map(|s| (s.name.to_string(), s))
        .collect()
}

fn attribute<'a>(span: &'a SpanData, key: &str) -> Option<&'a Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| &kv.value)
}

#[test]
fn a_turn_is_a_trace_with_its_model_calls_tools_hooks_and_compaction_under_it() {
    let (tree, exporter) = tree();
    tree.observe(&session_start("s1"));
    tree.observe(&turn_start("s1", "t1", 1_000));
    tree.observe(&tool_start("s1", "t1", "Bash", "u1", 1_100));
    tree.observe(&at(
        TelemetryEvent::hook_run(
            "s1",
            0,
            Some("t1".into()),
            HookRunPayload {
                event: "PreToolUse".into(),
                hook_count: 1,
                duration_ms: 20,
                blocked: false,
                errors: 0,
                tool_use_id: Some("u1".into()),
            },
        ),
        1_090,
    ));
    tree.observe(&tool_execution(
        "s1",
        "t1",
        "Bash",
        "u1",
        ToolOutcome::Succeeded,
        1_200,
    ));
    tree.observe(&api_request("s1", Some("t1"), 1_300));
    tree.observe(&at(
        TelemetryEvent::compact_action(
            "s1",
            0,
            Some("t1".into()),
            CompactActionPayload {
                kind: CompactKind::Compact,
                strategy: "llm".into(),
                before_tokens: 10,
                after_tokens: 5,
                before_message_count: 4,
                after_message_count: 2,
                success: true,
                latency_ms: 30,
            },
        ),
        1_400,
    ));
    tree.observe(&turn_complete("s1", "t1", 1_500, "end_turn"));

    let spans = by_name(&exporter);
    let root = &spans["invoke_agent coder"];
    assert_eq!(root.parent_span_id, SpanId::INVALID);
    for name in [
        "execute_tool Bash",
        "chat m-1",
        "hook PreToolUse",
        "compact llm",
    ] {
        let child = &spans[name];
        assert_eq!(child.parent_span_id, root.span_context.span_id(), "{name}");
        assert_eq!(child.span_context.trace_id(), root.span_context.trace_id());
    }
    assert_eq!(
        attribute(root, "gen_ai.conversation.id"),
        Some(&Value::from("s1"))
    );
    assert_eq!(
        attribute(root, "agix.stop_reason"),
        Some(&Value::from("end_turn"))
    );
}

#[test]
fn a_sub_agents_turns_hang_under_the_agent_tool_call_that_started_it() {
    let (tree, exporter) = tree();
    tree.observe(&session_start("p"));
    tree.observe(&turn_start("p", "t1", 1_000));
    tree.observe(&tool_start("p", "t1", "Agent", "u1", 1_010));
    tree.observe(&agent_spawned("p", "t1", "c", Some("u1")));
    tree.observe(&turn_start("c", "ct1", 1_020));
    tree.observe(&turn_complete("c", "ct1", 1_090, "end_turn"));
    tree.observe(&agent_completed("p", "c", 1_095));
    tree.observe(&tool_execution(
        "p",
        "t1",
        "Agent",
        "u1",
        ToolOutcome::Succeeded,
        1_100,
    ));
    tree.observe(&turn_complete("p", "t1", 1_110, "end_turn"));

    let spans = by_name(&exporter);
    let tool = &spans["execute_tool Agent"];
    let agent = &spans["invoke_agent explorer"];
    let child_turn = exporter
        .get_finished_spans()
        .unwrap()
        .into_iter()
        .find(|s| {
            attribute(s, "gen_ai.conversation.id") == Some(&Value::from("c"))
                && s.name == "invoke_agent explorer"
                && s.span_context.span_id() != agent.span_context.span_id()
        })
        .expect("the sub-agent's turn");
    assert_eq!(agent.parent_span_id, tool.span_context.span_id());
    assert_eq!(child_turn.parent_span_id, agent.span_context.span_id());
    assert_eq!(
        child_turn.span_context.trace_id(),
        spans["invoke_agent coder"].span_context.trace_id()
    );
}

#[test]
fn a_background_sub_agent_outliving_its_tool_call_still_has_it_as_parent() {
    let (tree, exporter) = tree();
    tree.observe(&session_start("p"));
    tree.observe(&turn_start("p", "t1", 1_000));
    tree.observe(&tool_start("p", "t1", "Agent", "u1", 1_010));
    tree.observe(&agent_spawned("p", "t1", "c", Some("u1")));
    tree.observe(&tool_execution(
        "p",
        "t1",
        "Agent",
        "u1",
        ToolOutcome::Succeeded,
        1_020,
    ));
    tree.observe(&turn_complete("p", "t1", 1_030, "end_turn"));
    tree.observe(&turn_start("c", "ct1", 2_000));
    tree.observe(&turn_complete("c", "ct1", 2_100, "end_turn"));
    tree.observe(&agent_completed("p", "c", 2_110));

    let finished = exporter.get_finished_spans().unwrap();
    let root = finished
        .iter()
        .find(|s| s.name == "invoke_agent coder")
        .unwrap();
    let late = finished
        .iter()
        .find(|s| {
            attribute(s, "gen_ai.conversation.id") == Some(&Value::from("c"))
                && s.start_time > root.end_time
        })
        .expect("a span that starts after its parent turn ended");
    assert_eq!(late.span_context.trace_id(), root.span_context.trace_id());
}

#[test]
fn model_span_carries_gen_ai_model_and_usage_and_nothing_of_the_content() {
    let (tree, exporter) = tree();
    tree.observe(&session_start("s1"));
    tree.observe(&turn_start("s1", "t1", 1_000));
    tree.observe(&api_request("s1", Some("t1"), 1_300));
    tree.observe(&turn_complete("s1", "t1", 1_500, "end_turn"));
    let spans = by_name(&exporter);
    let chat = &spans["chat m-1"];
    for (key, value) in [
        ("gen_ai.operation.name", Value::from("chat")),
        ("gen_ai.request.model", Value::from("m-1")),
        ("gen_ai.usage.input_tokens", Value::from(11_i64)),
        ("gen_ai.usage.output_tokens", Value::from(7_i64)),
        ("gen_ai.usage.cache_read.input_tokens", Value::from(5_i64)),
        ("gen_ai.usage.cache_write.input_tokens", Value::from(3_i64)),
    ] {
        assert_eq!(attribute(chat, key), Some(&value), "{key}");
    }
    for span in exporter.get_finished_spans().unwrap() {
        for kv in &span.attributes {
            let key = kv.key.as_str();
            assert!(
                !key.contains("messages")
                    && !key.contains("arguments")
                    && !key.contains("result")
                    && !key.contains("system_instructions"),
                "{key}"
            );
        }
    }
}

#[test]
fn a_failed_model_call_and_a_failed_tool_are_error_spans() {
    let (tree, exporter) = tree();
    tree.observe(&session_start("s1"));
    tree.observe(&turn_start("s1", "t1", 1_000));
    tree.observe(&at(
        TelemetryEvent::api_error(
            "s1",
            0,
            Some("t1".into()),
            ApiErrorPayload {
                model: "m-1".into(),
                error_kind: "rate_limited".into(),
                http_status: Some(429),
                error_message: "slow down".into(),
                latency_ms: 10,
            },
        ),
        1_100,
    ));
    tree.observe(&tool_start("s1", "t1", "Bash", "u1", 1_110));
    tree.observe(&tool_execution(
        "s1",
        "t1",
        "Bash",
        "u1",
        ToolOutcome::Failed,
        1_200,
    ));
    tree.observe(&turn_complete("s1", "t1", 1_300, "end_turn"));
    let spans = by_name(&exporter);
    assert!(matches!(spans["chat m-1"].status, Status::Error { .. }));
    assert_eq!(
        attribute(&spans["chat m-1"], "error.type"),
        Some(&Value::from("rate_limited"))
    );
    assert!(matches!(
        spans["execute_tool Bash"].status,
        Status::Error { .. }
    ));
    assert_eq!(
        attribute(&spans["execute_tool Bash"], "error.type"),
        Some(&Value::from("failed"))
    );
}

#[test]
fn spans_left_open_end_when_their_session_ends() {
    let (tree, exporter) = tree();
    tree.observe(&session_start("s1"));
    tree.observe(&turn_start("s1", "t1", 1_000));
    tree.observe(&tool_start("s1", "t1", "Bash", "u1", 1_010));
    assert!(exporter.get_finished_spans().unwrap().is_empty());
    tree.observe(&at(
        TelemetryEvent::session_end(
            "s1",
            0,
            None,
            SessionEndPayload {
                duration_ms: 1,
                total_turns: 1,
                total_api_calls: 0,
                total_tool_calls: 0,
                total_permission_denials: 0,
                total_input_tokens: 0,
                total_output_tokens: 0,
                total_cache_creation: 0,
                total_cache_read: 0,
                stop_reason: "close".into(),
            },
        ),
        1_100,
    ));
    let finished = exporter.get_finished_spans().unwrap();
    assert_eq!(finished.len(), 2);
    assert!(finished
        .iter()
        .all(|s| attribute(s, "agix.abandoned") == Some(&Value::Bool(true))));
}

#[test]
fn open_spans_are_capped_and_the_oldest_are_ended_as_abandoned() {
    let (tree, exporter) = tree();
    tree.observe(&session_start("s1"));
    tree.observe(&turn_start("s1", "t1", 1_000));
    for i in 0..4_100 {
        tree.observe(&tool_start("s1", "t1", "Bash", &format!("u{i}"), 1_001));
    }
    let finished = exporter.get_finished_spans().unwrap();
    assert_eq!(finished.len(), 5);
    assert!(finished
        .iter()
        .all(|s| attribute(s, "agix.abandoned") == Some(&Value::Bool(true))));
}

#[test]
fn an_auxiliary_call_outside_a_turn_is_a_root_span() {
    let (tree, exporter) = tree();
    tree.observe(&session_start("s1"));
    tree.observe(&api_request("s1", None, 1_000));
    let spans = by_name(&exporter);
    let chat = &spans["chat m-1"];
    assert_eq!(chat.parent_span_id, SpanId::INVALID);
    assert_eq!(attribute(chat, "agix.purpose"), Some(&Value::from("title")));
}
