use super::*;
use base::interface::exec::ExecProviders;
use base::tool::ToolContext;

trait CheckWith {
    async fn check_with(
        &self,
        tool: &FakeTool,
        input: &Value,
        ctx: &ToolContext,
    ) -> Result<PermissionOutcome, std::convert::Infallible>;
}

impl CheckWith for Policy {
    async fn check_with(
        &self,
        tool: &FakeTool,
        input: &Value,
        ctx: &ToolContext,
    ) -> Result<PermissionOutcome, std::convert::Infallible> {
        let tool: Arc<dyn Tool> = Arc::new(tool.clone());
        let facts = ToolFacts::gather(tool.as_ref(), input, ctx).await;
        let req =
            PermissionRequest::new(tool.name(), Some(&tool), input, &facts, ctx, &ctx.session);
        Ok(self.decide(&req).await)
    }
}
use async_trait::async_trait;
use base::error::ToolError;
use base::interface::permission::Attendance;
use base::permission::PermissionMode;
use base::tool::Tool;
use serde_json::json;
use session_state::SessionState;
use std::path::PathBuf;
use std::sync::Arc;

fn parsed(text: &str, behavior: RuleBehavior) -> PermissionRule {
    crate::rule::parse_rule_string(text, RuleSource::ProjectSettings, behavior).unwrap()
}

fn rule(source: base::permission::RuleSource, tool: &str) -> PermissionRule {
    PermissionRule {
        source,
        behavior: base::permission::RuleBehavior::Allow,
        tool_name: tool.into(),
        rule_content: None,
    }
}

/// Unloading a plugin must take its rules with it — `RuleSource::Plugin`
/// exists so that removal is a single call rather than a diff against
/// whatever the plugin happened to add.
#[test]
fn plugin_rules_are_removable_as_a_batch_without_touching_others() {
    let gate = Policy::new(RuleSet::new(vec![rule(
        base::permission::RuleSource::UserSettings,
        "Read",
    )]));
    gate.add_rules(vec![
        rule(base::permission::RuleSource::Plugin, "Grep"),
        rule(base::permission::RuleSource::Plugin, "Glob"),
    ]);
    assert_eq!(gate.rules().len(), 3);

    gate.remove_rules_by_source(base::permission::RuleSource::Plugin);

    let remaining = gate.rules();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].tool_name, "Read");
}

/// A plugin is third-party content pulled off the network; its allow
/// must never open up what the user's own settings or the org's managed
/// policy deny.
#[test]
fn a_plugin_allow_never_overrides_a_deny_from_any_source() {
    use base::permission::{RuleBehavior, RuleSource};
    for denier in [
        RuleSource::PolicySettings,
        RuleSource::UserSettings,
        RuleSource::ProjectSettings,
    ] {
        let rules = RuleSet::new(vec![
            PermissionRule {
                source: denier,
                behavior: RuleBehavior::Deny,
                tool_name: "Bash".into(),
                rule_content: None,
            },
            PermissionRule {
                source: RuleSource::Plugin,
                behavior: RuleBehavior::Allow,
                tool_name: "Bash".into(),
                rule_content: Some("curl:*".into()),
            },
        ]);
        assert!(
            matches!(rules.evaluate("Bash", Some("curl x")), RuleHit::Deny(_)),
            "plugin allow overrode a {denier:?} deny"
        );
    }
}

/// A tool whose every answer the test sets.
#[derive(Clone)]
struct FakeTool {
    name: &'static str,
    read_only: bool,
    own: Own,
    match_content: Option<String>,
    invalid: bool,
    leaves_plan: bool,
    shell: bool,
}

/// What the tool says about its own call.
#[derive(Clone)]
enum Own {
    Contained,
    ListedHost,
    EditsProject,
    Refused(&'static str),
    Unknown,
}

impl FakeTool {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            read_only: false,
            own: Own::Unknown,
            match_content: None,
            invalid: false,
            leaves_plan: false,
            shell: false,
        }
    }
    fn leaves_plan(mut self) -> Self {
        self.leaves_plan = true;
        self
    }
    fn invalid(mut self) -> Self {
        self.invalid = true;
        self
    }
    fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
    fn contained(mut self) -> Self {
        self.own = Own::Contained;
        self
    }
    fn listed_host(mut self) -> Self {
        self.own = Own::ListedHost;
        self
    }
    fn edits_project(mut self) -> Self {
        self.own = Own::EditsProject;
        self
    }
    fn refused(mut self, reason: &'static str) -> Self {
        self.own = Own::Refused(reason);
        self
    }
    fn matches(mut self, c: &str) -> Self {
        self.match_content = Some(c.into());
        self
    }
    fn shell(mut self) -> Self {
        self.shell = true;
        self
    }
}

#[async_trait]
impl Tool for FakeTool {
    fn name(&self) -> &str {
        self.name
    }
    fn spec(&self) -> base::tool::ToolSpec {
        let stance = if self.leaves_plan {
            ModeStance::LeavesPlan
        } else {
            ModeStance::Normal
        };
        base::tool::ToolSpec::new()
            .read_only(self.read_only)
            .stance(stance)
    }

