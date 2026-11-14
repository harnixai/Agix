//! The span tree of a run, derived from the telemetry event stream: a turn is
//! a trace root, and its model calls, tool calls, hook runs, compactions and
//! sub-agents hang under it. There is no session span: a daemon's sessions
//! live for days and a span is not exported until it ends, so
//! `gen_ai.conversation.id` ties the traces of one session together instead.
//!
//! The attribute names are the OpenTelemetry GenAI semantic conventions. No
//! content goes out, as in the JSONL: the events carry none.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::trace::{
    Span as _, SpanBuilder, SpanContext, SpanKind, Status, TraceContextExt, Tracer as _,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{Span, Tracer};
use telemetry::{EventPayload, TelemetryEvent};

/// Open spans, all tables together, before the oldest are ended as abandoned.
const MAX_OPEN_SPANS: usize = 4096;

#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Turn(String, String),
    Tool(String, String),
    Agent(String),
}

struct Open {
    session: String,
    span: Span,
    context: SpanContext,
    name: String,
}

#[derive(Default)]
struct SessionFacts {
    scene: String,
    model: String,
    last_turn: Option<String>,
}

#[derive(Default)]
struct Tables {
    spans: HashMap<Key, Open>,
    order: VecDeque<Key>,
    sessions: HashMap<String, SessionFacts>,
}

pub(crate) struct SpanTree {
    tracer: Tracer,
    tables: Mutex<Tables>,
}

