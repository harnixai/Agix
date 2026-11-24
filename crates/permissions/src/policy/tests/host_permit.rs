//! A host tool whose declaration permits its calls: let through where no rule
//! matched, under every refusal, question and mode limit the policy keeps, and
//! only for a tool that is the host's.

use super::*;
use async_trait::async_trait;
use base::error::ToolError;
use base::interface::permission::{Attendance, HookAnswer};
use base::permission::PermissionMode;
use base::tool::ToolOrigin;
use serde_json::json;
use session_state::SessionState;
use std::path::PathBuf;

#[derive(Clone)]
struct Declared {
    origin: ToolOrigin,
    read_only: bool,
    permits: bool,
}

impl Declared {
    fn permitted() -> Self {
        Self {
            origin: ToolOrigin::Host,
            read_only: false,
            permits: true,
        }
    }
}

#[async_trait]
impl Tool for Declared {
    fn name(&self) -> &str {
        "HostEcho"
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
            .origin(self.origin.clone())
            .read_only(self.read_only)
    }
    fn description(&self) -> &str {
        "a host's tool"
    }
    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        json!({"type": "object"})
    }
    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        let facts = PermissionFacts::new(Subject::None, Reach::Unknown);
        match self.permits {
            true => facts.permitted_by_host(),
            false => facts,
        }
    }
    async fn call(
        &self,
        _: Value,
        _: ToolContext,
        _: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, ToolError> {
        Ok(base::tool::ToolResult::text("ok"))
    }
}

struct HostAnswers(PermissionOutcome);

#[async_trait]
impl PermissionPolicy for HostAnswers {
    async fn decide(&self, _: &PermissionRequest<'_>) -> PermissionOutcome {
        self.0.clone()
    }
}

fn ctx_in(mode: PermissionMode) -> ToolContext {
    let mut ctx = session_state::testing::tool_context_for_test(
        base::interface::exec::ExecProviders::in_process(),
        PathBuf::from("/tmp"),
    );
    ctx.session =
        Arc::new(SessionState::for_tests(PathBuf::from("/tmp")).with_permission_mode(mode));
    ctx
}

async fn decide_with(
    gate: &Policy,
    tool: &Declared,
    mode: PermissionMode,
    hook: &HookAnswer,
    attendance: Attendance,
) -> PermissionOutcome {
    let ctx = ctx_in(mode);
    let tool: Arc<dyn Tool> = Arc::new(tool.clone());
    let input = json!({});
    let facts = ToolFacts::gather(tool.as_ref(), &input, &ctx).await;
    let req = PermissionRequest::new(tool.name(), Some(&tool), &input, &facts, &ctx, &ctx.session)
        .with_hook(hook)
        .with_attendance(attendance);
    gate.decide(&req).await
}

async fn decide(gate: &Policy, tool: &Declared, mode: PermissionMode) -> PermissionOutcome {
    decide_with(gate, tool, mode, &HookAnswer::None, Attendance::Attended).await
}

fn rule(source: RuleSource, behavior: RuleBehavior) -> PermissionRule {
    PermissionRule {
        source,
        behavior,
        tool_name: "HostEcho".into(),
        rule_content: None,
    }
}

fn basis(outcome: &PermissionOutcome) -> Option<&Basis> {
    match outcome {
        PermissionOutcome::Permit { basis, .. } => Some(basis),
        _ => None,
    }
}

#[tokio::test]
async fn the_declaration_lets_the_call_through_on_its_own_basis() {
    let tool = Declared::permitted();
    let gate = Policy::empty();
    for attendance in [Attendance::Attended, Attendance::Unattended] {
        let d = decide_with(
            &gate,
            &tool,
            PermissionMode::Default,
            &HookAnswer::None,
            attendance,
        )
        .await;
        assert_eq!(
            basis(&d),
            Some(&Basis::HostDeclared),
            "{attendance:?}: {d:?}"
        );
    }
    assert_eq!(Basis::HostDeclared.as_str(), "host_declared");

    let asked = Declared {
        permits: false,
        ..tool
    };
    let d = decide(&gate, &asked, PermissionMode::Default).await;
    assert!(matches!(d, PermissionOutcome::Prompt { .. }), "{d:?}");
    let d = decide_with(
        &gate,
        &asked,
        PermissionMode::Default,
        &HookAnswer::None,
        Attendance::Unattended,
    )
    .await;
    assert!(matches!(d, PermissionOutcome::Deny { .. }), "{d:?}");
}

#[tokio::test]
async fn a_rule_that_matches_is_reported_instead_of_the_declaration() {
    let allow = rule(RuleSource::UserSettings, RuleBehavior::Allow);
    let gate = Policy::new(RuleSet::new(vec![allow.clone()]));
    let d = decide(&gate, &Declared::permitted(), PermissionMode::Default).await;
    assert_eq!(basis(&d), Some(&Basis::Rule(allow)), "{d:?}");
}