    fn description(&self) -> &str {
        "fake tool (test tool)"
    }

    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        json!({"type":"object"})
    }
    async fn guide(&self, _: &base::tool::PromptContext) -> Option<String> {
        Some("fake".into())
    }
    async fn validate_input(
        &self,
        _: &Value,
        _: &base::tool::ToolContext,
    ) -> base::tool::ValidationResult {
        if self.invalid {
            base::tool::ValidationResult::err("bad input", 1)
        } else {
            base::tool::ValidationResult::Ok
        }
    }
    async fn permission_facts(&self, _: &Value, _: &base::tool::ToolContext) -> PermissionFacts {
        let subject = match (&self.match_content, self.shell) {
            (None, _) => Subject::None,
            (Some(c), true) => Subject::Shell(c.clone()),
            (Some(c), false) => Subject::Opaque(c.clone()),
        };
        match &self.own {
            Own::Contained if self.shell => PermissionFacts::new(subject, Reach::Inside),
            Own::Contained => PermissionFacts::new(subject, Reach::SessionOnly),
            Own::ListedHost => PermissionFacts::new(subject, Reach::Host { listed: true }),
            Own::EditsProject => PermissionFacts::new(subject, Reach::EditsProject),
            Own::Refused(reason) => {
                PermissionFacts::new(subject, Reach::Refused(reason.to_string()))
            }
            Own::Unknown => PermissionFacts::new(subject, Reach::Unknown).with_question("?"),
        }
    }
    async fn call(
        &self,
        _: Value,
        _: base::tool::ToolContext,
        _: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, ToolError> {
        Ok(base::tool::ToolResult::text("ok"))
    }
}

fn ctx_with_mode(mode: PermissionMode) -> base::tool::ToolContext {
    let mut ctx = session_state::testing::tool_context_for_test(
        ExecProviders::in_process(),
        PathBuf::from("/tmp"),
    );
    ctx.session =
        Arc::new(SessionState::for_tests(PathBuf::from("/tmp")).with_permission_mode(mode));
    ctx.tool_use_id = "test".into();
    ctx
}

/// A user's more specific ask matching too must not hide that the
/// managed layer asks: that is what keeps a hook from answering it.
#[tokio::test]
async fn an_ask_the_managed_layer_makes_is_reported_as_its_own() {
    let ask = |source, content: &str| PermissionRule {
        source,
        behavior: RuleBehavior::Ask,
        tool_name: "X".into(),
        rule_content: Some(content.into()),
    };
    let managed = ask(RuleSource::PolicySettings, "touch:*");
    let user = ask(RuleSource::UserSettings, "touch /tmp/a");
    let tool = FakeTool::new("X").matches("touch /tmp/a");
    let decide = |rules: Vec<PermissionRule>, policy: Vec<PermissionRule>| {
        let gate = Policy::new(RuleSet::new(rules)).with_managed(ManagedLayer::of_rules(policy));
        let tool = &tool;
        async move {
            match gate
                .check_with(tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
                .await
                .unwrap()
            {
                PermissionOutcome::Prompt { answerable, .. } => {
                    answerable == Answerable::PersonOnly
                }
                other => panic!("expected an ask, got {other:?}"),
            }
        }
    };

    assert!(decide(vec![managed.clone(), user.clone()], vec![managed.clone()]).await);
    assert!(!decide(vec![user], vec![]).await);

    let bypass = Policy::new(RuleSet::new(vec![managed.clone()]))
        .with_managed(ManagedLayer::of_rules(vec![managed]));
    let d = bypass
        .check_with(
            &tool,
            &json!({}),
            &ctx_with_mode(PermissionMode::BypassPermissions),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            &d,
            PermissionOutcome::Prompt {
                answerable: Answerable::PersonOnly,
                ..
            }
        ),
        "{d:?}"
    );
}

#[tokio::test]
async fn tool_own_allow_is_honoured_when_nothing_else_objects() {
    // 工具自判 Allow 仍然是"不弹窗"的快速通道 —— 只是不再短路：这里没有
    // deny 规则、没踩 bypass-immune 路径、模式也是 Default，所以兑现。
    let tool = FakeTool::new("X").contained();
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    match d {
        PermissionOutcome::Permit { basis, .. } => assert_eq!(basis, Basis::Contained),
        other => panic!("expected Allow(tool allowed), got {other:?}"),
    }
}

