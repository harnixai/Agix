//! Tool trait. All tools implement this single trait.

mod context;
mod registry;
mod result;
mod schema;

pub use context::*;
pub use registry::*;
pub use result::*;
pub use schema::*;

use crate::error::ToolError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub use crate::context::config::PathStyle;
use crate::interface::permission::{ModeStance, PermissionFacts};
pub use crate::permission::PermissionMode;
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PromptContext {
    pub cwd: PathBuf,
    pub model: String,
    pub session_id: String,
    pub is_interactive: bool,
    pub all_tool_names: Vec<String>,
    pub allowed_agent_types: Vec<String>,
    pub path_style: PathStyle,
    /// The name of the instance's project directory, for a guide that tells
    /// the model where something lives.
    pub project_dir: String,
}
impl Default for PromptContext {
    fn default() -> Self {
        Self {
            cwd: PathBuf::new(),
            model: String::new(),
            session_id: String::new(),
            is_interactive: false,
            all_tool_names: vec![],
            allowed_agent_types: vec![],
            path_style: PathStyle::default(),
            project_dir: crate::paths::DEFAULT_PROJECT_DIR.to_string(),
        }
    }
}

/// Where a tool came from, for [`crate::interface::model::ToolDef::source`].
///
/// A name prefix (`mcp__`) already implies the answer, but a naming
/// convention is not a declaration: it holds only for as long as nobody
/// registers a builtin whose name happens to start that way. An adapter that
/// knows which server or plugin it is fronting says so here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ToolOrigin {
    #[default]
    Builtin,
    Mcp {
        server: String,
    },
    Plugin {
        name: String,
    },
    /// Declared by the host for one session; calling it goes to the host.
    Host,
}

impl ToolOrigin {
    pub fn mcp(server: impl Into<String>) -> Self {
        Self::Mcp {
            server: server.into(),
        }
    }

    pub fn plugin(name: impl Into<String>) -> Self {
        Self::Plugin { name: name.into() }
    }

    pub fn mcp_server(&self) -> Option<&str> {
        match self {
            Self::Mcp { server } => Some(server),
            Self::Builtin | Self::Plugin { .. } | Self::Host => None,
        }
    }

    pub fn is_mcp(&self) -> bool {
        self.mcp_server().is_some()
    }

    /// `builtin`, `mcp:<server>`, `plugin:<name>` or `host`.
    pub fn wire(&self) -> String {
        match self {
            Self::Builtin => "builtin".to_string(),
            Self::Mcp { server } => format!("mcp:{server}"),
            Self::Plugin { name } => format!("plugin:{name}"),
            Self::Host => "host".to_string(),
        }
    }
}

/// What a tool says about one call: answered by [`Tool::call_traits`], which
/// is handed the call's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct CallTraits {
    pub read_only: bool,
    pub concurrency_safe: bool,
    pub destructive: bool,
}

impl CallTraits {
    pub fn read_only(mut self, yes: bool) -> Self {
        self.read_only = yes;
        self
    }
    pub fn concurrency_safe(mut self, yes: bool) -> Self {
        self.concurrency_safe = yes;
        self
    }
    pub fn destructive(mut self, yes: bool) -> Self {
        self.destructive = yes;
        self
    }
}

/// What a tool says about itself that holds for every call. Built with
/// [`ToolSpec::new`] and the setters: `ToolSpec::new().read_only(true)`.
///
/// `traits` is the answer when the input is not looked at. A tool whose answer
/// depends on the input overrides [`Tool::call_traits`] too, and puts its
/// conservative answer here.
///
/// The input schema is not here: it is built on every call and the spec is
/// asked for far more often than the schema.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct ToolSpec {
    /// What `ToolSearch` lists next to the name; the description when unset.
    pub short_description: Option<String>,
    pub origin: ToolOrigin,
    pub traits: CallTraits,
    /// Whether a program the model writes (`RunCode`) may call this tool.
    /// Tools that do work and return data say yes; tools that talk to the
    /// conversation or a person do not.
    pub callable_from_code: bool,
    /// What a program gets back from this tool, when that is more than the
    /// text the model sees — the shape of its structured content.
    pub code_result: Option<&'static str>,
    /// Whether this tool calls other tools, and so needs
    /// [`ToolContext::dispatch`].
    pub needs_dispatch: bool,
    /// How the permission modes treat this tool's calls.
    pub stance: ModeStance,
    /// The tool lets the model read a long result in parts (an offset), so
    /// the engine does not save a long result to a file for it.
    pub pages_itself: bool,
}

impl ToolSpec {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn short_description(mut self, text: impl Into<String>) -> Self {
        self.short_description = Some(text.into());
        self
    }
    pub fn origin(mut self, origin: ToolOrigin) -> Self {
        self.origin = origin;
        self
    }
    pub fn traits(mut self, traits: CallTraits) -> Self {
        self.traits = traits;
        self
    }
    pub fn read_only(mut self, yes: bool) -> Self {
        self.traits.read_only = yes;
        self
    }
    pub fn concurrency_safe(mut self, yes: bool) -> Self {
        self.traits.concurrency_safe = yes;
        self
    }
    pub fn destructive(mut self, yes: bool) -> Self {
        self.traits.destructive = yes;
        self
    }
    pub fn callable_from_code(mut self, yes: bool) -> Self {
        self.callable_from_code = yes;
        self
    }
    pub fn code_result(mut self, shape: &'static str) -> Self {
        self.code_result = Some(shape);
        self
    }
    pub fn pages_itself(mut self) -> Self {
        self.pages_itself = true;
        self
    }
    pub fn needs_dispatch(mut self, yes: bool) -> Self {
        self.needs_dispatch = yes;
        self
    }
    pub fn stance(mut self, stance: ModeStance) -> Self {
        self.stance = stance;
        self
    }
}

