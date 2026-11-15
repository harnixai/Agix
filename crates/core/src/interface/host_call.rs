//! `HostCalls` — how the engine hands the host something only the host can
//! do, and waits for its one answer: a tool the host declared, or a hook a
//! host connection registered. Both share the wait, timeout, cancellation and
//! disconnect handling; a call that names its [`HostId`] is answered by that
//! connection alone.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::interface::tool::{CallTraits, ProgressSender, ToolResult};

/// The host connection that provides a tool or a hook, rather than the
/// session's watchers at large.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostId(pub u64);

/// A tool the host declared for a session: the model is offered it as it is
/// offered any other, and calling it goes to the host.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct HostToolDecl {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub traits: CallTraits,
    pub timeout: Duration,
    /// Offered by name only; the schema comes from `ToolSearch`.
    pub deferred: bool,
    /// The session's sub-agents are offered it too.
    pub subagents: bool,
    /// The connection that provides it; `None` for the session's own.
    pub host: Option<HostId>,
}

impl HostToolDecl {
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
    pub const MIN_TIMEOUT: Duration = Duration::from_secs(1);
    pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);

    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            traits: CallTraits::default(),
            timeout: Self::DEFAULT_TIMEOUT,
            deferred: false,
            subagents: false,
            host: None,
        }
    }

    pub fn traits(mut self, traits: CallTraits) -> Self {
        self.traits = traits;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn deferred(mut self, yes: bool) -> Self {
        self.deferred = yes;
        self
    }

    pub fn subagents(mut self, yes: bool) -> Self {
        self.subagents = yes;
        self
    }

    pub fn host(mut self, host: HostId) -> Self {
        self.host = Some(host);
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum HostCallKind {
    /// Run the declared tool `name` on `input`, already checked against its
    /// schema.
    Tool { name: String, input: Value },
    /// Answer the hook `event` on `input`, the payload a command hook reads.
    Hook { event: String, input: Value },
}

/// One call put to the host.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HostCall {
    /// The engine's own id for the call, unique in the session the host
    /// answers: a provider's `tool_use_id` is unique only within one reply,
    /// and a parent and its sub-agent can reuse one.
    pub call_id: String,
    pub tool_use_id: String,
    pub kind: HostCallKind,
    pub timeout: Duration,
    /// The background task of the session the host watches that the call
    /// was made under, when a sub-agent made it.
    pub task_id: Option<String>,
    /// The sub-agent's own session, when a sub-agent made it.
    pub agent_session_id: Option<String>,
    /// The one connection whose answer counts; `None`: any watcher's.
    pub host: Option<HostId>,
    /// Where the host's progress reports on a tool call go.
    pub progress: Option<ProgressSender>,
}

impl HostCall {
    pub fn tool(
        call_id: impl Into<String>,
        tool_use_id: impl Into<String>,
        name: impl Into<String>,
        input: Value,
        timeout: Duration,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool_use_id: tool_use_id.into(),
            kind: HostCallKind::Tool {
                name: name.into(),
                input,
            },
            timeout,
            task_id: None,
            agent_session_id: None,
            host: None,
            progress: None,
        }
    }

    pub fn hook(
        call_id: impl Into<String>,
        tool_use_id: impl Into<String>,
        event: impl Into<String>,
        input: Value,
        timeout: Duration,
    ) -> Self {
        Self {
            kind: HostCallKind::Hook {
                event: event.into(),
                input,
            },
            ..Self::tool(call_id, tool_use_id, String::new(), Value::Null, timeout)
        }
    }

    pub fn for_host(mut self, host: Option<HostId>) -> Self {
        self.host = host;
        self
    }

    pub fn with_progress(mut self, progress: ProgressSender) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Made by a sub-agent: its session, if no deeper one is named already,
    /// and the task it runs as, replacing a deeper one's — the host sees the
    /// tasks of the session it watches.
    pub fn from_delegate(mut self, task_id: Option<String>, agent_session_id: &str) -> Self {
        if self.agent_session_id.is_none() {
            self.agent_session_id = Some(agent_session_id.to_string());
        }
        if task_id.is_some() {
            self.task_id = task_id;
        }
        self
    }
}

/// What the host said.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum HostReply {
    /// It ran the call; the result may itself say it failed (`is_error`).
    Result(ToolResult),
    /// It could not run the call.
    Failed(String),
    /// Its answer to a hook call: a hook response object.
    Hook(Value),
}

/// How a call to the host ended.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum HostCallEnd {
    Replied(HostReply),
    TimedOut,
    /// Nobody was there to answer when it was asked.
    NoHost,
    /// Everyone who could answer went away before anyone did.
    HostGone,
    Cancelled,
}

#[async_trait]
pub trait HostCalls: Send + Sync {
    /// Put `call` to the host and wait for how it ends: an answer, its
    /// timeout, the host's absence, or `cancel`.
    async fn call(&self, call: HostCall, cancel: &CancellationToken) -> HostCallEnd;
}