/// A contained write that names configuration — the instance's own project
/// directory outside its working space (`state/scratchpad`, `state/memory`),
/// or a foreign root — is never let
/// through silently, whatever the instance calls its directory.
#[tokio::test]
async fn a_contained_write_to_configuration_is_not_silent_in_either_layout() {
    for name in [".agix", ".codix"] {
        let layout = base::paths::Layout::new(name).unwrap();
        let silent = |content: String| {
            let layout = layout.clone();
            async move {
                let tool = FakeTool::new("Write").contained().matches(&content);
                let mut ctx = ctx_with_mode(PermissionMode::Default);
                in_layout(&mut ctx, &layout);
                matches!(
                    Policy::empty()
                        .check_with(&tool, &json!({}), &ctx)
                        .await
                        .unwrap(),
                    PermissionOutcome::Permit {
                        basis: Basis::Contained,
                        ..
                    }
                )
            }
        };
        assert!(!silent(format!("/tmp/{name}/agents/r.md")).await, "{name}");
        assert!(!silent("/tmp/.agix/hooks/x.sh".into()).await, "{name}");
        assert!(!silent("/tmp/.claude/settings.json".into()).await, "{name}");
        assert!(!silent(format!("/tmp/{name}/scratchpad/x")).await, "{name}");
        assert!(
            !silent(format!("/tmp/{name}/state/tool-results/s/x")).await,
            "{name}"
        );
        assert!(!silent(format!("/tmp/{name}/state/agix/x")).await, "{name}");
        assert!(
            silent(format!("/tmp/{name}/state/scratchpad/x")).await,
            "{name}"
        );
        assert!(
            silent(format!("/tmp/{name}/state/memory/m.md")).await,
            "{name}"
        );
    }
}

/// 用户在 settings.json 里写的 deny 规则必须压过工具的自判 Allow。
#[tokio::test]
async fn deny_rule_beats_self_allowing_tool() {
    let tool = FakeTool::new("Write")
        .contained()
        .matches("/repo/secrets.env");
    let rules = RuleSet::new(vec![PermissionRule {
        source: base::permission::RuleSource::UserSettings,
        behavior: base::permission::RuleBehavior::Deny,
        tool_name: "Write".into(),
        rule_content: Some("/repo/secrets.env".into()),
    }]);
    let gate = Policy::new(rules);
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    assert!(
        matches!(d, PermissionOutcome::Deny { .. }),
        "deny rule must outrank the tool's own Allow, got {d:?}"
    );
}

/// Plan 模式必须拦住自判 Allow 的非只读工具（典型就是项目内的
/// `Write` / `Edit`）。
#[tokio::test]
async fn plan_mode_denies_self_allowing_non_readonly_tool() {
    let tool = FakeTool::new("Write").contained();
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Plan))
        .await
        .unwrap();
    assert!(
        matches!(d, PermissionOutcome::Deny { .. }),
        "plan mode must outrank the tool's own Allow, got {d:?}"
    );
}

/// bypass-immune 路径必须压过工具自判 Allow —— 否则
/// `Read(~/.ssh/id_rsa)` 在第 2 步就返回了，第 4 步的检查永远跑不到。
#[tokio::test]
async fn bypass_immune_path_beats_self_allowing_tool() {
    let tool = FakeTool::new("Read")
        .read_only()
        .contained()
        .matches("~/.ssh/id_rsa");
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    match d {
        PermissionOutcome::Deny { ref reason } => assert_eq!(reason.stage, Stage::Credentials),
        other => panic!("expected a credentials Deny, got {other:?}"),
    }
}

#[tokio::test]
async fn credentials_are_refused_in_every_mode() {
    let state = tempfile::tempdir().unwrap();
    let token = state.path().join("mcp/oauth/x.json");
    for (content, mode, shell) in [
        (
            token.display().to_string(),
            PermissionMode::BypassPermissions,
            false,
        ),
        (
            token.display().to_string(),
            PermissionMode::AcceptEdits,
            false,
        ),
        (
            format!("cat '{}/Settings.json'", state.path().display()),
            PermissionMode::BypassPermissions,
            true,
        ),
        (
            "rg key .agix/settings.local.json".to_string(),
            PermissionMode::Default,
            true,
        ),
        (
            format!("--from={}/daemon/auth/x.token", state.path().display()),
            PermissionMode::Default,
            false,
        ),
    ] {
        let tool = FakeTool::new("Read")
            .read_only()
            .contained()
            .matches(&content);
        let tool = if shell { tool.shell() } else { tool };
        let mut ctx = ctx_with_mode(mode);
        ctx.sandbox_mut().paths = std::sync::Arc::new(SensitivePaths::new(
            Some(state.path()),
            None,
            Default::default(),
        ));
        let d = Policy::empty()
            .check_with(&tool, &json!({}), &ctx)
            .await
            .unwrap();
        assert!(
            matches!(
                d,
                PermissionOutcome::Deny { ref reason } if reason.stage == Stage::Credentials
            ),
            "{content} under {mode:?}: {d:?}"
        );
    }
}

#[tokio::test]
async fn the_rest_of_the_state_root_is_not_a_credential() {
    let state = tempfile::tempdir().unwrap();
    let tool = FakeTool::new("Read")
        .read_only()
        .contained()
        .matches(&state.path().join("skills/x/SKILL.md").display().to_string());
    let mut ctx = ctx_with_mode(PermissionMode::Default);
    ctx.sandbox_mut().paths = std::sync::Arc::new(SensitivePaths::new(
        Some(state.path()),
        None,
        Default::default(),
    ));
    let d = Policy::empty()
        .check_with(&tool, &json!({}), &ctx)
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Permit { .. }), "{d:?}");
}