/// A tool the model can call.
///
/// What holds for every call is one [`spec`](Self::spec); what depends on the
/// call's input is [`call_traits`](Self::call_traits), which defaults to the
/// spec's answer. [`permission_facts`](Self::permission_facts) is what the
/// permission engine weighs about the call; a tool that states none is asked
/// about.
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// Everything about this tool that holds for every call — see
    /// [`ToolSpec`].
    fn spec(&self) -> ToolSpec;
    /// The input schema as worded for a session's [`PathStyle`]; only tools
    /// whose argument docs mention paths read `style`.
    fn input_schema(&self, style: PathStyle) -> Value;

    /// This tool's detailed usage guide, when it has one that says more than
    /// `description()` already does.
    ///
    /// `ToolDef`s carry only `description()`; the full guides total ~85 KB
    /// across the built-in set, too much to pay on every API call, so the model
    /// fetches them on demand via `ToolSearch{query: "select:<name>"}`.
    async fn guide(&self, _ctx: &PromptContext) -> Option<String> {
        None
    }

    /// What this call reads, writes and may run beside; `spec().traits` unless
    /// the answer depends on the input.
    fn call_traits(&self, _input: &Value) -> CallTraits {
        self.spec().traits
    }

    async fn permission_facts(&self, _: &Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::unknown()
    }
    async fn validate_input(&self, _: &Value, _: &ToolContext) -> ValidationResult {
        ValidationResult::Ok
    }
    async fn call(
        &self,
        i: Value,
        c: ToolContext,
        p: ProgressSender,
    ) -> Result<ToolResult, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    struct F;
    #[async_trait]
    impl Tool for F {
        fn name(&self) -> &str {
            "f"
        }
        fn description(&self) -> &str {
            "f"
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec::new().read_only(true).concurrency_safe(true)
        }
        fn input_schema(&self, _: PathStyle) -> Value {
            serde_json::json!({})
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }
    #[test]
    fn empty() {
        assert!(InMemoryToolRegistry::new().list().is_empty());
    }
    #[test]
    fn find() {
        let r = InMemoryToolRegistry::new();
        r.register(Arc::new(F));
        assert!(r.get("f").is_some());
    }

    /// A tool that says nothing about where it came from is a builtin, so the
    /// ones that say otherwise (MCP) are the exceptions — a newly written
    /// builtin cannot forget to declare itself.
    #[test]
    fn a_tool_that_says_nothing_is_a_builtin() {
        assert_eq!(F.spec().origin, ToolOrigin::Builtin);
        assert_eq!(ToolOrigin::Builtin.wire(), "builtin");
        assert_eq!(ToolOrigin::mcp("github").wire(), "mcp:github");
    }
}

#[cfg(test)]
mod spec_tests {
    use super::*;

    async fn unreachable_call() -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("ok"))
    }

    struct Plain;
    #[async_trait]
    impl Tool for Plain {
        fn name(&self) -> &str {
            "Plain"
        }
        fn description(&self) -> &str {
            "says nothing about itself"
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec::new()
        }
        fn input_schema(&self, _: PathStyle) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            unreachable_call().await
        }
    }

    struct Declared;
    #[async_trait]
    impl Tool for Declared {
        fn name(&self) -> &str {
            "Declared"
        }
        fn description(&self) -> &str {
            "declares a spec"
        }
        fn input_schema(&self, _: PathStyle) -> Value {
            serde_json::json!({"type": "object"})
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec::new()
                .origin(ToolOrigin::mcp("x"))
                .concurrency_safe(true)
                .destructive(true)
                .needs_dispatch(true)
                .stance(ModeStance::LeavesPlan)
                .short_description("short")
        }
        fn call_traits(&self, input: &Value) -> CallTraits {
            self.spec().traits.read_only(input.get("peek").is_some())
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            unreachable_call().await
        }
    }

    #[tokio::test]
    async fn a_tool_that_declares_nothing_gets_the_defaults() {
        assert_eq!(Plain.spec(), ToolSpec::new());
        assert_eq!(Plain.call_traits(&Value::Null), CallTraits::default());
        assert_eq!(Plain.spec().origin, ToolOrigin::Builtin);
        assert_eq!(Plain.spec().stance, ModeStance::Normal);
        assert!(Plain.guide(&PromptContext::default()).await.is_none());
    }

    #[test]
    fn a_declared_spec_is_what_the_engine_reads() {
        let spec = Declared.spec();
        assert_eq!(spec.origin.mcp_server(), Some("x"));
        assert!(spec.traits.concurrency_safe && spec.traits.destructive);
        assert!(spec.needs_dispatch);
        assert_eq!(spec.stance, ModeStance::LeavesPlan);
        assert!(!spec.callable_from_code);
        assert_eq!(spec.short_description.as_deref(), Some("short"));
    }

    #[test]
    fn call_traits_that_read_the_input_win_over_the_spec() {
        assert!(!Declared.spec().traits.read_only);
        assert!(
            Declared
                .call_traits(&serde_json::json!({"peek": 1}))
                .read_only
        );
        assert!(!Declared.call_traits(&serde_json::json!({})).read_only);
    }
}
