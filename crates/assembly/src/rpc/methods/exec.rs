//! `exec.*` 的 params 与线格式：嵌入方在沙箱里跑自己的命令。

use crate::session_pool::api::{HostExec, HostExecProfile};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A field this does not know fails the call: a misspelled `deny_read`
/// must not run the command with less held than the host asked for.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecRunParams {
    /// The program and its arguments; no shell reads them.
    pub argv: Vec<String>,
    /// An absolute path to a directory.
    pub cwd: PathBuf,
    /// `read` writes only the call's own `TMPDIR`; `write` also `writable`.
    #[serde(default)]
    pub profile: HostExecProfile,
    /// Absolute paths that exist; only with `profile: "write"`.
    #[serde(default)]
    pub writable: Vec<PathBuf>,
    #[serde(default)]
    pub network: bool,
    /// Over the minimal environment (`PATH`, `HOME`, `USER`, the locale,
    /// `TZ`); `TMPDIR` is the call's own. No `LD_*` / `DYLD_*`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Absolute paths kept from being read, on top of the credential list.
    #[serde(default)]
    pub deny_read: Vec<PathBuf>,
    #[serde(default)]
    pub stdin: Option<String>,
    /// Default 60000, at most 1800000.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Of each stream. Default 1048576, at most 4194304.
    #[serde(default)]
    pub max_output_bytes: Option<u64>,
    /// Run unconfined when the sandbox cannot hold the command at all.
    #[serde(default)]
    pub allow_unconfined: bool,
    /// What `exec.cancel` names this call by, on this connection.
    #[serde(default)]
    pub run_id: Option<String>,
}

impl ExecRunParams {
    pub fn into_request(self) -> HostExec {
        HostExec {
            argv: self.argv,
            cwd: self.cwd,
            profile: self.profile,
            writable: self.writable,
            network: self.network,
            env: self.env,
            deny_read: self.deny_read,
            stdin: self.stdin.map(String::into_bytes),
            timeout: self.timeout_ms.map(std::time::Duration::from_millis),
            max_output_bytes: self
                .max_output_bytes
                .map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
            allow_unconfined: self.allow_unconfined,
        }
    }
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ExecCancelParams {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ExecCancelled {
    /// `false`: no call of this connection is running under `run_id`.
    pub cancelled: bool,
}