/// `DontAsk` 是"没有规则就拒绝"，自判 Allow 不该把它变成"什么都放行"。
#[tokio::test]
async fn dontask_mode_denies_self_allowing_tool_without_rule() {
    let tool = FakeTool::new("Write").contained();
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::DontAsk))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Deny { .. }));
}

#[tokio::test]
async fn tool_own_deny_short_circuits() {
    let tool = FakeTool::new("X").refused("no");
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Deny { .. }));
}

#[tokio::test]
async fn rule_engine_allow_overrides_default_ask() {
    let tool = FakeTool::new("Bash").matches("ls");
    let rules = RuleSet::new(vec![PermissionRule {
        source: base::permission::RuleSource::UserSettings,
        behavior: base::permission::RuleBehavior::Allow,
        tool_name: "Bash".into(),
        rule_content: Some("ls".into()),
    }]);
    let gate = Policy::new(rules);
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Permit { .. }));
}

#[tokio::test]
async fn rule_engine_deny_short_circuits() {
    let tool = FakeTool::new("Bash").matches("rm -rf /");
    let rules = RuleSet::new(vec![PermissionRule {
        source: base::permission::RuleSource::UserSettings,
        behavior: base::permission::RuleBehavior::Deny,
        tool_name: "Bash".into(),
        rule_content: Some("rm -rf:*".into()),
    }]);
    let gate = Policy::new(rules);
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Deny { .. }));
}

#[tokio::test]
async fn bypass_mode_consults_no_rule_no_protected_path_and_no_tool_verdict() {
    for spelling in ["bypassPermissions", "auto", "yolo"] {
        bypass_spelling_consults_nothing(spelling).await;
    }
}

async fn bypass_spelling_consults_nothing(spelling: &str) {
    let deny_bash = || {
        RuleSet::new(vec![PermissionRule {
            source: base::permission::RuleSource::UserSettings,
            behavior: base::permission::RuleBehavior::Deny,
            tool_name: "Bash".into(),
            rule_content: None,
        }])
    };
    let mode: PermissionMode = serde_json::from_value(json!(spelling)).unwrap();
    let ctx = ctx_with_mode(mode);
    let cases = [
        FakeTool::new("Bash").matches("rm -rf build"),
        FakeTool::new("Read").read_only().matches("/etc/passwd"),
        FakeTool::new("Bash").matches("rm -rf /"),
        FakeTool::new("Write")
            .matches("/repo/.agix/settings.json")
            .refused("guarded"),
    ];
    for tool in cases {
        let d = Policy::new(deny_bash())
            .check_with(&tool, &json!({}), &ctx)
            .await
            .unwrap();
        assert!(
            matches!(
                d,
                PermissionOutcome::Permit {
                    basis: Basis::Bypass,
                    ..
                }
            ),
            "{spelling} {:?}: {d:?}",
            tool.match_content
        );
    }
}

/// A delegation's own deny rules hold its delegate's calls in any mode, and
/// only its delegate's: the session relaying them is not held to them.
#[tokio::test]
async fn a_delegations_deny_rules_refuse_its_calls_in_any_mode_and_nothing_else() {
    let deny_bash = [PermissionRule {
        source: base::permission::RuleSource::Session,
        behavior: base::permission::RuleBehavior::Deny,
        tool_name: "Bash".into(),
        rule_content: None,
    }];
    let bash: Arc<dyn Tool> = Arc::new(FakeTool::new("Bash").matches("ls"));
    let input = json!({});
    for mode in [PermissionMode::BypassPermissions, PermissionMode::Default] {
        let ctx = ctx_with_mode(mode);
        let delegate: Arc<dyn Session> =
            Arc::new(SessionState::for_tests(PathBuf::from("/tmp")).with_permission_mode(mode));
        let facts = ToolFacts::gather(bash.as_ref(), &input, &ctx).await;
        let request =
            PermissionRequest::new("Bash", Some(&bash), &input, &facts, &ctx, &ctx.session);
        let delegated = request.for_delegate(base::interface::permission::Delegate::new(
            &delegate, mode, &deny_bash,
        ));
        match Policy::empty().decide(&delegated).await {
            PermissionOutcome::Deny { reason } => {
                assert_eq!(reason.stage, Stage::DelegateRule, "{mode:?}: {reason}")
            }
            other => panic!("{mode:?}: {other:?}"),
        }
        if mode == PermissionMode::BypassPermissions {
            assert!(matches!(
                Policy::empty().decide(&request).await,
                PermissionOutcome::Permit { .. }
            ));
        }
    }
}

/// Input validation is the caller's, once, before it asks: the policy
/// judges what the tool says about the call, not whether the call parses.
#[tokio::test]
async fn the_policy_leaves_input_validation_to_its_caller() {
    let tool = FakeTool::new("Write").invalid();
    let r = Policy::empty()
        .check_with(
            &tool,
            &json!({}),
            &ctx_with_mode(PermissionMode::BypassPermissions),
        )
        .await;
    assert!(
        matches!(
            &r,
            Ok(PermissionOutcome::Permit {
                basis: Basis::Bypass,
                ..
            })
        ),
        "{r:?}"
    );
}

