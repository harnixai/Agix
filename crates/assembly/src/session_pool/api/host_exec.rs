//! 嵌入方在沙箱里跑自己的命令（`exec.run`）：请求说跑什么、能写哪里、能不能联网，
//! 结局说退出状态、截到上限的输出、以及沙箱实际做到了几成。不经模型、不进会话。

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// What a host command may write besides its own temporary directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HostExecProfile {
    /// Nothing but the call's temporary directory.
    #[default]
    Read,
    /// Also the request's `writable`.
    Write,
}

#[derive(Debug, Clone)]
pub struct HostExec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub profile: HostExecProfile,
    pub writable: Vec<PathBuf>,
    pub network: bool,
    pub env: BTreeMap<String, String>,
    /// Kept from being read on top of the built-in credential list and the
    /// settings' `sandbox.deny_read`.
    pub deny_read: Vec<PathBuf>,
    pub stdin: Option<Vec<u8>>,
    /// `None` is [`HOST_EXEC_DEFAULT_TIME`].
    pub timeout: Option<Duration>,
    /// Of each stream; `None` is [`HOST_EXEC_DEFAULT_OUTPUT`].
    pub max_output_bytes: Option<usize>,
    /// Run outside the sandbox when it cannot hold the command at all.
    pub allow_unconfined: bool,
}

pub const HOST_EXEC_DEFAULT_TIME: Duration = Duration::from_secs(60);
pub const HOST_EXEC_TIME_LIMIT: Duration = Duration::from_secs(30 * 60);
pub const HOST_EXEC_DEFAULT_OUTPUT: usize = 1024 * 1024;
pub const HOST_EXEC_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

/// How much of the policy held the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HostExecEnforcement {
    Full,
    Partial,
    None,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HostExecResult {
    /// `None` when a signal ended it.
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// Lossy UTF-8.
    pub stdout: String,
    pub stderr: String,
    /// Either stream went past `max_output_bytes`; the rest was dropped.
    pub truncated: bool,
    /// Stopped, with its process group, at the time limit.
    pub timed_out: bool,
    /// Stopped by `exec.cancel` or the connection closing.
    pub cancelled: bool,
    pub ms: u64,
    pub enforcement: HostExecEnforcement,
    /// What of the policy did not hold, one line each; empty under `full`.
    pub unmet: Vec<String>,
}
