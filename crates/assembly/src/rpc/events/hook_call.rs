//! `hook_call` 帧的投影：会话里的 hook 调用，以及不带会话的 `plugin.call` 的临时引擎
//! 发出的——后者多带一个 `plugin_call`，说明它来自哪次调用。

use super::SessionEventPayload;
use crate::session_pool::api::PluginCallOrigin;
use base::event::AgentEvent;

impl SessionEventPayload {
    /// `event`'s frame when it is a hook call, marked as one of the call in
    /// no session `plugin_call` when it comes from one.
    pub(super) fn hook_call(
        event: &AgentEvent,
        plugin_call: Option<&PluginCallOrigin>,
    ) -> Option<Self> {
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
            plugin_call: plugin_call.cloned(),
        })
    }
}