#[tokio::test]
async fn bypass_mode_allows_when_no_rule() {
    let tool = FakeTool::new("Whatever");
    let gate = Policy::empty();
    let d = gate
        .check_with(
            &tool,
            &json!({}),
            &ctx_with_mode(PermissionMode::BypassPermissions),
        )
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Permit { .. }));
}

#[tokio::test]
async fn plan_mode_denies_non_readonly() {
    let tool = FakeTool::new("Write");
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Plan))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Deny { .. }));
}

#[tokio::test]
async fn the_way_out_of_plan_mode_is_asked_about_or_decided_by_a_rule() {
    let exit = || FakeTool::new("ExitPlanMode").leaves_plan();
    let plan = ctx_with_mode(PermissionMode::Plan);
    let asked = Policy::empty()
        .check_with(&exit(), &json!({}), &plan)
        .await
        .unwrap();
    assert!(
        matches!(asked, PermissionOutcome::Prompt { .. }),
        "{asked:?}"
    );

    let rule = |behavior| PermissionRule {
        source: base::permission::RuleSource::UserSettings,
        behavior,
        tool_name: "ExitPlanMode".into(),
        rule_content: None,
    };
    let allowed = Policy::new(RuleSet::new(vec![rule(
        base::permission::RuleBehavior::Allow,
    )]))
    .check_with(&exit(), &json!({}), &plan)
    .await
    .unwrap();
    assert!(
        matches!(allowed, PermissionOutcome::Permit { .. }),
        "{allowed:?}"
    );
    let denied = Policy::new(RuleSet::new(vec![rule(
        base::permission::RuleBehavior::Deny,
    )]))
    .check_with(&exit(), &json!({}), &plan)
    .await
    .unwrap();
    assert!(
        matches!(denied, PermissionOutcome::Deny { .. }),
        "{denied:?}"
    );

    let write = Policy::empty()
        .check_with(&FakeTool::new("Write"), &json!({}), &plan)
        .await
        .unwrap();
    assert!(matches!(write, PermissionOutcome::Deny { .. }), "{write:?}");
}

#[tokio::test]
async fn plan_mode_asks_for_a_read_outside_the_project_as_default_mode_does() {
    let inside = FakeTool::new("Read")
        .read_only()
        .matches("/tmp/a.txt")
        .contained();
    let outside = FakeTool::new("Read")
        .read_only()
        .matches("/elsewhere/a.txt");
    for mode in [PermissionMode::Plan, PermissionMode::Default] {
        let ctx = ctx_with_mode(mode);
        let gate = Policy::empty();
        let d = gate.check_with(&inside, &json!({}), &ctx).await.unwrap();
        assert!(
            matches!(d, PermissionOutcome::Permit { .. }),
            "{mode:?}: {d:?}"
        );
        let d = gate.check_with(&outside, &json!({}), &ctx).await.unwrap();
        assert!(
            matches!(d, PermissionOutcome::Prompt { .. }),
            "{mode:?}: {d:?}"
        );
    }
}

#[tokio::test]
async fn accept_edits_allows_a_write_the_tool_allowed_and_asks_for_one_it_did_not() {
    let ctx = ctx_with_mode(PermissionMode::AcceptEdits);
    let inside = FakeTool::new("Write").matches("/tmp/a.txt").contained();
    let d = Policy::empty()
        .check_with(&inside, &json!({}), &ctx)
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Permit { .. }), "{d:?}");

    for tool in [
        FakeTool::new("Write").matches("/elsewhere/a.txt"),
        FakeTool::new("Edit").matches("/elsewhere/a.txt"),
        FakeTool::new("Write")
            .matches("/tmp/.agix/hooks/x.sh")
            .contained(),
    ] {
        let d = Policy::empty()
            .check_with(&tool, &json!({}), &ctx)
            .await
            .unwrap();
        assert!(
            matches!(d, PermissionOutcome::Prompt { .. }),
            "{:?}: {d:?}",
            tool.match_content
        );
    }
}

/// A plugin command that writes is asked about in `default`, let through by
/// `acceptEdits`, refused by `plan`; an allow rule or a deny rule on its
/// command line decides it in any of them.
#[tokio::test]
async fn a_program_editing_the_project_is_asked_about_unless_edits_are_accepted() {
    let tool = FakeTool::new("RunExt")
        .matches("forge flow archive C-1")
        .edits_project();
    let outcome = |mode| {
        let tool = tool.clone();
        async move {
            Policy::empty()
                .check_with(&tool, &json!({}), &ctx_with_mode(mode))
                .await
                .unwrap()
        }
    };
    assert!(matches!(
        outcome(PermissionMode::Default).await,
        PermissionOutcome::Prompt { .. }
    ));
    assert!(matches!(
        outcome(PermissionMode::AcceptEdits).await,
        PermissionOutcome::Permit { .. }
    ));
    assert!(matches!(
        outcome(PermissionMode::Plan).await,
        PermissionOutcome::Deny { .. }
    ));
    let ctx = ctx_with_mode(PermissionMode::Default);
    let allowed = Policy::new(RuleSet::new(vec![parsed(
        "RunExt(forge flow:*)",
        RuleBehavior::Allow,
    )]))
    .check_with(&tool, &json!({}), &ctx)
    .await
    .unwrap();
    assert!(
        matches!(allowed, PermissionOutcome::Permit { .. }),
        "{allowed:?}"
    );
    let ctx = ctx_with_mode(PermissionMode::AcceptEdits);
    let denied = Policy::new(RuleSet::new(vec![parsed(
        "RunExt(forge flow archive:*)",
        RuleBehavior::Deny,
    )]))
    .check_with(&tool, &json!({}), &ctx)
    .await
    .unwrap();
    assert!(
        matches!(denied, PermissionOutcome::Deny { .. }),
        "{denied:?}"
    );
}

