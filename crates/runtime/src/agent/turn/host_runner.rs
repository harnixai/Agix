//! The session's [`HostToolRunner`]: a host's call of a tool object outside
//! any turn, run through the same pipeline as a turn's calls with the
//! session's tools, rules, hooks, wrappers and state, as the host's own call.
//! It shares nothing with a running turn but what every call shares, so it
//! may run beside one.

use super::tool_env::host_call_ctx;
use super::tool_exec::{execute_given_tool, ToolExecCtx};
use crate::agent::Agent;
use base::interface::permission::{
    Delegate, PathDenial, PathDenials, PermissionOutcome, PermissionPolicy, PermissionRequest,
    PermissionSet,
};
use base::interface::plugin_call::HostToolRunner;
use base::interface::tool_middleware::ToolOutcome;
use base::permission::PermissionRule;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

impl Agent {
    /// How the host calls a tool through this session outside any turn.
    /// Take it before spawning [`Agent::run`], as with [`Agent::commands`];
    /// it refuses once the session has ended.
    pub fn host_tool_runner(&self) -> Arc<dyn HostToolRunner> {
        Arc::new(SessionToolRunner {
            outside_a_turn: self.host_call_template(),
            session_ended: self.session_ended().clone(),
            environment: Arc::clone(self.policies.environment()),
        })
    }
}

struct SessionToolRunner {
    outside_a_turn: ToolExecCtx,
    session_ended: CancellationToken,
    environment: Arc<dyn base::interface::environment::Environment>,
}

#[async_trait::async_trait]
impl HostToolRunner for SessionToolRunner {
    async fn run(
        &self,
        tool: Arc<dyn base::tool::Tool>,
        input: serde_json::Value,
        rules: Vec<PermissionRule>,
        cancel: CancellationToken,
    ) -> ToolOutcome {
        if self.session_ended.is_cancelled() {
            return Err("the session has ended".into());
        }
        let stop = self.session_ended.child_token();
        let permission = match rules.is_empty() {
            true => self.outside_a_turn.permission.clone(),
            false => with_rules(&self.outside_a_turn.permission, rules),
        };
        let id = format!("host-{}", self.environment.new_id());
        let call = host_call_ctx(&self.outside_a_turn, id, stop.clone(), permission);
        let run = execute_given_tool(&call, tool, input);
        tokio::pin!(run);
        // The call is not dropped when the host cancels: it is told to stop,
        // and ends the way any cancelled call does, its hooks included.
        tokio::select! {
            outcome = &mut run => outcome,
            _ = cancel.cancelled() => {
                stop.cancel();
                run.await
            }
        }
    }

    fn tool(&self, name: &str) -> Option<Arc<dyn base::tool::Tool>> {
        self.outside_a_turn.tools.get(name)
    }
}

/// `permission`, holding every call it judges to `rules` as well, the way a
/// delegation's rules are held: after the session's own.
fn with_rules(permission: &PermissionSet, rules: Vec<PermissionRule>) -> PermissionSet {
    let held = Arc::new(WithRules {
        inner: permission.clone(),
        rules,
    });
    PermissionSet {
        policy: held.clone(),
        grants: permission.grants.clone(),
        path_denials: Some(held),
    }
}

struct WithRules {
    inner: PermissionSet,
    rules: Vec<PermissionRule>,
}

#[async_trait::async_trait]
impl PermissionPolicy for WithRules {
    async fn decide(&self, req: &PermissionRequest<'_>) -> PermissionOutcome {
        let (session, mode, inner, tightens_only) = match req.delegate {
            Some(d) => (d.session, d.mode, d.rules, d.tightens_only),
            None => (req.session, req.session.permission_mode(), &[][..], false),
        };
        let rules = [inner, &self.rules].concat();
        let mut delegate = Delegate::new(session, mode, &rules);
        delegate.tightens_only = tightens_only;
        let relayed = req.for_delegate(delegate);
        self.inner.policy.decide(&relayed).await
    }
}

impl PathDenials for WithRules {
    fn denial(
        &self,
        tool: &str,
        cwd: &std::path::Path,
        extra: &[PermissionRule],
    ) -> Option<PathDenial> {
        self.inner
            .path_denials
            .as_ref()?
            .denial(tool, cwd, &[extra, &self.rules].concat())
    }

    fn denied_read_paths(
        &self,
        cwd: &std::path::Path,
        extra: &[PermissionRule],
    ) -> Vec<std::path::PathBuf> {
        self.inner
            .path_denials
            .as_ref()
            .map(|d| d.denied_read_paths(cwd, &[extra, &self.rules].concat()))
            .unwrap_or_default()
    }
}
