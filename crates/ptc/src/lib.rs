//! Embedded JavaScript that calls this session's tools, in two front-ends on
//! one runtime: `RunCode` runs a program the model writes (`docs/ptc.md`),
//! `RunExt` runs a command an installed plugin brings (`docs/runext.md`).
//!
//! Sub-calls go through the session's own dispatch (`ToolContext::dispatch`):
//! the same validation, hooks, middleware, telemetry and permission checks as
//! a call the model makes directly, except that one needing approval fails
//! instead of asking. A program is a running task of kind `code`, a command
//! one of kind `ext`.

mod bytecode;
mod calls;
mod code;
mod engine;
mod ext;
mod run;

pub use code::{RunCodeInput, RunCodeTool, TOOL_NAME};
pub use ext::{split_command_line, RunExtInput, RunExtTool, EXT_TOOL_NAME};

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

/// How far one run may go.
#[derive(Debug, Clone)]
pub struct Limits {
    pub time: Duration,
    /// The time limit once the program has started a sub-agent.
    pub time_with_agents: Duration,
    pub calls: u32,
    pub agents: u32,
    pub parallel: usize,
    pub result_chars: usize,
    pub call_result_chars: usize,
    pub memory_bytes: usize,
    pub code_chars: usize,
    pub notices: u32,
    /// How much of the program's log comes back with its result.
    pub log_chars: usize,
    /// How long a run asked to stop gets to wrap up before it is cut off.
    pub stop_grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            time: Duration::from_secs(5 * 60),
            time_with_agents: Duration::from_secs(60 * 60),
            calls: 1_000,
            agents: 100,
            parallel: 8,
            result_chars: 50_000,
            call_result_chars: 200_000,
            memory_bytes: 64 * 1024 * 1024,
            code_chars: 100_000,
            notices: 10,
            log_chars: 4_000,
            stop_grace: Duration::from_secs(10),
        }
    }
}

impl Limits {
    /// A plugin command's: two minutes, no sub-agents, an entry of up to
    /// 4 MiB.
    pub fn for_commands() -> Self {
        Self {
            time: Duration::from_secs(2 * 60),
            time_with_agents: Duration::from_secs(2 * 60),
            agents: 0,
            code_chars: 4 * 1024 * 1024,
            notices: 0,
            ..Self::default()
        }
    }
}

/// The global a script finds the base library (`docs/agix_js.md`) under: the
/// engine's name.
#[derive(Debug, Clone)]
pub struct Library {
    global: String,
}

impl Library {
    /// The library of the engine called `engine` (`base::paths::Layout::engine`),
    /// and the name the model is told the helpers are under.
    pub fn for_engine(engine: &str) -> Self {
        Self {
            global: engine.to_string(),
        }
    }

    pub fn global(&self) -> &str {
        &self.global
    }

    pub(crate) fn aliases(&self) -> Vec<&str> {
        vec![self.global.as_str()]
    }
}

impl Default for Library {
    fn default() -> Self {
        Self::for_engine(js_lib::GLOBAL)
    }
}

/// A fresh budget of how many scripts may run at once, foreground and
/// background together — each one is a thread of its own — sized for this
/// machine. Whoever hands one to several engines decides how far it reaches;
/// a session pool shares one among its sessions.
pub fn default_run_slots() -> Arc<Semaphore> {
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    Arc::new(Semaphore::new(cpus * 2))
}

/// What ended a run early, for the message that says so.
#[derive(Default)]
pub(crate) struct StopCause(Mutex<Option<String>>);

impl StopCause {
    pub(crate) fn set(&self, cause: impl Into<String>) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(cause.into());
        }
    }
    pub(crate) fn get(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

pub(crate) fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

pub(crate) fn truncate(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        (text.to_string(), false)
    } else {
        (text.chars().take(max_chars).collect(), true)
    }
}

/// Take one from `left` unless it is already zero.
pub(crate) fn take_one(left: &AtomicU32) -> bool {
    let mut n = left.load(Ordering::SeqCst);
    while n > 0 {
        match left.compare_exchange(n, n - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(seen) => n = seen,
        }
    }
    false
}

#[cfg(test)]
mod ext_tests;
#[cfg(test)]
mod tests;