/// A read that reaches only a host the settings list is let through; a
/// read that claims nothing is asked about (SEC F-6).
#[tokio::test]
async fn accept_edits_allows_a_read_to_a_listed_host_but_not_one_that_claims_nothing() {
    let ctx = ctx_with_mode(PermissionMode::AcceptEdits);
    let listed = FakeTool::new("WebFetch")
        .read_only()
        .matches("https://docs.rs")
        .listed_host();
    let d = Policy::empty()
        .check_with(&listed, &json!({}), &ctx)
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Permit { .. }), "{d:?}");

    let silent = FakeTool::new("Read")
        .read_only()
        .matches("/elsewhere/a.txt");
    let d = Policy::empty()
        .check_with(&silent, &json!({}), &ctx)
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Prompt { .. }), "{d:?}");
}

#[tokio::test]
async fn dontask_mode_denies_all_unmatched_tools() {
    let tool = FakeTool::new("Bash");
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::DontAsk))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Deny { .. }));
}

#[tokio::test]
async fn dontask_mode_with_allow_rule_permits() {
    let tool = FakeTool::new("Read").read_only();
    let rules = RuleSet::new(vec![PermissionRule {
        source: base::permission::RuleSource::UserSettings,
        behavior: base::permission::RuleBehavior::Allow,
        tool_name: "Read".into(),
        rule_content: None,
    }]);
    let gate = Policy::new(rules);
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::DontAsk))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Permit { .. }));
}

#[test]
fn dontask_mode_parses_from_config() {
    let mode: PermissionMode = serde_json::from_str("\"dontAsk\"").unwrap();
    assert_eq!(mode, PermissionMode::DontAsk);
}

#[tokio::test]
async fn default_mode_asks_when_no_rule() {
    let tool = FakeTool::new("Bash");
    let gate = Policy::empty();
    let d = gate
        .check_with(&tool, &json!({}), &ctx_with_mode(PermissionMode::Default))
        .await
        .unwrap();
    assert!(matches!(d, PermissionOutcome::Prompt { .. }));
}

async fn delegates_call(
    policy: &Policy,
    content: &str,
    mode: PermissionMode,
    rules: Option<&[PermissionRule]>,
) -> PermissionOutcome {
    delegates_call_with(policy, content, mode, rules, Attendance::Attended).await
}

async fn delegates_call_with(
    policy: &Policy,
    content: &str,
    mode: PermissionMode,
    rules: Option<&[PermissionRule]>,
    attendance: Attendance,
) -> PermissionOutcome {
    let tool: Arc<dyn Tool> = Arc::new(FakeTool::new("X").matches(content));
    let ctx = ctx_with_mode(mode);
    let delegate: Arc<dyn Session> =
        Arc::new(SessionState::for_tests(PathBuf::from("/tmp")).with_permission_mode(mode));
    let input = json!({});
    let facts = ToolFacts::gather(tool.as_ref(), &input, &ctx).await;
    let request = PermissionRequest::new("X", Some(&tool), &input, &facts, &ctx, &ctx.session)
        .with_attendance(attendance);
    match rules {
        Some(rules) => {
            let delegated = request.for_delegate(base::interface::permission::Delegate::new(
                &delegate, mode, rules,
            ));
            policy.decide(&delegated).await
        }
        None => policy.decide(&request).await,
    }
}

