//! What a session has spent and asked for over its turns — kept on its
//! `SessionState` (in `session-state`), so it outlives the engine that ran
//! them. Its token total is also in the session log (`agix.spend`), so it
//! outlives the process.

use std::sync::Arc;

use crate::interface::budget_policy::{OutputProgress, SpendNode};

#[derive(Debug, Clone)]
pub struct SessionLedger {
    /// The session's node: every call of its turns spends into it as it is
    /// made, their delegates' and those made on their side included, and
    /// `session.token_budget` is its ceiling.
    pub spent: Arc<SpendNode>,
    /// The goal's node, under the session's, that its rounds spend into.
    pub goal_spent: Option<GoalSpend>,
    /// What the session log last recorded of the two.
    pub spent_recorded: Option<SpendRecord>,
    pub calls: SessionCalls,
    pub token_target: TokenTarget,
    /// Skills invoked or announced so far, re-injected after compaction.
    pub invoked_skills: Vec<String>,
    /// Repositories nested in the project the session has already reported
    /// or been told to keep, with a digest of their configuration then.
    pub nested_repos: std::collections::BTreeMap<std::path::PathBuf, u64>,
    /// The repositories nested in the project before the session's first
    /// tool call ran, with a digest of their configuration then; `None`
    /// until that call.
    pub nested_repos_at_start: Option<std::collections::BTreeMap<std::path::PathBuf, u64>>,
}

impl Default for SessionLedger {
    fn default() -> Self {
        Self {
            spent: Arc::new(SpendNode::session(None)),
            goal_spent: None,
            spent_recorded: None,
            calls: SessionCalls::default(),
            token_target: TokenTarget::default(),
            invoked_skills: Vec::new(),
            nested_repos: Default::default(),
            nested_repos_at_start: None,
        }
    }
}

impl SessionLedger {
    /// [`Self::spend_record`] when the log does not have it yet; a session
    /// that has spent nothing has nothing to record.
    pub fn spend_unrecorded(&self) -> Option<SpendRecord> {
        let record = self.spend_record();
        let recorded = self.spent_recorded.clone().unwrap_or_default();
        (record != recorded).then_some(record)
    }

    /// What the session and its goal have spent, as the log records it.
    pub fn spend_record(&self) -> SpendRecord {
        SpendRecord {
            session: self.spent.read(),
            goal: self.goal_spent.as_ref().map(|goal| GoalSpendRecord {
                id: goal.goal_id.clone(),
                spent: goal.node.read(),
            }),
        }
    }

    /// A node for goal `goal_id` under the session's, holding `limit`, that
    /// has spent `spent` already.
    pub fn open_goal(
        &mut self,
        goal_id: &str,
        limit: Option<u64>,
        spent: crate::interface::budget_policy::Spend,
    ) -> Arc<SpendNode> {
        let node = Arc::new(SpendNode::goal(limit, self.spent.clone()));
        node.restore(spent);
        self.goal_spent = Some(GoalSpend {
            goal_id: goal_id.to_string(),
            node: node.clone(),
        });
        node
    }

    pub fn note_skills<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) {
        for name in names {
            if !self.invoked_skills.iter().any(|s| s == name) {
                self.invoked_skills.push(name.to_string());
            }
        }
    }
}

/// The model calls and tool calls of a session's own agent, summed over the
/// turns it ran since it was loaded.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionCalls {
    pub api_calls: u32,
    pub tool_calls: u32,
}

/// The node of the goal whose rounds spend into it.
#[derive(Debug, Clone)]
pub struct GoalSpend {
    pub goal_id: String,
    pub node: Arc<SpendNode>,
}

/// Log namespace and event under which a [`SpendRecord`] is written.
pub const SPEND_NS: &str = "agix.spend";
pub const SPEND_EVENT: &str = "total";

/// What a session has spent since it was created, and its goal since it was
/// set, as the session log records them.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpendRecord {
    pub session: crate::interface::budget_policy::Spend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalSpendRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GoalSpendRecord {
    pub id: String,
    pub spent: crate::interface::budget_policy::Spend,
}

impl SpendRecord {
    /// The latest of `records`, a session log's in order. Two writers record
    /// them — the engine and the session pool — and what was spent only
    /// grows, so each figure is the largest recorded, whichever landed last;
    /// a goal's, the largest recorded under its id.
    pub fn latest(records: impl IntoIterator<Item = SpendRecord>) -> Option<SpendRecord> {
        records.into_iter().reduce(|seen, next| {
            let goal = match (seen.goal, next.goal) {
                (Some(a), Some(b)) if a.id == b.id => Some(GoalSpendRecord {
                    id: a.id,
                    spent: a.spent.max(b.spent),
                }),
                (a, b) => b.or(a),
            };
            SpendRecord {
                session: seen.session.max(next.session),
                goal,
            }
        })
    }
}

/// An output token target, kept until it is met, given up on, or cleared —
/// a turn that ends otherwise leaves it running into the next.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenTarget {
    pub target: Option<u64>,
    /// Set by the message that set the target; the model has not been told yet.
    pub unannounced: bool,
    pub accumulated: u64,
    pub continuations: u32,
    pub last_delta: u64,
}

impl TokenTarget {
    /// No target, nothing counted toward one.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Aim at `target` from nothing, for the model to be told of it.
    pub fn aim(&mut self, target: u64) {
        *self = Self {
            target: Some(target),
            unannounced: true,
            ..Self::default()
        };
    }

    /// The target the model has not been told of yet, now told.
    pub fn take_unannounced(&mut self) -> Option<u64> {
        std::mem::take(&mut self.unannounced)
            .then_some(self.target)
            .flatten()
    }

    /// Count `this_delta` output tokens toward `target`.
    pub fn add_output(&mut self, target: u64, this_delta: u64) -> OutputProgress {
        self.accumulated = self.accumulated.saturating_add(this_delta);
        OutputProgress {
            accumulated: self.accumulated,
            target,
            continuations: self.continuations,
            this_delta,
            last_delta: self.last_delta,
        }
    }

    /// Record that the model was asked to keep going after `this_delta`
    /// output tokens; how many times it has been, now.
    pub fn continued(&mut self, this_delta: u64) -> u32 {
        self.continuations += 1;
        self.last_delta = this_delta;
        self.continuations
    }
}

#[cfg(test)]
mod tests {
    use super::TokenTarget;

    #[test]
    fn a_new_aim_carries_nothing_over_from_the_last_target() {
        let mut target = TokenTarget::default();
        target.aim(5_000);
        target.add_output(5_000, 700);
        target.continued(700);
        target.take_unannounced();

        target.aim(9_000);
        let progress = target.add_output(9_000, 10);
        assert_eq!(
            (
                target.unannounced,
                progress.accumulated,
                progress.continuations,
                progress.last_delta
            ),
            (true, 10, 0, 0)
        );
    }
}
