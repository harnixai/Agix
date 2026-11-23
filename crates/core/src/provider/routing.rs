//! What a session's code asks of the router its model calls go through, and
//! what the router tells it back. The router itself — the providers of a
//! pair, their health and quota, the per-session order — is the model
//! crate's (`model::routing`); runtime and the modules see only this.

use std::sync::Arc;

use crate::interface::model::Model;

/// Where a session's model calls go: the model each kind of call goes
/// through, and what a model or tier name stands for at the provider calls
/// go to first.
pub trait TaskRouting: Send + Sync {
    /// The model `task`'s calls go through. A task with no tier of its own
    /// asks for `strong`.
    fn model_for(&self, task: &str) -> Arc<dyn Model>;

    /// The model `task` goes to at the head of the order; `None` for a task
    /// that uses no model.
    fn model_name_for(&self, task: &str) -> Option<String>;

    /// The model `name` stands for at the head: its model of that tier when
    /// `name` is a tier, else `name`.
    fn resolve_model(&self, name: &str) -> String;

    /// Report this session's provider switches to `observer`. The first
    /// observer stays.
    fn observe(&self, observer: Arc<dyn RouteObserver>);
}

/// A call that went to a different provider than the call before it.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ProviderSwitch {
    pub from: String,
    pub to: String,
    /// Why each provider before `to` did not take the call; when none was
    /// passed over, that `to` can take calls again.
    pub reason: String,
    /// The kind of call: `main`, `subagent`, `compact`, ….
    pub task: String,
}

/// Told about every [`ProviderSwitch`] of a session's calls.
pub trait RouteObserver: Send + Sync {
    fn switched(&self, switch: &ProviderSwitch);
}