fn at(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

fn before(ms: i64, latency_ms: u64) -> SystemTime {
    at(ms) - Duration::from_millis(latency_ms)
}

fn failed_stop(stop_reason: &str) -> bool {
    matches!(stop_reason, "error" | "api_error" | "failed")
}

impl SpanTree {
    pub(crate) fn new(tracer: Tracer) -> Self {
        Self {
            tracer,
            tables: Mutex::new(Tables::default()),
        }
    }

    pub(crate) fn observe(&self, event: &TelemetryEvent) {
        let mut tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
        self.apply(&mut tables, event);
    }

    fn start(
        &self,
        name: String,
        parent: Option<SpanContext>,
        started: SystemTime,
        attributes: Vec<KeyValue>,
    ) -> Span {
        let builder = SpanBuilder::from_name(name)
            .with_kind(SpanKind::Internal)
            .with_start_time(started)
            .with_attributes(attributes);
        let context = match parent {
            Some(parent) => Context::new().with_remote_span_context(parent),
            None => Context::new(),
        };
        self.tracer.build_with_context(builder, &context)
    }

    fn open(
        &self,
        tables: &mut Tables,
        owner: (&str, Key),
        name: String,
        parent: Option<SpanContext>,
        started: SystemTime,
        attributes: Vec<KeyValue>,
    ) {
        let (session, key) = owner;
        let span = self.start(name.clone(), parent, started, attributes);
        let context = span.span_context().clone();
        let open = Open {
            session: session.to_string(),
            span,
            context,
            name,
        };
        if let Some(mut replaced) = tables.spans.insert(key.clone(), open) {
            replaced
                .span
                .set_attribute(KeyValue::new("agix.abandoned", true));
            replaced.span.end();
        } else {
            tables.order.push_back(key);
        }
        while tables.spans.len() > MAX_OPEN_SPANS {
            let Some(oldest) = tables.order.pop_front() else {
                break;
            };
            if let Some(mut open) = tables.spans.remove(&oldest) {
                open.span
                    .set_attribute(KeyValue::new("agix.abandoned", true));
                open.span.end();
            }
        }
    }

    fn close(
        tables: &mut Tables,
        key: &Key,
        ended: SystemTime,
        attributes: Vec<KeyValue>,
        status: Option<Status>,
    ) {
        let Some(mut open) = tables.spans.remove(key) else {
            return;
        };
        tables.order.retain(|k| k != key);
        for attribute in attributes {
            open.span.set_attribute(attribute);
        }
        if let Some(status) = status {
            open.span.set_status(status);
        }
        open.span.end_with_timestamp(ended);
    }

    fn close_session(tables: &mut Tables, session: &str, ended: SystemTime) {
        let mine: Vec<Key> = tables
            .spans
            .iter()
            .filter(|(_, open)| open.session == session)
            .map(|(key, _)| key.clone())
            .collect();
        for key in mine {
            Self::close(
                tables,
                &key,
                ended,
                vec![KeyValue::new("agix.abandoned", true)],
                None,
            );
        }
        tables.sessions.remove(session);
    }

    fn under_turn(tables: &Tables, event: &TelemetryEvent) -> Option<SpanContext> {
        let turn = event.turn_id.clone()?;
        tables
            .spans
            .get(&Key::Turn(event.session_id.clone(), turn))
            .map(|o| o.context.clone())
    }

    fn base_attributes(event: &TelemetryEvent) -> Vec<KeyValue> {
        let mut attributes = vec![
            KeyValue::new("gen_ai.conversation.id", event.session_id.clone()),
            KeyValue::new("agix.turn_no", i64::from(event.turn_no)),
        ];
        if let Some(turn) = &event.turn_id {
            attributes.push(KeyValue::new("agix.turn_id", turn.clone()));
        }
        attributes
    }

    fn instant(
        &self,
        tables: &mut Tables,
        event: &TelemetryEvent,
        name: String,
        started: SystemTime,
        mut attributes: Vec<KeyValue>,
        status: Option<Status>,
    ) {
        attributes.extend(Self::base_attributes(event));
        let mut span = self.start(name, Self::under_turn(tables, event), started, attributes);
        if let Some(status) = status {
            span.set_status(status);
        }
        span.end_with_timestamp(at(event.timestamp_ms));
    }

    fn apply(&self, tables: &mut Tables, event: &TelemetryEvent) {
        let sid = &event.session_id;
        let now = at(event.timestamp_ms);
        match &event.payload {
            EventPayload::SessionStart(p) => {
                let facts = tables.sessions.entry(sid.clone()).or_default();
                facts.scene = p.scene.clone();
                facts.model = p.model.clone();
            }
            EventPayload::SessionEnd(_) => Self::close_session(tables, sid, now),
            EventPayload::TurnStart(_) => self.turn_started(tables, event),
            EventPayload::TurnComplete(p) => Self::turn_completed(tables, event, p),
            EventPayload::ApiRequest(p) => self.model_call(tables, event, p),
            EventPayload::ApiError(p) => self.failed_model_call(tables, event, p),
            EventPayload::ToolStart(p) => self.tool_started(tables, event, p),
            EventPayload::ToolExecution(p) => self.tool_executed(tables, event, p),
            EventPayload::ToolCancelled(p) => Self::tool_cancelled(tables, event, p),
            EventPayload::HookRun(p) => self.hook_run(tables, event, p),
            EventPayload::CompactAction(p) => self.compaction(tables, event, p),
            EventPayload::AgentSpawned(p) => self.agent_spawned(tables, event, p),
            EventPayload::AgentCompleted(p) => {
                let status = p.had_error.then(|| Status::error("sub-agent failed"));
                Self::close(
                    tables,
                    &Key::Agent(p.agent_id.clone()),
                    now,
                    Vec::new(),
                    status,
                );
                Self::close_session(tables, &p.agent_id, now);
            }
            _ => {}
        }
    }

    fn turn_started(&self, tables: &mut Tables, event: &TelemetryEvent) {
        let sid = &event.session_id;
        let Some(turn) = event.turn_id.clone() else {
            return;
        };
        let facts = tables.sessions.entry(sid.clone()).or_default();
        facts.last_turn = Some(turn.clone());
        let (scene, model) = (facts.scene.clone(), facts.model.clone());
        let agent = tables.spans.get(&Key::Agent(sid.clone()));
        let name = agent.map_or(scene, |a| a.name.clone());
        let parent = agent.map(|a| a.context.clone());
        let mut attributes = Self::base_attributes(event);
        attributes.push(KeyValue::new("gen_ai.operation.name", "invoke_agent"));
        attributes.push(KeyValue::new("gen_ai.agent.name", name.clone()));
        if !model.is_empty() {
            attributes.push(KeyValue::new("gen_ai.request.model", model));
        }
        let owner = (sid.as_str(), Key::Turn(sid.clone(), turn));
        let started = at(event.timestamp_ms);
        self.open(
            tables,
            owner,
            format!("invoke_agent {name}"),
            parent,
            started,
            attributes,
        );
    }

    fn turn_completed(
        tables: &mut Tables,
        event: &TelemetryEvent,
        p: &telemetry::TurnCompletePayload,
    ) {
        let Some(turn) = event.turn_id.clone() else {
            return;
        };
        let status = failed_stop(&p.stop_reason).then(|| Status::error(p.stop_reason.clone()));
        let attributes = vec![
            KeyValue::new("agix.stop_reason", p.stop_reason.clone()),
            KeyValue::new("agix.api_calls", i64::from(p.api_calls)),
            KeyValue::new("agix.tool_calls", i64::from(p.tool_calls)),
            KeyValue::new("agix.permission_denials", i64::from(p.permission_denials)),
        ];
        let key = Key::Turn(event.session_id.clone(), turn);
        Self::close(tables, &key, at(event.timestamp_ms), attributes, status);
    }

    fn model_call(
        &self,
        tables: &mut Tables,
        event: &TelemetryEvent,
        p: &telemetry::ApiRequestPayload,
    ) {
        let finish = opentelemetry::Value::Array(opentelemetry::Array::String(vec![p
            .stop_reason
            .clone()
            .into()]));
        let mut attributes = vec![
            KeyValue::new("gen_ai.operation.name", "chat"),
            KeyValue::new("gen_ai.request.model", p.model.clone()),
            KeyValue::new("gen_ai.usage.input_tokens", p.input_tokens as i64),
            KeyValue::new("gen_ai.usage.output_tokens", p.output_tokens as i64),
            KeyValue::new(
                "gen_ai.usage.cache_read.input_tokens",
                p.cache_read_tokens as i64,
            ),
            KeyValue::new(
                "gen_ai.usage.cache_write.input_tokens",
                p.cache_creation_tokens as i64,
            ),
            KeyValue::new("gen_ai.response.finish_reasons", finish),
        ];
        if let Some(purpose) = &p.purpose {
            attributes.push(KeyValue::new("agix.purpose", purpose.clone()));
        }
        let started = before(event.timestamp_ms, p.latency_ms);
        self.instant(
            tables,
            event,
            format!("chat {}", p.model),
            started,
            attributes,
            None,
        );
    }

    fn failed_model_call(
        &self,
        tables: &mut Tables,
        event: &TelemetryEvent,
        p: &telemetry::ApiErrorPayload,
    ) {
        let attributes = vec![
            KeyValue::new("gen_ai.operation.name", "chat"),
            KeyValue::new("gen_ai.request.model", p.model.clone()),
            KeyValue::new("error.type", p.error_kind.clone()),
        ];
        let started = before(event.timestamp_ms, p.latency_ms);
        let status = Some(Status::error(p.error_kind.clone()));
        self.instant(
            tables,
            event,
            format!("chat {}", p.model),
            started,
            attributes,
            status,
        );
    }

    fn tool_attributes(event: &TelemetryEvent, tool: &str, call_id: &str) -> Vec<KeyValue> {
        let mut attributes = Self::base_attributes(event);
        attributes.extend([
            KeyValue::new("gen_ai.operation.name", "execute_tool"),
            KeyValue::new("gen_ai.tool.name", tool.to_string()),
            KeyValue::new("gen_ai.tool.call.id", call_id.to_string()),
        ]);
        attributes
    }

    fn tool_started(
        &self,
        tables: &mut Tables,
        event: &TelemetryEvent,
        p: &telemetry::ToolStartPayload,
    ) {
        let sid = &event.session_id;
        let mut attributes = Self::tool_attributes(event, &p.tool_name, &p.tool_use_id);
        let kind = if p.extension { "extension" } else { "function" };
        attributes.push(KeyValue::new("gen_ai.tool.type", kind));
        let parent = Self::under_turn(tables, event);
        let owner = (sid.as_str(), Key::Tool(sid.clone(), p.tool_use_id.clone()));
        let started = at(event.timestamp_ms);
        self.open(
            tables,
            owner,
            format!("execute_tool {}", p.tool_name),
            parent,
            started,
            attributes,
        );
    }

    fn tool_executed(
        &self,
        tables: &mut Tables,
        event: &TelemetryEvent,
        p: &telemetry::ToolExecutionPayload,
    ) {
        let sid = &event.session_id;
        let key = Key::Tool(sid.clone(), p.tool_use_id.clone());
        if !tables.spans.contains_key(&key) {
            let attributes = Self::tool_attributes(event, &p.tool_name, &p.tool_use_id);
            let parent = Self::under_turn(tables, event);
            let started = before(event.timestamp_ms, p.latency_ms);
            let name = format!("execute_tool {}", p.tool_name);
            self.open(
                tables,
                (sid, key.clone()),
                name,
                parent,
                started,
                attributes,
            );
        }
        let failed = !matches!(p.outcome, telemetry::ToolOutcome::Succeeded);
        let outcome = serde_json::to_value(p.outcome)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let mut attributes = vec![
            KeyValue::new("agix.user_approved", p.user_approved),
            KeyValue::new("agix.result_chars", p.result_content_size as i64),
        ];
        if failed {
            attributes.push(KeyValue::new("error.type", outcome.clone()));
        }
        let status = failed.then(|| Status::error(outcome));
        Self::close(tables, &key, at(event.timestamp_ms), attributes, status);
    }

    fn tool_cancelled(
        tables: &mut Tables,
        event: &TelemetryEvent,
        p: &telemetry::ToolCancelledPayload,
    ) {
        let name = format!("execute_tool {}", p.tool_name);
        let cancelled = tables.spans.iter().find_map(|(key, open)| match key {
            Key::Tool(s, _) if *s == event.session_id && open.name == name => Some(key.clone()),
            _ => None,
        });
        if let Some(key) = cancelled {
            let attributes = vec![KeyValue::new("error.type", "cancelled")];
            let status = Some(Status::error(p.reason.clone()));
            Self::close(tables, &key, at(event.timestamp_ms), attributes, status);
        }
    }

    fn hook_run(&self, tables: &mut Tables, event: &TelemetryEvent, p: &telemetry::HookRunPayload) {
        let attributes = vec![
            KeyValue::new("agix.hook.event", p.event.clone()),
            KeyValue::new("agix.hook.count", p.hook_count as i64),
            KeyValue::new("agix.hook.blocked", p.blocked),
            KeyValue::new("agix.hook.errors", p.errors as i64),
        ];
        let started = before(event.timestamp_ms, p.duration_ms);
        self.instant(
            tables,
            event,
            format!("hook {}", p.event),
            started,
            attributes,
            None,
        );
    }

    fn compaction(
        &self,
        tables: &mut Tables,
        event: &TelemetryEvent,
        p: &telemetry::CompactActionPayload,
    ) {
        let kind = match p.kind {
            telemetry::CompactKind::Compact => "compact",
            telemetry::CompactKind::Trim => "trim",
        };
        let attributes = vec![
            KeyValue::new("agix.compact.kind", kind),
            KeyValue::new("agix.compact.strategy", p.strategy.clone()),
            KeyValue::new("agix.tokens_before", p.before_tokens as i64),
            KeyValue::new("agix.tokens_after", p.after_tokens as i64),
        ];
        let status = (!p.success).then(|| Status::error("compaction failed"));
        let started = before(event.timestamp_ms, p.latency_ms);
        self.instant(
            tables,
            event,
            format!("compact {}", p.strategy),
            started,
            attributes,
            status,
        );
    }

    fn agent_spawned(
        &self,
        tables: &mut Tables,
        event: &TelemetryEvent,
        p: &telemetry::AgentSpawnedPayload,
    ) {
        let sid = &event.session_id;
        let by_tool = p
            .parent_tool_use_id
            .as_ref()
            .and_then(|id| tables.spans.get(&Key::Tool(sid.clone(), id.clone())));
        let parent = by_tool.map(|o| o.context.clone()).or_else(|| {
            let turn = tables.sessions.get(sid)?.last_turn.clone()?;
            tables
                .spans
                .get(&Key::Turn(sid.clone(), turn))
                .map(|o| o.context.clone())
        });
        let mut attributes = Self::base_attributes(event);
        attributes.extend([
            KeyValue::new("gen_ai.operation.name", "invoke_agent"),
            KeyValue::new("gen_ai.agent.name", p.role.clone()),
            KeyValue::new("gen_ai.agent.id", p.agent_id.clone()),
            KeyValue::new("agix.background", p.background),
        ]);
        let key = Key::Agent(p.agent_id.clone());
        let started = at(event.timestamp_ms);
        self.open(
            tables,
            (sid, key.clone()),
            p.role.clone(),
            parent,
            started,
            attributes,
        );
        if let Some(open) = tables.spans.get_mut(&key) {
            open.span.update_name(format!("invoke_agent {}", p.role));
        }
    }
}
