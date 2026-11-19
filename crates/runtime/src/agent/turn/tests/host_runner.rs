//! 宿主在轮次之外经会话的工具管线调用工具对象：入口算宿主批准，规则与模式照拦，
//! 子调用不能问人，额外规则只管这一次，模式切换立即可见，能与正在跑的一轮并发。

use super::support::recall_test_settings;
use crate::agent::{Builder, InputMessage};
use base::context::goal::TurnOrigin;
use base::event::AgentEvent;
use base::interface::permission::{PermissionFacts, PermissionSet, Reach, Subject};
use base::interface::plugin_call::HostToolRunner;
use base::interface::tool_dispatch::CommandScope;
use base::interface::tool_middleware::{NextDispatch, ToolCall, ToolExec, ToolOutcome};
use base::permission::{PermissionMode, PermissionRule, RuleBehavior, RuleSource};
use base::tool::{InMemoryToolRegistry, Tool, ToolContext, ToolResult, ToolSpec};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

fn rule(behavior: RuleBehavior, tool: &str) -> PermissionRule {
    PermissionRule {
        source: RuleSource::Session,
        behavior,
        tool_name: tool.into(),
        rule_content: None,
    }
}

/// Stands in for the tool a host's plugin call runs: it makes `sub` as a
/// plugin function's call, if given, and says what came back.
struct PluginCall {
    read_only: bool,
    sub: Option<(CommandScope, &'static str)>,
    seen: Arc<Mutex<Vec<TurnOrigin>>>,
}

impl PluginCall {
    fn new(read_only: bool) -> Self {
        Self {
            read_only,
            sub: None,
            seen: Arc::default(),
        }
    }

    fn calling(tool: &'static str, read_only: bool) -> Self {
        let scope = CommandScope {
            command: "xflow.check".into(),
            plugin: "xflow".into(),
            read_only,
            writes: Vec::new(),
        };
        Self {
            sub: Some((scope, tool)),
            ..Self::new(true)
        }
    }
}

#[async_trait::async_trait]
impl Tool for PluginCall {
    fn name(&self) -> &str {
        "PluginCall"
    }
    fn description(&self) -> &str {
        "plugin call (test tool)"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
            .read_only(self.read_only)
            .needs_dispatch(true)
    }
    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        json!({"type": "object"})
    }
    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::Opaque("xflow.check".into()), Reach::Unknown)
    }
    async fn call(
        &self,
        _: Value,
        ctx: ToolContext,
        _: base::tool::ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        self.seen.lock().unwrap().push(ctx.turn.origin);
        let Some((scope, tool)) = &self.sub else {
            return Ok(ToolResult::text("ran"));
        };
        let function = ctx
            .dispatch
            .as_ref()
            .and_then(|d| d.for_command(scope.clone()))
            .expect("a plugin call gets a dispatch for its function");
        Ok(ToolResult::text(
            match function.call(tool, json!({}), ctx.cancel.clone()).await {
                Ok(answer) => answer.text,
                Err(refused) => format!("refused: {refused}"),
            },
        ))
    }
}

/// A tool a plugin function calls: `Touch` changes something the session
/// would ask about, `Put` writes a file, `Look` only looks at the session.
struct SubTool(&'static str);

#[async_trait::async_trait]
impl Tool for SubTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "sub tool (test tool)"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new()
            .callable_from_code(true)
            .read_only(self.0 == "Look")
    }
    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        json!({"type": "object"})
    }
    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        match self.0 {
            "Look" => PermissionFacts::new(Subject::None, Reach::SessionOnly),
            "Put" => PermissionFacts::new(Subject::WritePath("out.txt".into()), Reach::Inside),
            _ => PermissionFacts::new(Subject::Opaque("x".into()), Reach::Unknown),
        }
    }
    async fn call(
        &self,
        _: Value,
        _: ToolContext,
        _: base::tool::ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        Ok(ToolResult::text(format!("{} done", self.0)))
    }
}

#[derive(Default)]
struct Around(Mutex<Vec<(String, String, Option<String>)>>);

#[async_trait::async_trait]
impl base::interface::tool_middleware::ToolMiddleware for Around {
    async fn around(
        &self,
        call: &ToolCall,
        exec: &mut ToolExec,
        next: NextDispatch<'_>,
    ) -> ToolOutcome {
        self.0
            .lock()
            .unwrap()
            .push((call.name.clone(), call.via.clone(), call.plugin.clone()));
        next.run(call, exec).await
    }
}

