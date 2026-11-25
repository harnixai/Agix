//! A host's call of a tool object outside any turn, through the session's
//! whole tool pipeline — permission, `PreToolUse`, `tool.around`, the call,
//! `PostToolUse`, telemetry — the way the model's calls go, beside a turn
//! that may be running. The tool is handed over, so it need not be one the model can call.
//!
//! The call itself is the host's approval: what the session would ask about
//! runs, what its rules deny or its mode refuses does not. Nobody is asked
//! anything, neither about it nor about the calls it makes. `permission_rules`
//! hold after the session's own for this call and the calls it makes: a deny
//! refuses in any mode, an allow answers only what the mode would ask.

use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::interface::tool::Tool;
use crate::interface::tool_middleware::ToolOutcome;
use crate::permission::PermissionRule;

#[async_trait::async_trait]
pub trait HostToolRunner: Send + Sync {
    async fn run(
        &self,
        tool: Arc<dyn Tool>,
        input: Value,
        permission_rules: Vec<PermissionRule>,
        cancel: CancellationToken,
    ) -> HostCallOutcome;
    /// The session's tool `name`, if its tool policy lets it have one.
    fn tool(&self, name: &str) -> Option<Arc<dyn Tool>>;
}

/// `outcome` `Err`: no answer — refused, invalid, failed, the session gone. `by_middleware`: the
/// answer is not the tool's own — a `tool.around` gave or changed it, or a `tool.result` rewrote
/// it. `stopped`: the host or the session's end told the call to stop.
#[derive(Debug, Clone, PartialEq)]
pub struct HostCallOutcome {
    pub outcome: ToolOutcome,
    pub by_middleware: bool,
    pub stopped: bool,
}
