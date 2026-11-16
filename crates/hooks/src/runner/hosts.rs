//! `HookRunner` 的 hosts 后端：宿主连接经协议注册的 hook。设置文件里的 hook 先跑，
//! 它们拦下的调用不再问宿主；没拦下的，宿主看到的是设置文件 hook 改写之后的入参。
//! 每次触发时才去读注册表，所以注册、撤销对已经建好的会话也立刻生效。调用经会话
//! 自己的 `HostCalls` 发出：子 Agent 共用根会话的 runner，按 payload 的 `session_id`
//! 找到它自己那条（带上 `task_id` / `agent_session_id` 的）通道。

use super::matcher::tool_matches;
use super::{block_on_failure, HookOutcome, HookRunner};
use crate::config::{HookEvent, OnError};
use crate::payload::{HookDecision, HookInput, HookResponse};
use base::interface::host_call::{HostCall, HostCallEnd, HostCalls, HostId, HostReply};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The events a host connection may hook.
pub const HOSTED_EVENTS: &[HookEvent] = &[
    HookEvent::PreToolUse,
    HookEvent::PostToolUse,
    HookEvent::PostToolUseFailure,
    HookEvent::UserPromptSubmit,
    HookEvent::Stop,
];

/// One hook a host connection registered.
#[derive(Clone)]
pub struct HostHook {
    pub host: HostId,
    /// Its place in the list the connection registered.
    pub index: usize,
    /// Which tool calls it is asked about, as tool selector patterns; empty
    /// is every one. Only the tool events read it.
    pub tools: Vec<String>,
    pub timeout: Duration,
    pub on_error: OnError,
    /// The connection's hook calls in flight, shared by all its hooks.
    pub slots: Arc<tokio::sync::Semaphore>,
}

impl HostHook {
    pub fn describe(&self) -> String {
        format!("host connection {} hook #{}", self.host.0, self.index)
    }

    fn applies_to(&self, event: HookEvent, input: &HookInput) -> bool {
        let tool_event = matches!(
            event,
            HookEvent::PreToolUse | HookEvent::PostToolUse | HookEvent::PostToolUseFailure
        );
        if !tool_event || self.tools.is_empty() {
            return true;
        }
        let Some(name) = input.tool_name.as_deref() else {
            return false;
        };
        self.tools.iter().any(|p| tool_matches(p, name))
    }
}

/// The hooks host connections have registered, read each time one may fire.
pub trait HostHookSource: Send + Sync {
    fn has_hooks_for(&self, event: HookEvent) -> bool;
    /// In a fixed order: by connection, then by index.
    fn hooks_for(&self, event: HookEvent) -> Vec<HostHook>;
}

pub(super) struct Hosts {
    source: Arc<dyn HostHookSource>,
    /// Each session of the tree's own host calls, its root's first.
    routes: Mutex<Vec<(String, Weak<dyn HostCalls>)>>,
}

impl Hosts {
    pub(super) fn has_hooks_for(&self, event: HookEvent) -> bool {
        self.source.has_hooks_for(event)
    }

    fn route(&self, session_id: &str, calls: &Arc<dyn HostCalls>) {
        let mut routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        routes.retain(|(id, calls)| id != session_id && calls.strong_count() > 0);
        routes.push((session_id.to_string(), Arc::downgrade(calls)));
    }

    /// `session_id`'s own calls; a call made under a session that never
    /// registered one — a program's nested calls — goes out as the root's.
    fn calls_for(&self, session_id: &str) -> Option<Arc<dyn HostCalls>> {
        let routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        routes
            .iter()
            .find(|(id, _)| id == session_id)
            .and_then(|(_, calls)| calls.upgrade())
            .or_else(|| routes.iter().find_map(|(_, calls)| calls.upgrade()))
    }
}

impl HookRunner {
    /// Builder: also ask the hooks host connections registered with `source`.
    pub fn with_hosts(mut self, source: Arc<dyn HostHookSource>) -> Self {
        self.hosts = Some(Arc::new(Hosts {
            source,
            routes: Mutex::new(Vec::new()),
        }));
        self
    }

    /// The host calls of `session_id`, one of the sessions this runner
    /// serves; nothing when it asks no host.
    pub fn route_host_calls(&self, session_id: &str, calls: &Arc<dyn HostCalls>) {
        if let Some(hosts) = &self.hosts {
            hosts.route(session_id, calls);
        }
    }

