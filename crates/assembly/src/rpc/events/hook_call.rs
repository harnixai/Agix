//! `hook_call` 帧的投影：会话里的 hook 调用，以及宿主不带会话的调用的临时引擎发出的——
//! 后者多带一个标记说明它来自哪次调用：`plugin.call` 的是 `plugin_call`，`mcp.callTool`
//! 的是 `mcp_call`。

use super::SessionEventPayload;
use crate::session_pool::api::HostCallOrigin;
use base::event::AgentEvent;

impl SessionEventPayload {
    /// `event`'s frame when it is a hook call, marked as one of the host's
    /// call in no session `origin` when it comes from one.
    pub(super) fn hook_call(event: &AgentEvent, origin: Option<&HostCallOrigin>) -> Option<Self> {
        let AgentEvent::HostHookCall {
            call_id,
            hook_event,
            input,
            timeout_ms,
            task_id,
            agent_session_id,
            cwd,
            depth,
            host: _,
        } = event
        else {
            return None;
        };
        Some(Self::HookCall {
            call_id: call_id.clone(),
            hook_event: hook_event.clone(),
            input: input.clone(),
            timeout_ms: *timeout_ms,
            task_id: task_id.clone(),
            agent_session_id: agent_session_id.clone(),
            cwd: cwd.display().to_string(),
            depth: *depth,
            plugin_call: match origin {
                Some(HostCallOrigin::Plugin(call)) => Some(call.clone()),
                _ => None,
            },
            mcp_call: match origin {
                Some(HostCallOrigin::Mcp(call)) => Some(call.clone()),
                _ => None,
            },
        })
    }
}
