//! Real `PromptHookExecutor`/`AgentHookExecutor` implementations.
//!
//! `crates/hooks` deliberately has no way to run these itself (it doesn't
//! depend on `model`/`runtime` to avoid a circular dependency — see the
//! `PromptHookExecutor` doc comment in `hooks::runner`); without an executor
//! a `"type": "prompt"` or `"type": "agent"` hook is `HookOutcome::Skipped`
//! with "no executor configured". `Builder::build()` wires these in using
//! the session's `model` and its `Delegator`.

use base::interface::delegate::{
    AskRoute, DelegateEnd, DelegateError, DelegateMode, DelegateRequest, Delegated, Delegator,
    StopsWith, Workdir,
};
use base::interface::model::ThinkingMode;
use base::interface::model::{
    MessageRole, Model, ModelContentBlock, ModelEvent, ModelMessage, StreamParams, UsageTap,
};
use base::interface::tool_selector::{ToolPattern, ToolSelector};
use base::prompt::PromptBlock;
use hooks::payload::HookInput;
use hooks::runner::{AgentHookError, AgentHookExecutor, PromptHookExecutor};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Runs a `"type": "prompt"` hook as a single one-shot, tool-less model call:
/// the hook's `prompt` becomes the system prompt, the serialized `HookInput`
/// payload becomes the user message, and the concatenated text deltas become
/// the response `HookRunner` parses as a `HookResponse`. The call is spent
/// into the turn the payload names, on that turn's side.
pub struct ModelPromptHookExecutor {
    model: Arc<dyn Model>,
    default_model_name: String,
    max_tokens: u32,
    /// The session whose hooks these are, so the call is filed under it.
    session_id: Option<String>,
}

impl ModelPromptHookExecutor {
    pub fn new(
        model: Arc<dyn Model>,
        default_model_name: String,
        max_tokens: u32,
        session_id: Option<String>,
    ) -> Self {
        Self {
            model,
            default_model_name,
            max_tokens,
            session_id,
        }
    }

    /// The hook's call asks for no more than the turn it spends into has
    /// left once its input is paid, and is not made when that is too little.
    /// Nothing of the hook's prompt is assumed cached.
    fn max_tokens_within(
        &self,
        payload: &HookInput,
        prompt: &str,
        messages: &[ModelMessage],
    ) -> Result<u32, String> {
        let Some(spend) = &payload.spend else {
            return Ok(self.max_tokens);
        };
        let counter = base::interface::token_counter::default_counter();
        let input = counter.count_text(prompt) + counter.count_messages(messages);
        match crate::turn::call_input::headroom(spend.remaining(), input as u64) {
            Ok(left) => Ok(left.map_or(self.max_tokens, |left| self.max_tokens.min(left))),
            Err(refused) => Err(format!("not asked: {}", refused.message())),
        }
    }
}

#[async_trait::async_trait]
impl PromptHookExecutor for ModelPromptHookExecutor {
    async fn execute(
        &self,
        prompt: &str,
        model: Option<&str>,
        payload: &HookInput,
        stop: &CancellationToken,
    ) -> Result<String, String> {
        let payload_json = serde_json::to_string(payload).unwrap_or_default();
        let messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: payload_json }],
        }];
        let max_tokens = self.max_tokens_within(payload, prompt, &messages)?;
        let params = StreamParams {
            model: model
                .map(str::to_string)
                .unwrap_or_else(|| self.default_model_name.clone()),
            max_tokens,
            thinking_mode: ThinkingMode::Off,
            effort: None,
            cache_edits: vec![],
            origin: self.session_id.as_ref().map(|id| {
                base::interface::model::CallOrigin::auxiliary(
                    id,
                    base::interface::model::call_purpose::HOOK,
                )
            }),
            input_map: None,
            provider_search: None,
            usage_tap: payload
                .spend
                .clone()
                .map(UsageTap::charging)
                .unwrap_or_default(),
        };
        let prompt_blocks = vec![PromptBlock::system(prompt.to_string())];
        let mut stream = self
            .model
            .stream(prompt_blocks, vec![], messages, params, stop.child_token())
            .await
            .map_err(|e| format!("model call failed: {e}"))?;

        use futures::StreamExt;
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(ModelEvent::TextDelta { text: t }) => text.push_str(&t),
                Ok(ModelEvent::EndTurn { .. }) => break,
                Ok(_) => {}
                Err(e) => return Err(format!("model stream error: {e}")),
            }
        }
        Ok(text)
    }
}