struct Session {
    agent: crate::agent::Agent,
    events: crate::agent::EventReceiver,
    input: crate::agent::InputSender,
    runner: Arc<dyn HostToolRunner>,
    around: Arc<Around>,
    _dir: tempfile::TempDir,
}

fn session_with(
    rules: Vec<PermissionRule>,
    model: Arc<dyn base::interface::model::Model>,
    hold: Arc<Hold>,
) -> Session {
    let dir = tempfile::tempdir().unwrap();
    let tools = Arc::new(InMemoryToolRegistry::new());
    tools.register(Arc::new(SubTool("Touch")));
    tools.register(Arc::new(SubTool("Look")));
    tools.register(Arc::new(SubTool("Put")));
    tools.register(hold);
    let policy = permissions::policy::Policy::new(permissions::ruleset::RuleSet::new(rules));
    let around = Arc::new(Around::default());
    let (agent, events, input) = Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(model)
        .settings(Arc::new(recall_test_settings(dir.path())))
        .tools(tools)
        .permission(PermissionSet::of(Arc::new(policy)))
        .tool_middleware(around.clone())
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    let runner = agent.host_tool_runner();
    Session {
        agent,
        events,
        input,
        runner,
        around,
        _dir: dir,
    }
}

fn session(rules: Vec<PermissionRule>) -> Session {
    let model = super::support::FixedUsageModel {
        usage: Default::default(),
    };
    session_with(rules, Arc::new(model), Arc::default())
}

async fn host_calls(session: &Session, tool: PluginCall) -> ToolOutcome {
    session
        .runner
        .run(
            Arc::new(tool),
            json!({}),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn the_host_calling_is_its_approval_of_what_the_session_would_ask() {
    let s = session(Vec::new());
    let tool = PluginCall::new(false);
    let seen = tool.seen.clone();
    let outcome = host_calls(&s, tool).await;
    assert_eq!(outcome.expect("the host's call runs").text, "ran");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![TurnOrigin::Automation],
        "a call outside any turn is nobody's typed turn"
    );
}

#[tokio::test]
async fn a_deny_rule_stops_the_host_too() {
    let s = session(vec![rule(RuleBehavior::Deny, "PluginCall")]);
    let refused = host_calls(&s, PluginCall::new(true)).await.unwrap_err();
    assert!(refused.contains("denied by rule"), "{refused}");
}

#[tokio::test]
async fn a_mode_switched_after_the_runner_was_taken_is_the_mode_it_judges_in() {
    let s = session(Vec::new());
    let state = s.agent.session_state();

    assert!(state.switch_permission_mode(PermissionMode::Plan));
    let refused = host_calls(&s, PluginCall::new(false)).await.unwrap_err();
    assert!(refused.contains("plan mode"), "{refused}");
    let read = host_calls(&s, PluginCall::new(true)).await;
    assert_eq!(read.expect("plan mode lets a read through").text, "ran");

    assert!(state.switch_permission_mode(PermissionMode::Default));
    let written = host_calls(&s, PluginCall::new(false)).await;
    assert_eq!(
        written.expect("out of plan mode it writes again").text,
        "ran"
    );
}

#[tokio::test]
async fn a_function_call_that_needs_asking_fails_and_says_why() {
    let s = session(Vec::new());
    let answer = host_calls(&s, PluginCall::calling("Touch", false))
        .await
        .expect("the entry itself runs");
    assert!(answer.text.starts_with("refused: "), "{}", answer.text);
    assert!(answer.text.contains("Touch"), "{}", answer.text);
    assert!(
        answer.text.contains("plugin function the host called"),
        "{}",
        answer.text
    );

    let looked = host_calls(&s, PluginCall::calling("Look", true)).await;
    assert_eq!(looked.expect("the entry runs").text, "Look done");
}

#[tokio::test]
async fn a_function_the_host_did_not_let_write_writes_nothing() {
    let s = session(vec![rule(RuleBehavior::Allow, "Put")]);
    let answer = host_calls(&s, PluginCall::calling("Put", true))
        .await
        .expect("the entry runs");
    assert!(
        answer.text.contains("without letting it write"),
        "{}",
        answer.text
    );
}

#[tokio::test]
async fn tool_around_is_told_the_host_made_the_call_and_the_function_its_calls() {
    let s = session(Vec::new());
    host_calls(&s, PluginCall::calling("Look", true))
        .await
        .expect("the entry runs");
    assert_eq!(
        *s.around.0.lock().unwrap(),
        vec![
            ("PluginCall".to_string(), "host".to_string(), None),
            (
                "Look".to_string(),
                "PluginCall:xflow.check".to_string(),
                Some("xflow".to_string())
            ),
        ]
    );
}

#[tokio::test]
async fn the_rules_a_call_brings_hold_for_it_and_its_calls_only() {
    let s = session(Vec::new());
    let call = |tool: PluginCall, rules: Vec<PermissionRule>| {
        s.runner
            .run(Arc::new(tool), json!({}), rules, CancellationToken::new())
    };

    let refused = call(
        PluginCall::new(true),
        vec![rule(RuleBehavior::Deny, "PluginCall")],
    )
    .await
    .unwrap_err();
    assert!(refused.contains("denied by rule"), "{refused}");
    let again = host_calls(&s, PluginCall::new(true)).await;
    assert_eq!(again.expect("the next call has no such rule").text, "ran");

    let allowed = call(
        PluginCall::calling("Touch", false),
        vec![rule(RuleBehavior::Allow, "Touch")],
    )
    .await
    .expect("the entry runs");
    assert_eq!(allowed.text, "Touch done");

    let denied = call(
        PluginCall::calling("Look", true),
        vec![rule(RuleBehavior::Deny, "Look")],
    )
    .await
    .expect("the entry runs");
    assert!(denied.text.contains("denied by rule"), "{}", denied.text);
}

#[tokio::test]
async fn a_runner_of_an_ended_session_runs_nothing() {
    let s = session(Vec::new());
    let Session { agent, runner, .. } = s;
    drop(agent);
    let refused = runner
        .run(
            Arc::new(PluginCall::new(true)),
            json!({}),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(refused.contains("ended"), "{refused}");
}

/// Holds the turn's call open until released, after saying it has started.
#[derive(Default)]
struct Hold {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl Tool for Hold {
    fn name(&self) -> &str {
        "Hold"
    }
    fn description(&self) -> &str {
        "hold (test tool)"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new().read_only(true)
    }
    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        json!({"type": "object"})
    }
    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::SessionOnly)
    }
    async fn call(
        &self,
        _: Value,
        ctx: ToolContext,
        _: base::tool::ProgressSender,
    ) -> Result<ToolResult, base::error::ToolError> {
        ctx.session.post_notice(None, "held".into());
        self.started.notify_one();
        self.release.notified().await;
        Ok(ToolResult::text("released"))
    }
}

