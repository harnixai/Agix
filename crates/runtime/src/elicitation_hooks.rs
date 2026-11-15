//! The `Elicitation` and `ElicitationResult` hooks around an MCP server's
//! question. Whatever [`Elicitation`] a session asks through — the channel to
//! its host, its parent's for a delegate, an embedder's own — the hooks see the
//! question before anyone is asked and the answer before the server gets it.
//! The other kinds of question pass straight through.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base::interface::elicitation::{
    ElicitKind, ElicitOutcome, ElicitRequest, Elicitation, McpElicitMode, McpElicitationAnswer,
};
use hooks::{HookEvent, HookRunner, McpElicitationHook};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// `inner` with the session's MCP question hooks around it, or `inner` itself
/// when no such hook is configured.
pub(crate) fn hooked(
    inner: Arc<dyn Elicitation>,
    hooks: &Arc<HookRunner>,
    ambient: &hooks::payload::Ambient,
    stop: &CancellationToken,
) -> Arc<dyn Elicitation> {
    if !hooks.has_hooks_for(HookEvent::Elicitation)
        && !hooks.has_hooks_for(HookEvent::ElicitationResult)
    {
        return inner;
    }
    Arc::new(HookedElicitation {
        inner,
        hooks: hooks.clone(),
        ambient: ambient.clone(),
        stop: stop.clone(),
    })
}

struct HookedElicitation {
    inner: Arc<dyn Elicitation>,
    hooks: Arc<HookRunner>,
    ambient: hooks::payload::Ambient,
    stop: CancellationToken,
}

impl HookedElicitation {
    async fn mcp(&self, request: ElicitRequest, within: Option<Duration>) -> Option<ElicitOutcome> {
        let ElicitKind::McpServer {
            server,
            mode,
            tool_use_id,
            ..
        } = &request.kind
        else {
            return self.inner.ask_within(request, within).await;
        };
        let mut tool_input = json!({
            "server_name": server,
            "message": request.message,
        });
        match mode {
            McpElicitMode::Form { requested_schema } => {
                tool_input["mode"] = json!("form");
                tool_input["requested_schema"] = requested_schema.clone();
            }
            McpElicitMode::Url { url, .. } => {
                tool_input["mode"] = json!("url");
                tool_input["url"] = json!(url);
            }
        }
        let asked = McpElicitationHook {
            server,
            tool_input,
            tool_use_id: tool_use_id.as_deref(),
        };
        let blocked = self
            .hooks
            .run_elicitation(&self.ambient, &asked, &self.stop)
            .await
            .blocked()
            .is_some();
        let outcome = if blocked {
            Some(ElicitOutcome::answered(&McpElicitationAnswer::Decline))
        } else {
            self.inner.ask_within(request.clone(), within).await
        };
        let answer = outcome
            .as_ref()
            .and_then(ElicitOutcome::answer_as::<McpElicitationAnswer>)
            .unwrap_or(McpElicitationAnswer::Cancel);
        let content = match &answer {
            McpElicitationAnswer::Accept { content } => content.as_ref(),
            _ => None,
        };
        self.hooks
            .run_elicitation_result(&self.ambient, &asked, answer.action(), content, &self.stop)
            .await;
        outcome
    }
}

#[async_trait]
impl Elicitation for HookedElicitation {
    async fn ask(&self, request: ElicitRequest) -> ElicitOutcome {
        if !matches!(request.kind, ElicitKind::McpServer { .. }) {
            return self.inner.ask(request).await;
        }
        self.mcp(request, None)
            .await
            .unwrap_or_else(|| ElicitOutcome::declined("the question was withdrawn"))
    }

    async fn ask_within(
        &self,
        request: ElicitRequest,
        within: Option<Duration>,
    ) -> Option<ElicitOutcome> {
        self.mcp(request, within).await
    }

    fn can_clarify(&self) -> bool {
        self.inner.can_clarify()
    }
}