/// A delegate's grants answer the question its mode would put, and nothing
/// else: a deny rule, an ask rule, a managed ask and `dontAsk` all stand, a
/// call the grant does not name is still asked about, and so is the same
/// call made by the session that relays it.
#[tokio::test]
async fn a_delegates_grants_answer_only_the_modes_question() {
    let rule = |source, behavior| PermissionRule {
        source,
        behavior,
        tool_name: "X".into(),
        rule_content: Some("deploy staging".into()),
    };
    let grants = [rule(RuleSource::Command, RuleBehavior::Allow)];
    let staging = "deploy staging";
    let default = PermissionMode::Default;

    match delegates_call(&Policy::empty(), staging, default, Some(&grants)).await {
        PermissionOutcome::Permit { basis, .. } => {
            assert_eq!(basis, Basis::Rule(grants[0].clone()))
        }
        other => panic!("the grant did not answer the mode's question: {other:?}"),
    }
    assert!(matches!(
        delegates_call(&Policy::empty(), staging, default, None).await,
        PermissionOutcome::Prompt { .. }
    ));
    let empty = Policy::empty();
    let unattended =
        |rules| delegates_call_with(&empty, staging, default, rules, Attendance::Unattended);
    assert!(matches!(
        unattended(Some(&grants)).await,
        PermissionOutcome::Permit { .. }
    ));
    assert!(matches!(
        unattended(None).await,
        PermissionOutcome::Deny { .. }
    ));
    assert!(matches!(
        delegates_call(&Policy::empty(), "deploy prod", default, Some(&grants)).await,
        PermissionOutcome::Prompt { .. }
    ));

    let denying = Policy::new(RuleSet::new(vec![rule(
        RuleSource::UserSettings,
        RuleBehavior::Deny,
    )]));
    assert!(matches!(
        delegates_call(&denying, staging, default, Some(&grants)).await,
        PermissionOutcome::Deny { .. }
    ));
    let asking = Policy::new(RuleSet::new(vec![rule(
        RuleSource::UserSettings,
        RuleBehavior::Ask,
    )]));
    assert!(matches!(
        delegates_call(&asking, staging, default, Some(&grants)).await,
        PermissionOutcome::Prompt { .. }
    ));
    let managed_ask = rule(RuleSource::PolicySettings, RuleBehavior::Ask);
    let managed = Policy::new(RuleSet::new(vec![managed_ask.clone()]))
        .with_managed(ManagedLayer::of_rules(vec![managed_ask]));
    assert!(matches!(
        delegates_call(&managed, staging, default, Some(&grants)).await,
        PermissionOutcome::Prompt {
            answerable: Answerable::PersonOnly,
            ..
        }
    ));
    assert!(matches!(
        delegates_call(
            &Policy::empty(),
            staging,
            PermissionMode::DontAsk,
            Some(&grants)
        )
        .await,
        PermissionOutcome::Deny { .. }
    ));
}

#[tokio::test]
async fn a_deny_rule_for_the_name_a_call_is_also_known_by_applies_but_an_allow_does_not() {
    use base::interface::permission::Reach;
    use base::permission::{RuleBehavior, RuleSource};
    let rule = |behavior| PermissionRule {
        source: RuleSource::UserSettings,
        behavior,
        tool_name: "mcp__corp".into(),
        rule_content: None,
    };
    let tool: Arc<dyn Tool> = Arc::new(FakeTool::new("ReadMcpResource"));
    let ctx = ctx_with_mode(PermissionMode::Default);
    let mut facts = ToolFacts::gather(tool.as_ref(), &json!({}), &ctx).await;
    facts.reach = Reach::Operator;
    facts.also_named = Some("mcp__corp".into());
    let input = json!({});
    let request =
        PermissionRequest::new(tool.name(), Some(&tool), &input, &facts, &ctx, &ctx.session);

    let denying = Policy::new(RuleSet::new(vec![rule(RuleBehavior::Deny)]));
    assert!(matches!(
        denying.decide(&request).await,
        PermissionOutcome::Deny { .. }
    ));

    let allowing = Policy::new(RuleSet::new(vec![rule(RuleBehavior::Allow)]));
    let outcome = allowing.decide(&request).await;
    assert!(
        matches!(outcome, PermissionOutcome::Permit { ref basis, .. } if *basis == Basis::Contained),
        "{outcome:?}"
    );
}

/// In a worktree of the project, a relative deny or ask path rule holds
/// under the worktree and under the project root both; an allow only under
/// the worktree.
#[test]
fn a_worktree_session_is_refused_by_relative_rules_under_the_project_too() {
    let project = PathBuf::from("/p");
    let worktree = project.join(".wkts/wt");
    let with = |behavior, tool: &str, content: Option<&str>| PermissionRule {
        source: RuleSource::ProjectSettings,
        behavior,
        tool_name: tool.into(),
        rule_content: content.map(Into::into),
    };
    let rules = RuleSet::new(vec![
        with(RuleBehavior::Allow, "Read", None),
        with(RuleBehavior::Deny, "Read", Some(".env")),
        with(RuleBehavior::Allow, "Edit", Some("src/**")),
        with(RuleBehavior::Ask, "Edit", Some("docs/**")),
    ]);
    let hit = |roots: Vec<PathBuf>, tool: &str, subject: Subject| {
        let mut ctx = session_state::testing::tool_context_for_test(
            ExecProviders::in_process(),
            worktree.clone(),
        );
        ctx.session = Arc::new(SessionState::for_tests_in(base::context::SessionEnv {
            refusal_rule_roots: roots,
            ..base::context::SessionEnv::new(worktree.clone())
        }));
        super::super::subject::Named::of(&subject, &ctx).hit(&rules, tool, &ctx)
    };
    let in_worktree = || vec![project.clone()];
    let read = |p: &str| Subject::ReadPath(p.into());
    let write = |p: &str| Subject::WritePath(p.into());

    assert!(matches!(
        hit(in_worktree(), "Read", read("/p/.env")),
        RuleHit::Deny(_)
    ));
    assert!(matches!(
        hit(in_worktree(), "Read", read("../../.env")),
        RuleHit::Deny(_)
    ));
    assert!(matches!(
        hit(in_worktree(), "Read", read(".env")),
        RuleHit::Deny(_)
    ));
    assert!(matches!(
        hit(in_worktree(), "Edit", write("/p/docs/a.md")),
        RuleHit::Ask(_)
    ));
    assert!(matches!(
        hit(in_worktree(), "Edit", write("src/a.rs")),
        RuleHit::Allow(_)
    ));
    assert!(matches!(
        hit(in_worktree(), "Edit", write("/p/src/a.rs")),
        RuleHit::None
    ));
    assert!(matches!(
        hit(Vec::new(), "Read", read("/p/.env")),
        RuleHit::Allow(_)
    ));
}