/// Calls `Hold` once, then answers.
#[derive(Default)]
struct HoldsOnce(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl base::interface::model::Model for HoldsOnce {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        _: Vec<base::prompt::PromptBlock>,
        _: Vec<base::interface::model::ToolDef>,
        _: Vec<base::interface::model::ModelMessage>,
        _: base::interface::model::StreamParams,
        _: CancellationToken,
    ) -> Result<base::interface::model::ModelStream, base::interface::model::ModelError> {
        use base::interface::model::ModelEvent;
        let first = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let events = if first {
            vec![
                Ok(ModelEvent::ToolUse {
                    id: "toolu_hold".into(),
                    name: "Hold".into(),
                    input: json!({}),
                }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "tool_use".into(),
                    usage: Default::default(),
                }),
            ]
        } else {
            vec![
                Ok(ModelEvent::TextDelta { text: "ok".into() }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ]
        };
        Ok(Box::new(futures::stream::iter(events)))
    }
}

#[tokio::test]
async fn a_host_call_runs_beside_a_turn_that_is_running_a_call() {
    let hold = Arc::new(Hold::default());
    let (started, release) = (hold.started.clone(), hold.release.clone());
    let s = session_with(Vec::new(), Arc::new(HoldsOnce::default()), hold);
    let Session {
        mut agent,
        mut events,
        input,
        runner,
        ..
    } = s;
    let engine = tokio::spawn(async move { agent.run(CancellationToken::new()).await });
    input
        .send(InputMessage::User {
            content: "hold on".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();
    started.notified().await;

    let hang_guard = std::time::Duration::from_secs(60);
    let beside = tokio::time::timeout(
        hang_guard,
        runner.run(
            Arc::new(PluginCall::calling("Touch", false)),
            json!({}),
            vec![rule(RuleBehavior::Allow, "Touch")],
            CancellationToken::new(),
        ),
    )
    .await
    .expect("the host's call waits on nothing the turn holds");
    assert_eq!(beside.expect("it runs").text, "Touch done");

    release.notify_one();
    tokio::time::timeout(hang_guard, async {
        while let Some(event) = events.recv().await {
            if matches!(event, AgentEvent::TurnComplete { .. }) {
                return;
            }
        }
        panic!("the session closed before its turn completed");
    })
    .await
    .expect("the turn completes once its call is released");
    drop(input);
    engine.abort();
}
