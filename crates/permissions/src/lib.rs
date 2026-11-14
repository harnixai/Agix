//! Permission engine — rule matching, mode dispatch, path safety.

pub mod dangerous;
mod dangerous_variable;
pub mod error;
pub mod invocation;
pub mod path_rule;
pub mod path_safety;
pub mod policy;
pub mod rule;
pub mod rule_string;
pub mod ruleset;
pub mod shadow;
pub mod shell;
