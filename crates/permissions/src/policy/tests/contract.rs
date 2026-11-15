//! The contract types of `base::interface::permission`, exercised through their public constructors.

use async_trait::async_trait;
use base::context::Session;
use base::interface::permission::{
    Attendance, CallTraits, HookAnswer, ModeStance, PermissionFacts, PermissionRequest, Reach,
    Runs, Subject, ToolFacts, ToolOrigin,
};
use base::error::ToolError;
use base::interface::tool::{PathStyle, ProgressSender, ToolResult, ToolSpec};
use base::tool::{Tool, ToolContext};
use serde_json::{json, Value};
use session_state::SessionState;
use std::path::PathBuf;
use std::sync::Arc;

struct Silent;

#[async_trait]
impl Tool for Silent {
    fn name(&self) -> &str {
        "Silent"
    }
    fn description(&self) -> &str {
        "states nothing about its calls"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new().read_only(true)
    }
    fn input_schema(&self, _: PathStyle) -> Value {
        json!({"type": "object"})
    }
    async fn call(
        &self,
        _: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("ok"))
    }
}

struct Declares {
    spec: ToolSpec,
    facts: PermissionFacts,
}

impl Declares {
    fn stating(facts: PermissionFacts) -> Self {
        Self {
            spec: ToolSpec::new().origin(ToolOrigin::plugin("p")),
            facts,
        }
    }
}

#[async_trait]
impl Tool for Declares {
    fn name(&self) -> &str {
        "Declares"
    }
    fn description(&self) -> &str {
        "states its own facts"
    }
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn input_schema(&self, _: PathStyle) -> Value {
        json!({"type": "object"})
    }
    fn call_traits(&self, input: &Value) -> CallTraits {
        self.spec.traits.read_only(input.get("peek").is_some())
    }
    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        self.facts.clone()
    }
    async fn call(
        &self,
        _: Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("ok"))
    }
}

fn ctx() -> ToolContext {
    session_state::testing::tool_context_for_test(
        base::interface::exec::ExecProviders::in_process(),
        PathBuf::from("/tmp"),
    )
}

/// SEC F-6: a tool that says nothing about a call is asked about, even
/// one that only reads.
#[tokio::test]
async fn a_tool_that_states_nothing_claims_nothing() {
    assert_eq!(
        Silent.permission_facts(&json!({}), &ctx()).await,
        PermissionFacts::unknown()
    );
    let facts = ToolFacts::gather(&Silent, &json!({}), &ctx()).await;
    assert_eq!(facts.reach, Reach::Unknown);
    assert_eq!(facts.subject, Subject::None);
    assert!(facts.traits.read_only);
}

#[tokio::test]
async fn gathered_facts_carry_the_spec_and_the_traits_of_this_input() {
    let mut tool = Declares::stating(PermissionFacts::new(
        Subject::ReadPath("src/lib.rs".into()),
        Reach::Inside,
    ));
    tool.spec = ToolSpec::new()
        .origin(ToolOrigin::mcp("srv"))
        .stance(ModeStance::Coordination)
        .concurrency_safe(true);
    let ctx = ctx();

    let peek = ToolFacts::gather(&tool, &json!({"peek": 1}), &ctx).await;
    assert_eq!(peek.subject, Subject::ReadPath("src/lib.rs".into()));
    assert_eq!(peek.reach, Reach::Inside);
    assert_eq!(peek.stance, ModeStance::Coordination);
    assert_eq!(peek.origin, ToolOrigin::mcp("srv"));
    assert!(peek.traits.read_only && peek.traits.concurrency_safe);

    let write = ToolFacts::gather(&tool, &json!({}), &ctx).await;
    assert!(!write.traits.read_only);
}

#[tokio::test]
async fn a_tool_that_states_its_facts_is_taken_at_its_word() {
    let stated = PermissionFacts::new(Subject::Shell("tail -f log".into()), Reach::Unknown)
        .with_runs(Runs::background())
        .with_question("watch it?");
    let facts = ToolFacts::gather(&Declares::stating(stated.clone()), &json!({}), &ctx()).await;
    assert_eq!(facts.subject, stated.subject);
    assert_eq!(facts.reach, stated.reach);
    assert_eq!(facts.runs, Some(Runs { background: true }));
    assert_eq!(facts.question.as_deref(), Some("watch it?"));
    assert_eq!(facts.origin, ToolOrigin::plugin("p"));
    assert_eq!(facts.stance, ModeStance::Normal);
}

#[test]
fn an_unknown_tool_claims_nothing() {
    let facts = ToolFacts::unknown_tool();
    assert_eq!(facts.reach, Reach::Unknown);
    assert_eq!(facts.subject, Subject::None);
    assert!(!facts.traits.read_only);
    assert_eq!(facts.runs, None);
}

#[test]
fn a_subject_keeps_its_content_whatever_its_kind() {
    for subject in [
        Subject::Opaque("x".into()),
        Subject::ReadPath("x".into()),
        Subject::WritePath("x".into()),
        Subject::Shell("x".into()),
        Subject::Url("x".into()),
    ] {
        assert_eq!(subject.content(), Some("x"));
    }
    assert_eq!(Subject::None.content(), None);
}

#[test]
fn a_request_is_attended_as_its_context_and_unanswered_by_a_hook() {
    let session: Arc<dyn Session> = Arc::new(SessionState::for_tests(PathBuf::from("/tmp")));
    let input = json!({});
    let facts = ToolFacts::unknown_tool();
    let mut ctx = ctx();
    ctx.attendance = Attendance::Unattended;
    let bare = PermissionRequest::new("Silent", None, &input, &facts, &ctx, &session);
    assert_eq!(bare.hook, &HookAnswer::None);
    assert_eq!(bare.attendance, Attendance::Unattended);
    assert!(bare.delegate.is_none());

    let hook = HookAnswer::Approve {
        hook: "audit".into(),
    };
    let full = bare
        .with_hook(&hook)
        .with_attendance(Attendance::NoOneToAsk("a program's call"));
    let copy = full;
    assert_eq!(copy.facts, &facts);
    assert_eq!(copy.ctx.session_id, "test");
    assert_eq!(copy.hook, &hook);
    assert_eq!(copy.attendance, Attendance::NoOneToAsk("a program's call"));
}
