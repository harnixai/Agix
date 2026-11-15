//! `daemon.*` 与 `commands.list` 的线格式。

use crate::discovery::DaemonIdentity;
use crate::session_pool::api::CommandInfo;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Pong {
    #[serde(flatten)]
    pub identity: DaemonIdentity,
    pub pong: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DaemonStatus {
    #[serde(flatten)]
    pub identity: DaemonIdentity,
    pub uptime_secs: u64,
    /// Sessions in memory, sidechains included.
    pub sessions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EventsSubscribed {
    pub subscribed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ShuttingDown {
    pub shutting_down: bool,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct CommandsListParams {
    /// That session's own commands; omitted, the daemon's own pair's.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CommandList {
    pub commands: Vec<CommandInfo>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetHostToolsParams {
    /// Each entry as `options.host_tools` takes it; `[]` withdraws them all.
    /// Read one by one: a bad entry is refused alone.
    pub host_tools: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetHooksParams {
    /// Each entry a `HostHookOption`; `[]` or `null` withdraws them all.
    /// Read one by one: a bad entry is refused alone.
    #[serde(default)]
    pub hooks: Option<Vec<serde_json::Value>>,
}