/// A deny or ask path rule anchored at the project, absolute or relative,
/// holds for the same path in a worktree under `<repository>/.wkts/`
/// from any session, and for a project in a subdirectory of its repository
/// where that subdirectory is in the worktree. An allow does not carry over.
#[test]
fn project_anchored_refusals_cover_a_worktrees_copy() {
    for name in [".agix", ".codix"] {
        refusals_cover_a_worktrees_copy_in(&base::paths::Layout::new(name).unwrap());
    }
}

/// Under `layout`, with its project directory spelled where the default
/// one is below.
fn refusals_cover_a_worktrees_copy_in(layout: &base::paths::Layout) {
    let w = |p: &str| p.replace(".agix", layout.project_dir_name());
    let with = |behavior, tool: &str, content: &str| PermissionRule {
        source: RuleSource::ProjectSettings,
        behavior,
        tool_name: tool.into(),
        rule_content: Some(content.into()),
    };
    let hit = |cwd: &str, roots: &[&str], rules: &RuleSet, tool: &str, subject: Subject| {
        let cwd = PathBuf::from(cwd);
        let mut ctx =
            session_state::testing::tool_context_for_test(ExecProviders::in_process(), cwd.clone());
        in_layout(&mut ctx, layout);
        ctx.session = Arc::new(SessionState::for_tests_in(base::context::SessionEnv {
            refusal_rule_roots: roots.iter().map(PathBuf::from).collect(),
            ..base::context::SessionEnv::new(cwd)
        }));
        super::super::subject::Named::of(&subject, &ctx).hit(rules, tool, &ctx)
    };
    let read = |p: &str| Subject::ReadPath(p.into());
    let wt = &w("/p/.wkts/wt");
    let absolute = RuleSet::new(vec![with(RuleBehavior::Deny, "Read", "/p/secrets/**")]);
    let relative = RuleSet::new(vec![
        with(RuleBehavior::Deny, "Read", "secrets/**"),
        with(RuleBehavior::Ask, "Edit", "docs/**"),
        with(RuleBehavior::Allow, "Read", "src/**"),
    ]);

    let denied = |hit: RuleHit| matches!(hit, RuleHit::Deny(_));
    assert!(denied(hit(
        wt,
        &["/p"],
        &absolute,
        "Read",
        read("secrets/key")
    )));
    assert!(denied(hit(
        "/p",
        &[],
        &relative,
        "Read",
        read(&w("/p/.wkts/wt/secrets/key"))
    )));
    assert!(denied(hit(
        "/p",
        &[],
        &absolute,
        "Read",
        read(&w("/p/.wkts/wt/secrets/key"))
    )));
    assert!(denied(hit(
        &w("/r/.wkts/wt"),
        &["/r/sub"],
        &relative,
        "Read",
        read("sub/secrets/key")
    )));
    assert!(denied(hit(
        "/r/sub",
        &[],
        &relative,
        "Read",
        read(&w("/r/.wkts/wt/sub/secrets/key"))
    )));
    assert!(matches!(
        hit(
            "/p",
            &[],
            &relative,
            "Edit",
            Subject::WritePath(w("/p/.wkts/wt/docs/a.md"))
        ),
        RuleHit::Ask(_)
    ));
    assert!(matches!(
        hit(
            "/p",
            &[],
            &relative,
            "Read",
            read(&w("/p/.wkts/wt/src/a.rs"))
        ),
        RuleHit::None
    ));
    assert!(matches!(
        hit(
            "/p",
            &[],
            &relative,
            "Read",
            read("/p/elsewhere/wt/secrets/key")
        ),
        RuleHit::None
    ));

    let walk = crate::ruleset::path_denial(
        vec![with(RuleBehavior::Deny, "Read", "secrets/**")],
        "Grep",
        std::path::Path::new("/p"),
        layout,
    )
    .expect("a deny rule with a path");
    assert!(walk(std::path::Path::new(&w("/p/.wkts/wt/secrets/key"))));
    assert!(!walk(std::path::Path::new(&w("/p/.wkts/wt/src/a.rs"))));
    assert!(
        walk(std::path::Path::new("/p/.wkts/wt/secrets/key")),
        "a worktree under .agix is an alias whatever the instance's own name"
    );
}

fn in_layout(ctx: &mut base::tool::ToolContext, layout: &base::paths::Layout) {
    let config = Arc::make_mut(&mut ctx.config);
    config.layout = layout.clone();
    Arc::make_mut(&mut config.sandbox).paths =
        Arc::new(base::paths::SensitivePaths::new(None, None, layout.clone()));
}