    /// The host hooks' outcomes on `input`, by registration order, with the
    /// name each is reported under. `number` is the first one's position
    /// among the run's hooks.
    pub(super) async fn run_hosted(
        &self,
        event: HookEvent,
        input: &HookInput,
        number: usize,
        stop: &CancellationToken,
    ) -> Vec<(String, HookOutcome)> {
        let Some(hosts) = &self.hosts else {
            return Vec::new();
        };
        let hooks: Vec<HostHook> = hosts
            .source
            .hooks_for(event)
            .into_iter()
            .filter(|h| h.applies_to(event, input))
            .collect();
        if hooks.is_empty() {
            return Vec::new();
        }
        let payload = serde_json::to_value(input).unwrap_or_default();
        let calls = hosts.calls_for(&input.session_id);
        let asks = hooks.iter().map(|hook| {
            let calls = calls.clone();
            let payload = payload.clone();
            async move { ask(hook, calls, event, input, payload, stop).await }
        });
        let outcomes = futures::future::join_all(asks).await;
        hooks
            .iter()
            .zip(outcomes)
            .enumerate()
            .map(|(n, (hook, outcome))| {
                let name = hook.describe();
                let outcome = match (hook.on_error, outcome) {
                    (OnError::Block, outcome) => {
                        block_on_failure(event, number + n, &name, outcome)
                    }
                    (OnError::Allow, HookOutcome::Error(error)) => {
                        tracing::warn!(
                            event = ?event,
                            hook = %name,
                            %error,
                            "host hook failed; it neither blocks nor adds anything"
                        );
                        HookOutcome::Error(error)
                    }
                    (OnError::Allow, outcome) => outcome,
                };
                (name, outcome)
            })
            .collect()
    }
}

async fn ask(
    hook: &HostHook,
    calls: Option<Arc<dyn HostCalls>>,
    event: HookEvent,
    input: &HookInput,
    payload: serde_json::Value,
    stop: &CancellationToken,
) -> HookOutcome {
    let Some(calls) = calls else {
        return HookOutcome::Error("the session has no way to reach its host".into());
    };
    let Ok(_slot) = hook.slots.clone().try_acquire_owned() else {
        return HookOutcome::Error("its connection has too many hook calls open".into());
    };
    let call = HostCall::hook(
        format!("hk_{}", base::id::Id::new()),
        input.tool_use_id.clone().unwrap_or_default(),
        event.name(),
        payload,
        hook.timeout,
    )
    .made_in(&input.cwd, input.depth)
    .for_host(Some(hook.host));
    match calls.call(call, stop).await {
        HostCallEnd::Replied(HostReply::Hook(answer)) => {
            match serde_json::from_value::<HookResponse>(answer.clone()) {
                Ok(response) => HookOutcome::Ran {
                    response: hosted(response),
                    stdout: answer.to_string(),
                    stderr: String::new(),
                    exit_code: None,
                    failure: None,
                },
                Err(e) => HookOutcome::Error(format!("its answer is not a hook response: {e}")),
            }
        }
        HostCallEnd::Replied(HostReply::Failed(why)) => HookOutcome::Error(why),
        HostCallEnd::Replied(_) => HookOutcome::Error("it answered with a tool result".into()),
        HostCallEnd::TimedOut => HookOutcome::Error(format!(
            "its host did not answer within {}ms",
            hook.timeout.as_millis()
        )),
        HostCallEnd::Cancelled => HookOutcome::Cancelled,
        _ => HookOutcome::Error("its host connection is gone".into()),
    }
}

/// What a host's answer may do: block and add context. It cannot stand in
/// for the user, rewrite the input, end the turn or ask to be run again.
fn hosted(mut response: HookResponse) -> HookResponse {
    if response.decision == Some(HookDecision::Approve) {
        response.decision = None;
    }
    response.updated_input = None;
    response.r#continue = None;
    response.rewake = None;
    response
}

