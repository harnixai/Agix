//! `PromptAssembler` — the algorithm that turns every contribution into the
//! blocks one request carries.
//!
//! Contributing a block and rewriting one are already open: a registration
//! adds, an `AssemblyHook` gets a last pass over the result. What was not open
//! is the assembly itself — the order stages are placed in, how a
//! contribution's order merges with the kernel's, where the cache boundaries
//! fall, whether two blocks stay two blocks. Those are decisions
//! [`assemble_prompt_with`](crate::prompt::assemble_prompt_with) makes, and a
//! deployment that wants different ones had to make them by bolting a hook on
//! the end and undoing the arrangement it had just been handed.
//!
//! # The inputs are a value
//!
//! Assembly reads eight things. Passing them as a struct is not a style
//! preference: an implementation that wants only two should not have to
//! restate the other five, and a new input should not break every
//! implementation that never asked for one.

use crate::interface::prompt_registry::PromptRegistry;
use crate::interface::scene::{ScenePromptContext, SceneSpec};
use crate::prompt::PromptBlock;

/// Everything one assembly reads.
pub struct AssemblyRequest<'a> {
    /// What else asked to be in this prompt.
    pub registry: &'a dyn PromptRegistry,
    pub scene: &'a SceneSpec,
    /// `settings.prompt.override`: when set, it is the whole prompt.
    pub prompt_override: Option<&'a str>,
    /// `settings.prompt.append`.
    pub prompt_append: Option<&'a str>,
    /// The memory section, already rendered; `None` when memory is off.
    pub memory_prompt: Option<&'a str>,
    pub ctx: &'a ScenePromptContext<'a>,
    /// The rule documents the model is told exist, already merged.
    pub rules: &'a [crate::rules::RuleEntry],
    /// The skills inventory, already rendered.
    pub skills_text: Option<&'a str>,
    /// Usage guidance from the connected MCP servers, already rendered.
    pub mcp_instructions: Option<&'a str>,
}

/// How a request's system prompt gets built.
pub trait PromptAssembler: Send + Sync {
    fn assemble(&self, request: &AssemblyRequest<'_>) -> Vec<PromptBlock>;
}