#[tokio::test]
async fn deny_and_ask_rules_of_every_tier_beat_the_declaration() {
    let tool = Declared::permitted();
    for source in [RuleSource::UserSettings, RuleSource::PolicySettings] {
        let deny = rule(source, RuleBehavior::Deny);
        let gate = Policy::new(RuleSet::new(vec![deny.clone()])).with_managed(
            ManagedLayer::of_rules(match source {
                RuleSource::PolicySettings => vec![deny],
                _ => vec![],
            }),
        );
        let d = decide(&gate, &tool, PermissionMode::Default).await;
        assert!(
            matches!(d, PermissionOutcome::Deny { .. }),
            "{source:?}: {d:?}"
        );

        let ask = rule(source, RuleBehavior::Ask);
        let gate = Policy::new(RuleSet::new(vec![ask.clone()])).with_managed(
            ManagedLayer::of_rules(match source {
                RuleSource::PolicySettings => vec![ask],
                _ => vec![],
            }),
        );
        let d = decide(&gate, &tool, PermissionMode::Default).await;
        assert!(
            matches!(d, PermissionOutcome::Prompt { .. }),
            "{source:?}: {d:?}"
        );
        let d = decide_with(
            &gate,
            &tool,
            PermissionMode::Default,
            &HookAnswer::None,
            Attendance::Unattended,
        )
        .await;
        assert!(
            matches!(d, PermissionOutcome::Deny { .. }),
            "{source:?}: {d:?}"
        );
    }
}

#[tokio::test]
async fn the_hosts_policy_and_a_hooks_block_beat_the_declaration() {
    let tool = Declared::permitted();
    let refusing = Policy::empty().with_host(Some(Arc::new(HostAnswers(PermissionOutcome::deny(
        Stage::Rule,
        "no",
    )))));
    let d = decide(&refusing, &tool, PermissionMode::Default).await;
    assert!(matches!(d, PermissionOutcome::Deny { .. }), "{d:?}");

    let asking = Policy::empty().with_host(Some(Arc::new(HostAnswers(PermissionOutcome::prompt(
        Stage::Rule,
        "really?",
    )))));
    let d = decide(&asking, &tool, PermissionMode::Default).await;
    assert!(matches!(d, PermissionOutcome::Prompt { .. }), "{d:?}");

    let blocked = HookAnswer::Block {
        hook: "PreToolUse".into(),
        reason: "no".into(),
    };
    let d = decide_with(
        &Policy::empty(),
        &tool,
        PermissionMode::Default,
        &blocked,
        Attendance::Attended,
    )
    .await;
    match d {
        PermissionOutcome::Deny { reason } => assert_eq!(reason.stage, Stage::Hook),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_modes_limits_hold_and_dont_ask_counts_it_as_a_rule() {
    let gate = Policy::empty();
    let writes = Declared::permitted();
    match decide(&gate, &writes, PermissionMode::Plan).await {
        PermissionOutcome::Deny { reason } => assert_eq!(reason.stage, Stage::PlanMode),
        other => panic!("{other:?}"),
    }
    let reads = Declared {
        read_only: true,
        ..writes.clone()
    };
    let d = decide(&gate, &reads, PermissionMode::Plan).await;
    assert_eq!(basis(&d), Some(&Basis::HostDeclared), "{d:?}");

    let d = decide(&gate, &writes, PermissionMode::DontAsk).await;
    assert_eq!(basis(&d), Some(&Basis::HostDeclared), "{d:?}");
    let unpermitted = Declared {
        permits: false,
        ..writes.clone()
    };
    let d = decide(&gate, &unpermitted, PermissionMode::DontAsk).await;
    assert!(matches!(d, PermissionOutcome::Deny { .. }), "{d:?}");

    let d = decide(&gate, &writes, PermissionMode::BypassPermissions).await;
    assert_eq!(basis(&d), Some(&Basis::Bypass), "{d:?}");
}

#[tokio::test]
async fn a_tool_that_is_not_the_hosts_cannot_claim_the_declaration() {
    let gate = Policy::empty();
    for origin in [
        ToolOrigin::Builtin,
        ToolOrigin::plugin("demo"),
        ToolOrigin::mcp("srv"),
    ] {
        let spoof = Declared {
            origin: origin.clone(),
            ..Declared::permitted()
        };
        let d = decide(&gate, &spoof, PermissionMode::Default).await;
        assert!(
            matches!(d, PermissionOutcome::Prompt { .. }),
            "{origin:?}: {d:?}"
        );
        let d = decide(&gate, &spoof, PermissionMode::DontAsk).await;
        assert!(
            matches!(d, PermissionOutcome::Deny { .. }),
            "{origin:?}: {d:?}"
        );
    }
}