/// `input` as the settings' hooks left it: the host sees what the call will
/// run on.
pub(super) fn rewritten(
    event: HookEvent,
    input: &HookInput,
    updated: Option<&serde_json::Value>,
) -> HookInput {
    let mut input = input.clone();
    match (event, updated) {
        (HookEvent::PreToolUse, Some(v)) => input.tool_input = Some(v.clone()),
        (HookEvent::UserPromptSubmit, Some(serde_json::Value::String(text))) => {
            input.user_prompt = Some(text.clone());
        }
        _ => {}
    }
    input
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HookConfig, HooksSettings};
    use crate::runner::HookExec;
    use async_trait::async_trait;
    use base::interface::host_call::HostCallKind;
    use serde_json::json;
    use std::collections::HashMap;

    struct Scripted {
        end: Mutex<Option<HostCallEnd>>,
        seen: Mutex<Vec<HostCall>>,
    }

    impl Scripted {
        fn new(end: HostCallEnd) -> Arc<Self> {
            Arc::new(Self {
                end: Mutex::new(Some(end)),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn answering(answer: serde_json::Value) -> Arc<Self> {
            Self::new(HostCallEnd::Replied(HostReply::Hook(answer)))
        }

        fn calls(&self) -> Vec<HostCall> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HostCalls for Scripted {
        async fn call(&self, call: HostCall, _cancel: &CancellationToken) -> HostCallEnd {
            self.seen.lock().unwrap().push(call);
            self.end
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(HostCallEnd::NoHost)
        }
    }

    struct Registry(Vec<Entry>);

    impl HostHookSource for Registry {
        fn has_hooks_for(&self, event: HookEvent) -> bool {
            !self.hooks_for(event).is_empty()
        }

        fn hooks_for(&self, event: HookEvent) -> Vec<HostHook> {
            self.0
                .iter()
                .filter(|h| h.events.contains(&event))
                .map(|h| h.hook.clone())
                .collect()
        }
    }

    struct Entry {
        events: Vec<HookEvent>,
        hook: HostHook,
    }

    impl Registry {
        fn of(entries: Vec<Entry>) -> Registry {
            Registry(entries)
        }
    }

    fn hook(tools: &[&str], on_error: OnError, slots: usize) -> HostHook {
        HostHook {
            host: HostId(7),
            index: 0,
            tools: tools.iter().map(|s| s.to_string()).collect(),
            timeout: Duration::from_secs(5),
            on_error,
            slots: Arc::new(tokio::sync::Semaphore::new(slots)),
        }
    }

    fn pre_tool(tools: &[&str], on_error: OnError) -> Registry {
        Registry::of(vec![Entry {
            events: vec![HookEvent::PreToolUse, HookEvent::UserPromptSubmit],
            hook: hook(tools, on_error, 4),
        }])
    }

    fn input(session: &str, tool: &str) -> HookInput {
        HookInput {
            hook_event_name: "PreToolUse".into(),
            session_id: session.into(),
            cwd: "/tmp".into(),
            permission_mode: "default".into(),
            tool_name: Some(tool.into()),
            tool_input: Some(json!({"command": "ls"})),
            tool_use_id: Some("toolu_1".into()),
            ..Default::default()
        }
    }

    fn runner(settings: HooksSettings, registry: Registry) -> HookRunner {
        let providers = platform::local_providers();
        HookRunner::new(
            settings,
            HookExec {
                process: providers.process,
                network: providers.network,
            },
        )
        .with_hosts(Arc::new(registry))
    }

    fn routed(runner: &HookRunner, calls: &Arc<Scripted>) {
        let as_calls: Arc<dyn HostCalls> = calls.clone();
        runner.route_host_calls("s1", &as_calls);
    }

    fn settings_hook(command: &str) -> HooksSettings {
        let mut s: HooksSettings = HashMap::new();
        s.insert(
            HookEvent::PreToolUse,
            vec![HookConfig::Command {
                command: command.into(),
                shell: None,
                timeout: None,
                if_pattern: None,
                only_on_error: None,
                once: None,
                async_rewake: None,
                on_error: OnError::Allow,
            }],
        );
        s
    }

    async fn fire(runner: &HookRunner, input: &HookInput) -> crate::runner::HookRunResult {
        runner
            .run(HookEvent::PreToolUse, input, &CancellationToken::new())
            .await
    }

    #[test]
    fn has_hooks_for_is_true_with_only_host_hooks() {
        let r = runner(HashMap::new(), pre_tool(&[], OnError::Allow));
        assert!(r.has_hooks_for(HookEvent::PreToolUse));
        assert!(!r.has_hooks_for(HookEvent::PostToolUse));
    }

    #[tokio::test]
    async fn a_settings_hook_that_blocks_keeps_the_host_from_being_asked() {
        let r = runner(
            settings_hook(r#"echo '{"decision":"block","message":"no"}'"#),
            pre_tool(&[], OnError::Allow),
        );
        let host = Scripted::answering(json!({}));
        routed(&r, &host);
        let result = fire(&r, &input("s1", "Bash")).await;
        assert!(result.blocked().is_some());
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn the_host_sees_the_input_the_settings_hooks_rewrote() {
        let r = runner(
            settings_hook(r#"echo '{"updated_input":{"command":"echo rewritten"}}'"#),
            pre_tool(&[], OnError::Allow),
        );
        let host = Scripted::answering(json!({}));
        routed(&r, &host);
        fire(&r, &input("s1", "Bash").with_depth(2)).await;
        let calls = host.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            (calls[0].cwd.as_path(), calls[0].depth),
            (std::path::Path::new("/tmp"), 2)
        );
        let HostCallKind::Hook { event, input } = &calls[0].kind else {
            panic!("not a hook call");
        };
        assert_eq!(event, "PreToolUse");
        assert_eq!(input["tool_input"]["command"], "echo rewritten");
        assert_eq!(input["depth"], 2);
        assert_eq!(calls[0].host, Some(HostId(7)));
        assert_eq!(calls[0].tool_use_id, "toolu_1");
    }

    #[tokio::test]
    async fn a_host_block_blocks_with_its_message() {
        let r = runner(HashMap::new(), pre_tool(&[], OnError::Allow));
        let host = Scripted::answering(json!({"decision": "block", "message": "m"}));
        routed(&r, &host);
        let result = fire(&r, &input("s1", "Bash")).await;
        assert_eq!(
            result.blocked().and_then(|b| b.message.as_deref()),
            Some("m")
        );
    }

    #[tokio::test]
    async fn a_host_cannot_approve_rewrite_stop_or_rewake() {
        let r = runner(HashMap::new(), pre_tool(&[], OnError::Allow));
        let host = Scripted::answering(json!({
            "decision": "approve",
            "updated_input": {"command": "rm -rf /"},
            "continue": false,
            "rewake": true,
        }));
        routed(&r, &host);
        let result = fire(&r, &input("s1", "Bash")).await;
        assert!(result.approved().is_none());
        assert!(result.blocked().is_none());
        assert!(result.updated_input().is_none());
        assert!(!result.discontinued());
    }

    #[tokio::test]
    async fn a_host_answer_that_is_no_hook_response_is_a_failure() {
        let r = runner(HashMap::new(), pre_tool(&[], OnError::Block));
        let host = Scripted::answering(json!({"decision": "maybe"}));
        routed(&r, &host);
        let result = fire(&r, &input("s1", "Bash")).await;
        assert!(result.blocked().is_some());
    }

    #[tokio::test]
    async fn additional_context_reaches_the_reminders() {
        let r = runner(HashMap::new(), pre_tool(&[], OnError::Allow));
        let host = Scripted::answering(json!({"additional_context": "remember the milk"}));
        routed(&r, &host);
        let result = fire(&r, &input("s1", "Bash")).await;
        let reminders = result.context_reminders(HookEvent::PreToolUse);
        assert_eq!(reminders.len(), 1);
        assert!(
            reminders[0].contains("remember the milk"),
            "{}",
            reminders[0]
        );
    }

    #[tokio::test]
    async fn failures_follow_on_error() {
        let failures = [
            HostCallEnd::TimedOut,
            HostCallEnd::NoHost,
            HostCallEnd::HostGone,
            HostCallEnd::Replied(HostReply::Failed("boom".into())),
        ];
        for end in failures {
            let r = runner(HashMap::new(), pre_tool(&[], OnError::Allow));
            let host = Scripted::new(end.clone());
            routed(&r, &host);
            let result = fire(&r, &input("s1", "Bash")).await;
            assert!(result.blocked().is_none(), "{end:?}");
            assert_eq!(host.calls().len(), 1);

            let r = runner(HashMap::new(), pre_tool(&[], OnError::Block));
            let host = Scripted::new(end.clone());
            routed(&r, &host);
            let result = fire(&r, &input("s1", "Bash")).await;
            let message = result.blocked().and_then(|b| b.message.clone());
            assert!(
                message
                    .as_deref()
                    .is_some_and(|m| m.contains("on_error is block")),
                "{end:?}: {message:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_session_that_reaches_no_host_is_a_failure() {
        let r = runner(HashMap::new(), pre_tool(&[], OnError::Block));
        let result = fire(&r, &input("s1", "Bash")).await;
        assert!(result.blocked().is_some());
    }

    #[tokio::test]
    async fn a_cancelled_call_is_a_cancelled_outcome() {
        let r = runner(HashMap::new(), pre_tool(&[], OnError::Allow));
        let host = Scripted::new(HostCallEnd::Cancelled);
        routed(&r, &host);
        let result = fire(&r, &input("s1", "Bash")).await;
        assert!(matches!(
            result.outcomes.as_slice(),
            [HookOutcome::Cancelled]
        ));
    }

    #[tokio::test]
    async fn no_free_slot_is_a_failure_and_asks_nobody() {
        let registry = Registry::of(vec![Entry {
            events: vec![HookEvent::PreToolUse],
            hook: hook(&[], OnError::Block, 0),
        }]);
        let r = runner(HashMap::new(), registry);
        let host = Scripted::answering(json!({}));
        routed(&r, &host);
        let result = fire(&r, &input("s1", "Bash")).await;
        assert!(result.blocked().is_some());
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn tools_patterns_pick_the_calls_asked_about() {
        let cases = [
            (vec!["Bash"], "Read", false),
            (vec!["Bash"], "Bash", true),
            (vec!["mcp__srv"], "mcp__srv__x", true),
            (vec!["mcp__srv"], "mcp__other__x", false),
            (vec![], "Anything", true),
        ];
        for (tools, tool, asked) in cases {
            let r = runner(HashMap::new(), pre_tool(&tools, OnError::Allow));
            let host = Scripted::answering(json!({}));
            routed(&r, &host);
            fire(&r, &input("s1", tool)).await;
            assert_eq!(host.calls().len() == 1, asked, "{tools:?} on {tool}");
        }
    }

    #[tokio::test]
    async fn user_prompt_submit_ignores_tools() {
        let r = runner(HashMap::new(), pre_tool(&["Bash"], OnError::Allow));
        let host = Scripted::answering(json!({}));
        routed(&r, &host);
        let mut prompt = input("s1", "x");
        prompt.tool_name = None;
        prompt.user_prompt = Some("hi".into());
        r.run(
            HookEvent::UserPromptSubmit,
            &prompt,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(host.calls().len(), 1);
    }

    #[tokio::test]
    async fn a_call_goes_out_on_its_own_sessions_calls_and_unknown_ones_on_the_root() {
        let r = runner(HashMap::new(), pre_tool(&[], OnError::Allow));
        let root = Scripted::answering(json!({}));
        let child = Scripted::answering(json!({}));
        let root_calls: Arc<dyn HostCalls> = root.clone();
        let child_calls: Arc<dyn HostCalls> = child.clone();
        r.route_host_calls("root", &root_calls);
        r.route_host_calls("child", &child_calls);

        fire(&r, &input("child", "Bash")).await;
        assert_eq!((root.calls().len(), child.calls().len()), (0, 1));

        fire(&r, &input("stranger", "Bash")).await;
        assert_eq!((root.calls().len(), child.calls().len()), (1, 1));

        fire(&r, &input("root", "Bash")).await;
        assert_eq!((root.calls().len(), child.calls().len()), (2, 1));
    }

    #[test]
    fn tool_matches_reads_selector_patterns_against_the_name() {
        assert!(tool_matches("Bash", "Bash"));
        assert!(!tool_matches("Bash", "BashOutput"));
        assert!(tool_matches("*", "Anything"));
        assert!(tool_matches("Notebook*", "NotebookEdit"));
        assert!(!tool_matches("Notebook*", "Read"));
        assert!(tool_matches("mcp__srv", "mcp__srv__x"));
        assert!(tool_matches("mcp__srv__*", "mcp__srv__x"));
        assert!(!tool_matches("mcp__srv", "mcp__srv2__x"));
        assert!(tool_matches("mcp__*", "mcp__any__x"));
        assert!(!tool_matches("mcp__*", "Bash"));
    }
}
