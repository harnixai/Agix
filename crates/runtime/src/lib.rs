//! Agix — agent runtime: turn loop, streaming, tool dispatch, agent lifecycle.

pub mod agent;
pub mod checkpoint;
pub mod checkpoint_settings;
pub mod commands;
pub mod elicitation;
mod elicitation_hooks;
pub mod event_bus;
mod exec;
pub mod hook_executors;
pub mod host_tools;
pub mod module;
pub mod plugin;
mod repeat_guard;
pub mod steering;
pub mod streaming;
mod tool_policy;
pub use agent::turn;

/// Whether this build carries `RunCode` (the `ptc` feature).
pub const PTC_ENABLED: bool = cfg!(feature = "ptc");