/// Runs a `"type": "agent"` hook as a `DelegateMode::Hook` delegate of the
/// session, stopping with the hook's `stop` and spending into the turn the
/// payload names, as any delegate of that turn does. The `HookInput` payload
/// is folded into the prompt; a hook's `model` runs the agent on that model,
/// routed the way the session routes a model name.
///
/// The agent reads a payload the model wrote and its own calls are not judged
/// by the hook, so it gets only the read-only tools and nobody to ask: what
/// would need approval is refused.
pub struct DelegatorHookExecutor {
    delegator: Arc<dyn Delegator>,
    cwd: PathBuf,
    /// The project whose worktrees a payload's `cwd` may name instead.
    project: Option<PathBuf>,
    layout: base::paths::Layout,
}

const HOOK_AGENT_TOOLS: [&str; 3] = ["Read", "Grep", "Glob"];

fn hook_agent_tools() -> ToolSelector {
    ToolSelector::layer(
        Some(HOOK_AGENT_TOOLS.map(|t| ToolPattern::Name(t.into())).into()),
        Vec::new(),
    )
}

impl DelegatorHookExecutor {
    pub fn new(delegator: Arc<dyn Delegator>, cwd: PathBuf) -> Self {
        Self {
            delegator,
            cwd,
            project: None,
            layout: base::paths::Layout::default(),
        }
    }

    /// The agent judges a call made in `project` or one of its worktrees
    /// ([`base::paths::Layout::worktrees_dir_names`]) where it was made, as
    /// the payload's `cwd` names it.
    pub fn in_worktrees_of(
        mut self,
        project: Option<PathBuf>,
        layout: base::paths::Layout,
    ) -> Self {
        self.project = project;
        self.layout = layout;
        self
    }

    fn workdir_for(&self, payload: &HookInput) -> PathBuf {
        let named = Path::new(&payload.cwd);
        if payload.cwd.is_empty() || named == self.cwd {
            return self.cwd.clone();
        }
        let Some(project) = &self.project else {
            return self.cwd.clone();
        };
        let real = base::path::resolve_physically(named);
        let in_a_worktrees_dir = |parent: &Path| {
            self.layout
                .worktrees_dir_names()
                .any(|name| base::path::resolve_physically(&project.join(name)) == parent)
        };
        if real == base::path::resolve_physically(project)
            || (real.parent().is_some_and(in_a_worktrees_dir) && real.is_dir())
        {
            return named.to_path_buf();
        }
        tracing::warn!(
            cwd = %payload.cwd,
            "an agent hook's payload names a directory that is neither the project nor one \
             of its worktrees; the hook's agent works in {} instead",
            self.cwd.display()
        );
        self.cwd.clone()
    }
}

