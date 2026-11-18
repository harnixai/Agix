//! How a tool calls other tools of its session — through the same dispatch a
//! model's own tool call takes (validation, hooks, middleware, telemetry,
//! permission), so a sub-call can do nothing its caller could not have done
//! directly. Handed to tools that ask for it (`ToolSpec::needs_dispatch`);
//! a skill's inline command takes the same dispatch through
//! [`crate::interface::inline_command::InlineCommands`].

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// A tool another tool can call by name.
#[derive(Debug, Clone)]
pub struct CallableTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// See `ToolSpec::code_result`.
    pub result: Option<String>,
}

/// What a sub-call produced: the result's text, its structured content when
/// the tool gave one, and whether the tool called it a failure. `Err` is the
/// call not running at all — unknown, not callable, refused, invalid.
#[derive(Debug, Clone)]
pub struct SubCallAnswer {
    pub text: String,
    pub structured: Option<Value>,
    pub reported_failure: bool,
}

/// What the sub-calls of one plugin command's run are held to: they are
/// that command's (`via` `RunExt:<command>`), a read-only run writes nothing,
/// and the files a write run may change are `writes`, globs relative to the
/// project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandScope {
    pub command: String,
    pub read_only: bool,
    pub writes: Vec<String>,
}

#[async_trait]
pub trait ToolDispatch: Send + Sync {
    /// The tools a sub-call may name.
    fn callable(&self) -> Vec<CallableTool>;

    /// Whether this call may run beside others of its kind.
    fn concurrency_safe(&self, name: &str, input: &Value) -> bool;

    /// Run one sub-call. A call that would need a person's approval is
    /// refused rather than asked.
    async fn call(
        &self,
        name: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> Result<SubCallAnswer, String>;

    /// The same dispatch for one plugin command's sub-calls. `None` where
    /// this dispatch cannot run a command's calls.
    fn for_command(&self, _scope: CommandScope) -> Option<std::sync::Arc<dyn ToolDispatch>> {
        None
    }
}
