//! The policy judged through the requests a session makes of it, and its
//! stages one at a time.

use super::*;
use crate::ruleset::{RuleHit, RuleSet};
use base::context::Session;
use base::interface::permission::{
    Answerable, Basis, ModeStance, PermissionFacts, PermissionOutcome, PermissionRequest, Reach,
    Stage, Subject, ToolFacts,
};
use base::paths::SensitivePaths;
use base::permission::{RuleBehavior, RuleSource};
use base::tool::{Tool, ToolContext};
use serde_json::Value;
use std::sync::Arc;

mod answering;
mod contract;
mod judging;
mod requests;

/// The context the runtime runs a call of `session`'s with, and what `tool`
/// says about the call in it.
async fn call_in(
    tool: Option<&Arc<dyn Tool>>,
    input: &Value,
    session: &Arc<dyn Session>,
) -> (ToolContext, ToolFacts) {
    let env = session.env();
    let mut ctx = session_state::testing::tool_context_for_test(
        session
            .exec()
            .unwrap_or_else(base::interface::exec::ExecProviders::in_process),
        env.cwd.clone(),
    );
    ctx.session_id = "s1".into();
    ctx.session = session.clone();
    let facts = match tool {
        Some(tool) => ToolFacts::gather(tool.as_ref(), input, &ctx).await,
        None => ToolFacts::unknown_tool(),
    };
    (ctx, facts)
}