#[async_trait::async_trait]
impl AgentHookExecutor for DelegatorHookExecutor {
    async fn execute(
        &self,
        prompt: &str,
        model: Option<&str>,
        payload: &HookInput,
        stop: &CancellationToken,
    ) -> Result<String, AgentHookError> {
        let payload_json = serde_json::to_string(payload).unwrap_or_default();
        let mode = DelegateMode::Hook {
            judged_session: payload.session_id.clone(),
        };
        let stops_with = StopsWith::Call(stop.child_token());
        let mut req = DelegateRequest::new(
            format!("{prompt}\n\nContext:\n{payload_json}"),
            mode,
            stops_with,
        );
        req.model = model.map(str::to_string);
        req.workdir = Workdir::At(self.workdir_for(payload));
        req.tools = hook_agent_tools();
        req.asks = AskRoute::Refuse;
        req.spend = payload.spend.clone();
        match self.delegator.delegate(req).await {
            Ok(Delegated::Finished(outcome)) => match outcome.end {
                DelegateEnd::Failed(e) => Err(AgentHookError::Failed(e)),
                DelegateEnd::Completed | DelegateEnd::Cancelled | DelegateEnd::CutShort(_) => {
                    Ok(outcome.text)
                }
            },
            Ok(Delegated::Started(_)) => Err(AgentHookError::Failed(
                "the hook's agent was started, not run".into(),
            )),
            Err(DelegateError::NotJudged(_)) => Err(AgentHookError::NotJudged),
            Err(e) => Err(AgentHookError::Failed(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::{ModelError, ModelEvent, ModelStream, ToolDef};

    fn test_input() -> HookInput {
        HookInput {
            hook_event_name: "PreToolUse".into(),
            session_id: "test".into(),
            cwd: "/tmp".into(),
            permission_mode: "default".into(),
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "ls"})),
            tool_use_id: Some("toolu_1".into()),
            tool_result: None,
            is_error: None,
            user_prompt: None,
            ..Default::default()
        }
    }

    /// Returns a fixed text response, ignoring the actual prompt/messages —
    /// enough to prove `ModelPromptHookExecutor` really calls `Model::stream`
    /// and collects its `TextDelta`s, without needing a real API call.
    struct StubModel {
        response: &'static str,
    }
    #[async_trait::async_trait]
    impl Model for StubModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<PromptBlock>,
            _tools: Vec<ToolDef>,
            _messages: Vec<ModelMessage>,
            _params: StreamParams,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            let text = self.response.to_string();
            let events = vec![
                Ok(ModelEvent::TextDelta { text }),
                Ok(ModelEvent::EndTurn {
                    stop_reason: "end_turn".into(),
                    usage: Default::default(),
                }),
            ];
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn model_prompt_hook_executor_returns_the_models_text() {
        let model: Arc<dyn Model> = Arc::new(StubModel {
            response: r#"{"decision":"approve"}"#,
        });
        let executor = ModelPromptHookExecutor::new(model, "test-model".into(), 100, None);
        let out = executor
            .execute(
                "is this safe?",
                None,
                &test_input(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out, r#"{"decision":"approve"}"#);
    }

    #[tokio::test]
    async fn a_prompt_hook_is_not_asked_when_its_turn_cannot_pay_for_it() {
        let model: Arc<dyn Model> = Arc::new(StubModel { response: "{}" });
        let executor = ModelPromptHookExecutor::new(model, "test-model".into(), 4000, None);
        let turn = Arc::new(base::interface::budget_policy::SpendNode::turn(
            Some(2000),
            None,
        ));
        turn.add(base::interface::model::Usage {
            input_tokens: 1200,
            ..Default::default()
        });
        let input = HookInput {
            spend: Some(turn),
            ..test_input()
        };
        let refused = executor
            .execute("is this safe?", None, &input, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(refused.contains("token budget of 2000"), "{refused}");
    }

    struct StubDelegator {
        response: &'static str,
    }

    #[async_trait::async_trait]
    impl Delegator for StubDelegator {
        async fn delegate(
            &self,
            req: DelegateRequest,
        ) -> Result<Delegated, base::interface::delegate::DelegateError> {
            assert!(
                matches!(&req.mode, DelegateMode::Hook { judged_session } if judged_session == "test")
            );
            let builtin = base::interface::tool::ToolOrigin::Builtin;
            assert_eq!(req.asks, AskRoute::Refuse);
            assert!(req.tools.admits("Read", &builtin));
            assert!(!req.tools.admits("Bash", &builtin));
            assert!(!req
                .tools
                .admits("mcp__s__t", &base::interface::tool::ToolOrigin::mcp("s")));
            let text = match (&req.model, &req.stops_with) {
                (_, StopsWith::Call(stop)) if stop.is_cancelled() => "stopped".to_string(),
                (Some(model), _) => format!("ran on {model}"),
                (None, _) => self.response.to_string(),
            };
            Ok(Delegated::Finished(
                base::interface::delegate::DelegateOutcome {
                    text,
                    end: DelegateEnd::Completed,
                    structured: None,
                    session_id: "hook-agent".into(),
                },
            ))
        }

        async fn stop_member(&self, _: &base::interface::delegate::TeamSeat) {}

        fn live_members(&self) -> Vec<base::interface::delegate::LiveMember> {
            Vec::new()
        }
    }

    fn judge() -> DelegatorHookExecutor {
        let delegator = Arc::new(StubDelegator {
            response: r#"{"ok":true}"#,
        });
        DelegatorHookExecutor::new(delegator, std::path::PathBuf::from("/tmp"))
    }

    #[tokio::test]
    async fn an_agent_hook_answers_with_its_agents_text() {
        let out = judge()
            .execute(
                "review this",
                None,
                &test_input(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out, r#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn an_agent_hooks_model_is_the_model_its_agent_runs_on() {
        let out = judge()
            .execute(
                "review this",
                Some("claude-haiku-4-5"),
                &test_input(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out, "ran on claude-haiku-4-5");
    }

    #[tokio::test]
    async fn an_agent_hooks_agent_stops_with_the_hook() {
        let stop = CancellationToken::new();
        stop.cancel();
        let out = judge()
            .execute("review this", None, &test_input(), &stop)
            .await
            .unwrap();
        assert_eq!(out, "stopped");
    }

    #[derive(Default)]
    struct WhereItRuns(std::sync::Mutex<Vec<Workdir>>);

    #[async_trait::async_trait]
    impl Delegator for WhereItRuns {
        async fn delegate(&self, req: DelegateRequest) -> Result<Delegated, DelegateError> {
            self.0.lock().unwrap().push(req.workdir);
            Ok(Delegated::Finished(
                base::interface::delegate::DelegateOutcome {
                    text: r#"{"ok":true}"#.into(),
                    end: DelegateEnd::Completed,
                    structured: None,
                    session_id: "hook-agent".into(),
                },
            ))
        }

        async fn stop_member(&self, _: &base::interface::delegate::TeamSeat) {}

        fn live_members(&self) -> Vec<base::interface::delegate::LiveMember> {
            Vec::new()
        }
    }

    /// The payload's `cwd` decides where the agent works only when it is the
    /// project or one of its worktrees.
    #[tokio::test]
    async fn an_agent_hook_works_where_the_call_was_made_inside_the_project() {
        for name in [".agix", ".codix"] {
            let layout = base::paths::Layout::new(name).unwrap();
            an_agent_hook_works_inside_the_project_in(&layout).await;
            an_agent_hook_works_inside_the_project_in(
                &layout.with_worktrees_dir(".trees").unwrap(),
            )
            .await;
        }
    }

    async fn an_agent_hook_works_inside_the_project_in(layout: &base::paths::Layout) {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap().join("project");
        let worktree = layout.worktrees_dir(&project).join("wt");
        let made_before = project.join(base::paths::DEFAULT_WORKTREES_DIR).join("old");
        let not_one = layout.project_dir(&project).join("worktrees").join("wt");
        let elsewhere = dir.path().canonicalize().unwrap().join("elsewhere");
        for d in [
            &worktree,
            &made_before,
            &not_one,
            &elsewhere,
            &project.join("src"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        let runs = Arc::new(WhereItRuns::default());
        let executor = DelegatorHookExecutor::new(runs.clone(), project.clone())
            .in_worktrees_of(Some(project.clone()), layout.clone());
        for cwd in [
            &worktree,
            &made_before,
            &project,
            &not_one,
            &elsewhere,
            &project.join("src"),
        ] {
            let input = HookInput {
                cwd: cwd.display().to_string(),
                ..test_input()
            };
            executor
                .execute("judge", None, &input, &CancellationToken::new())
                .await
                .unwrap();
        }
        let dirs: Vec<PathBuf> = runs
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|w| match w {
                Workdir::At(dir) => dir.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            dirs,
            [
                worktree,
                made_before,
                project.clone(),
                project.clone(),
                project.clone(),
                project
            ]
        );
    }
}
