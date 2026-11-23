//! What a turn is allowed to spend, and how much context it may carry: the
//! engine's ceilings ([`SpendNode`]) and the deployment's own judgement
//! ([`BudgetPolicy`]).
//!
//! # Two ceilings that look alike and are not
//!
//! `Spending` is about the bill, accumulated and checked against what the
//! provider actually reported. `OutputTarget` is about the opposite problem —
//! the model stopping short of the volume of work that was asked for. One ends
//! a turn that has spent too much, the other prolongs a turn that has produced
//! too little. They stay separate methods because merging them would let
//! "write more" and "you have spent enough" argue with each other inside one
//! verdict.
//!
//! # One count, one chain
//!
//! Every call is counted once, where it is routed: the router (or the meter
//! that stands in for one) adds its usage to the [`SpendNode`] the call's
//! [`UsageTap`](crate::interface::model::UsageTap) is charged to. That node is
//! the turn the call belongs to; above it is the turn that delegated it, and
//! above a root turn its goal (for a goal round) and its session. A call
//! spends into every node up the chain, and the engine's ceilings — a turn's,
//! a goal's, a session's — are judged together: the next call may ask for no
//! more than the tightest of them leaves ([`SpendNode::remaining`]).
//!
//! The engine counts [`Spend::counted_tokens`] — input not read from the cache,
//! cache writes and output — against every ceiling and provider quota alike. A
//! cache read is billed at a fraction of ordinary input, and leaving it out is
//! what keeps a ceiling from moving with the cache hit rate. [`Spend`] keeps
//! the four figures apart, so a [`BudgetPolicy`] that wants another reading
//! has the parts; it is judged in addition to the engine's ceilings, never in
//! their place.
//!
//! `ContextBudget` is neither: it is about the size of a single request, not
//! the cost of the turn.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::interface::scene::TokenBudget;

/// What has been spent so far, as the provider reported it, kept apart by
/// kind because each kind is priced differently.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub struct Spend {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens written into the prompt cache, and tokens read back from it.
    #[serde(rename = "cache_creation_input_tokens")]
    pub cache_creation_tokens: u64,
    #[serde(rename = "cache_read_input_tokens")]
    pub cache_read_tokens: u64,
}

impl Spend {
    /// Input, cache writes and output: what every ceiling and quota of the
    /// engine counts. Cache reads are left out.
    pub fn counted_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_creation_tokens)
            .saturating_add(self.output_tokens)
    }

    /// Every token the provider reported, cache reads included.
    pub fn all_tokens(&self) -> u64 {
        self.counted_tokens().saturating_add(self.cache_read_tokens)
    }

    /// As the four-field usage a turn reports, each figure held at
    /// `u32::MAX`.
    pub fn as_usage(&self) -> crate::interface::model::Usage {
        let narrow = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
        crate::interface::model::Usage {
            input_tokens: narrow(self.input_tokens),
            output_tokens: narrow(self.output_tokens),
            cache_creation_input_tokens: narrow(self.cache_creation_tokens),
            cache_read_input_tokens: narrow(self.cache_read_tokens),
        }
    }

    /// Figure by figure, the larger of the two.
    pub fn max(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.max(other.input_tokens),
            output_tokens: self.output_tokens.max(other.output_tokens),
            cache_creation_tokens: self.cache_creation_tokens.max(other.cache_creation_tokens),
            cache_read_tokens: self.cache_read_tokens.max(other.cache_read_tokens),
        }
    }
}

impl From<crate::interface::model::Usage> for Spend {
    fn from(u: crate::interface::model::Usage) -> Self {
        Self {
            input_tokens: u.input_tokens as u64,
            output_tokens: u.output_tokens as u64,
            cache_creation_tokens: u.cache_creation_input_tokens as u64,
            cache_read_tokens: u.cache_read_input_tokens as u64,
        }
    }
}

/// Whose ceiling a [`SpendNode`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Turn,
    Goal,
    Session,
}

impl Scope {
    pub fn whose(self) -> &'static str {
        match self {
            Self::Turn => "this turn's",
            Self::Goal => "the goal's",
            Self::Session => "this session's",
        }
    }
}

/// What one turn, goal or session has spent and the engine's ceiling on it,
/// with the node above it that it spends into as well.
///
/// A turn's own calls, those made on its side (compaction, memory, web
/// extraction) and those of the delegates it started all spend into its node:
/// a delegate's turn sits under the turn that started it, and keeps spending
/// into it after that turn has ended. A root turn sits under its goal, for a
/// goal round, and its session.
#[derive(Debug)]
pub struct SpendNode {
    scope: Scope,
    /// `0` is no ceiling.
    limit: AtomicU64,
    all: Tally,
    delegated: Tally,
    above: Option<Arc<SpendNode>>,
    /// A delegate turn's own session, which it counts into without being
    /// judged by: the chain it is judged by runs through the turn above it.
    own_session: Option<Arc<SpendNode>>,
}

impl Default for SpendNode {
    fn default() -> Self {
        Self::turn(None, None)
    }
}

impl SpendNode {
    pub fn session(limit: Option<u64>) -> Self {
        Self::node(Scope::Session, limit, None)
    }

    pub fn goal(limit: Option<u64>, session: Arc<SpendNode>) -> Self {
        Self::node(Scope::Goal, limit, Some(session))
    }

    /// A root turn under its goal or session, or a delegate's under the
    /// turn that started it.
    pub fn turn(limit: Option<u64>, above: Option<Arc<SpendNode>>) -> Self {
        Self::node(Scope::Turn, limit, above)
    }

    fn node(scope: Scope, limit: Option<u64>, above: Option<Arc<SpendNode>>) -> Self {
        Self {
            scope,
            limit: AtomicU64::new(limit.unwrap_or(0)),
            all: Tally::default(),
            delegated: Tally::default(),
            above,
            own_session: None,
        }
    }

    /// A delegate turn that also counts into `session`, its own.
    pub fn counting_into(mut self, session: Arc<SpendNode>) -> Self {
        self.own_session = Some(session);
        self
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    pub fn limit(&self) -> Option<u64> {
        Some(self.limit.load(Ordering::Relaxed)).filter(|&l| l > 0)
    }

    pub fn set_limit(&self, limit: Option<u64>) {
        self.limit.store(limit.unwrap_or(0), Ordering::Relaxed);
    }

    /// One call, made by this node's agent or on its side.
    pub fn add(&self, call: crate::interface::model::Usage) {
        let call = Spend::from(call);
        self.all.add(call);
        if let Some(own) = &self.own_session {
            own.all.add(call);
        }
        let mut delegated = self.scope == Scope::Turn;
        let mut above = self.above.as_deref();
        while let Some(node) = above {
            node.all.add(call);
            if delegated && node.scope == Scope::Turn {
                node.delegated.add(call);
            }
            delegated &= node.scope == Scope::Turn;
            above = node.above.as_deref();
        }
    }

    /// This node, everything under it included.
    pub fn read(&self) -> Spend {
        self.all.read()
    }

    /// The part of a turn's [`Self::read`] its delegates spent.
    pub fn delegated(&self) -> Spend {
        self.delegated.read()
    }

    /// Start from what the session log recorded, before anything spends.
    pub fn restore(&self, spent: Spend) {
        self.all.set(spent);
    }

    /// The root turn and every delegate under it; for a node that is not a
    /// turn, its own reading.
    pub fn whole_turn(&self) -> Spend {
        let mut turn = self;
        while let Some(above) = turn.above.as_deref().filter(|a| a.scope == Scope::Turn) {
            turn = above;
        }
        turn.read()
    }

    /// The nearest node up the chain, this one included, that is `scope`'s.
    pub fn find(&self, scope: Scope) -> Option<&SpendNode> {
        self.chain().find(|node| node.scope == scope)
    }

    fn chain(&self) -> impl Iterator<Item = &SpendNode> {
        std::iter::successors(Some(self), |node| node.above.as_deref())
    }

    fn ceilings(&self) -> impl Iterator<Item = (Scope, u64, u64)> + '_ {
        self.chain().filter_map(|node| {
            let limit = node.limit()?;
            Some((node.scope, limit, node.read().counted_tokens()))
        })
    }

    /// What is left under the tightest ceiling up the chain, and whose it
    /// is; `None` with no ceiling.
    pub fn remaining(&self) -> Option<(Scope, Remaining)> {
        self.ceilings()
            .map(|(scope, limit, spent)| {
                let tokens = limit.saturating_sub(spent);
                (scope, Remaining { tokens, limit })
            })
            .min_by_key(|(_, left)| left.tokens)
    }

    /// Whether to keep spending under the ceilings up the chain: the first
    /// one reached ends the turn, and the nearest one past 90% has the model
    /// told to wrap up.
    pub fn judge(&self) -> (Scope, Spending) {
        let mut warned = None;
        for (scope, limit, spent) in self.ceilings() {
            if spent >= limit {
                return (scope, Spending::Exhausted { limit });
            }
            if warned.is_none() && spent >= limit.saturating_mul(9) / 10 {
                warned = Some((scope, limit));
            }
        }
        match warned {
            Some((scope, limit)) => (
                scope,
                Spending::Warn {
                    reminder: format!(
                        "<system-reminder>\n{} token budget is nearly exhausted. Keep your \
                         response concise and wrap up.\n</system-reminder>",
                        capitalized(scope.whose())
                    ),
                    limit,
                },
            ),
            None => (Scope::Turn, Spending::WithinBudget),
        }
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Why a turn ended `budget_exceeded`: whose ceiling was reached, for the
/// turn's `message`.
pub fn exhausted_message(scope: Scope, limit: u64) -> String {
    format!("{} token budget of {limit} is used up", scope.whose())
}

#[derive(Debug, Default)]
struct Tally {
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    cache_creation_tokens: AtomicU64,
    cache_read_tokens: AtomicU64,
}

impl Tally {
    fn add(&self, call: Spend) {
        for (total, part) in [
            (&self.input_tokens, call.input_tokens),
            (&self.output_tokens, call.output_tokens),
            (&self.cache_creation_tokens, call.cache_creation_tokens),
            (&self.cache_read_tokens, call.cache_read_tokens),
        ] {
            total.fetch_add(part, Ordering::Relaxed);
        }
    }

    fn set(&self, to: Spend) {
        self.input_tokens.store(to.input_tokens, Ordering::Relaxed);
        self.output_tokens
            .store(to.output_tokens, Ordering::Relaxed);
        self.cache_creation_tokens
            .store(to.cache_creation_tokens, Ordering::Relaxed);
        self.cache_read_tokens
            .store(to.cache_read_tokens, Ordering::Relaxed);
    }

    fn read(&self) -> Spend {
        Spend {
            input_tokens: self.input_tokens.load(Ordering::Relaxed),
            output_tokens: self.output_tokens.load(Ordering::Relaxed),
            cache_creation_tokens: self.cache_creation_tokens.load(Ordering::Relaxed),
            cache_read_tokens: self.cache_read_tokens.load(Ordering::Relaxed),
        }
    }
}

/// What is left under a ceiling before the next model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remaining {
    pub tokens: u64,
    pub limit: u64,
}

/// Whether the turn may keep spending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spending {
    WithinBudget,
    /// Close enough to say so. The turn continues.
    Warn {
        reminder: String,
        /// The ceiling this was measured against, for the record the engine
        /// keeps of budget enforcement.
        limit: u64,
    },
    /// The turn ends, reporting `budget_exceeded`.
    Exhausted {
        limit: u64,
    },
}

/// Progress toward an output-volume target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputProgress {
    /// Output tokens accumulated across the turn.
    pub accumulated: u64,
    pub target: u64,
    /// How many times the model has already been asked to keep going.
    pub continuations: u32,
    /// Output tokens from the call that just returned.
    pub this_delta: u64,
    /// Output tokens from the call before it.
    pub last_delta: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputTarget {
    /// Enough, or not worth asking again. The turn ends normally.
    Reached,
    /// Say this and go around again.
    KeepGoing { nudge: String },
}

/// How large a single request's context may get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// Compact once the context passes this. `0` disables compaction.
    pub compact_threshold: usize,
    /// Rounds compaction keeps.
    pub compact_keep_recent: usize,
    /// A ceiling compaction is not allowed to leave the context above. `None`
    /// is what the engine has always done — the threshold said when to start
    /// compacting and nothing said when to stop trying.
    pub hard_cap: Option<usize>,
}

/// A deployment's judgement of what a turn may spend and how much it may
/// carry, on top of the engine's own ceilings.
pub trait BudgetPolicy: Send + Sync {
    /// After a model call, against the root turn's reading
    /// ([`SpendNode::whole_turn`]) then. A turn the engine's ceilings
    /// already ended does not ask.
    fn on_usage(&self, _spend: &Spend) -> Spending {
        Spending::WithinBudget
    }

    /// Before a model call, against the root turn's reading. The call is not
    /// sent when less than [`MIN_CALL_TOKENS`] remain under this or any of
    /// the engine's ceilings, and otherwise asks for no more output than the
    /// tightest leaves. `None`: no ceiling of the policy's own.
    fn remaining(&self, _spend: &Spend) -> Option<Remaining> {
        None
    }

    /// When the model has stopped and a volume of output was asked for.
    ///
    /// Last of the three that can act on a reply cut off at `max_tokens`:
    /// [`TurnPolicy::after_model_call`] can end the turn first,
    /// [`RecoveryPolicy::on_early_stop`] can retry it with a higher limit
    /// first, and a `Stop` hook that ends or sends back the turn comes first
    /// too.
    ///
    /// [`TurnPolicy::after_model_call`]: crate::interface::turn_policy::TurnPolicy::after_model_call
    /// [`RecoveryPolicy::on_early_stop`]: crate::interface::recovery_policy::RecoveryPolicy::on_early_stop
    fn on_output_target(&self, _progress: &OutputProgress) -> OutputTarget {
        OutputTarget::Reached
    }

    /// What the scene asked for, and what the deployment will actually allow.
    fn context_budget(&self, scene: &TokenBudget) -> ContextBudget {
        ContextBudget {
            compact_threshold: scene.compact_threshold,
            compact_keep_recent: scene.compact_keep_recent,
            hard_cap: None,
        }
    }
}

/// Below this much remaining budget a model call is not sent.
pub const MIN_CALL_TOKENS: u64 = 1024;

/// The engine's own numbers for what a policy judges: the output target and
/// the compaction settings. Its token ceilings are on the [`SpendNode`]s.
#[derive(Default)]
pub struct EngineBudget {
    /// `settings.compaction`: over the scene's threshold and kept rounds when
    /// set.
    pub compact_threshold: Option<usize>,
    pub compact_keep_recent: Option<usize>,
}

impl BudgetPolicy for EngineBudget {
    fn on_output_target(&self, p: &OutputProgress) -> OutputTarget {
        // 90% of the target counts as met, and three consecutive
        // sub-500-token calls count as the model having run out of things to
        // say rather than needing another nudge.
        let met = (p.target as f64 * 0.9) as u64;
        let diminishing = p.continuations >= 3 && p.this_delta < 500 && p.last_delta < 500;
        if diminishing || p.accumulated >= met {
            return OutputTarget::Reached;
        }
        let remaining = p.target.saturating_sub(p.accumulated);
        OutputTarget::KeepGoing {
            nudge: format!(
                "\
<system-reminder>
Continue working. Used {}/{} output tokens ({} remaining).
</system-reminder>",
                p.accumulated, p.target, remaining
            ),
        }
    }

    fn context_budget(&self, scene: &TokenBudget) -> ContextBudget {
        ContextBudget {
            compact_threshold: self.compact_threshold.unwrap_or(scene.compact_threshold),
            compact_keep_recent: self
                .compact_keep_recent
                .unwrap_or(scene.compact_keep_recent),
            hard_cap: None,
        }
    }
}

/// A ceiling on request size, on top of whatever else a policy decides.
///
/// The second implementation, and the one the missing capability calls for: a
/// deployment that has to guarantee it never sends a request past some size —
/// a provider hard limit, a cost ceiling, a compliance rule — wraps its policy
/// in this. A turn whose context is still above the cap after compaction ends
/// rather than sending the request anyway.
pub struct Capped<P> {
    pub inner: P,
    pub context_hard_cap: usize,
}

impl<P: BudgetPolicy> BudgetPolicy for Capped<P> {
    fn on_usage(&self, spend: &Spend) -> Spending {
        self.inner.on_usage(spend)
    }

    fn remaining(&self, spend: &Spend) -> Option<Remaining> {
        self.inner.remaining(spend)
    }

    fn on_output_target(&self, progress: &OutputProgress) -> OutputTarget {
        self.inner.on_output_target(progress)
    }

    fn context_budget(&self, scene: &TokenBudget) -> ContextBudget {
        ContextBudget {
            hard_cap: Some(self.context_hard_cap),
            ..self.inner.context_budget(scene)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> EngineBudget {
        EngineBudget::default()
    }

    fn call(input: u32, output: u32) -> crate::interface::model::Usage {
        crate::interface::model::Usage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        }
    }

    fn turn_capped(limit: u64) -> SpendNode {
        SpendNode::turn(Some(limit), None)
    }

    #[test]
    fn the_bill_warns_at_ninety_percent_and_stops_at_the_cap() {
        let turn = turn_capped(1_000);
        turn.add(call(800, 99));
        assert_eq!(turn.judge(), (Scope::Turn, Spending::WithinBudget));
        turn.add(call(0, 1));
        assert!(matches!(turn.judge(), (Scope::Turn, Spending::Warn { .. })));
        turn.add(call(0, 100));
        assert_eq!(
            turn.judge(),
            (Scope::Turn, Spending::Exhausted { limit: 1_000 }),
            "the check is `>=`; at the cap the turn is over, not one call later"
        );
    }

    #[test]
    fn what_remains_is_the_tightest_ceiling_up_the_chain() {
        let session = Arc::new(SpendNode::session(Some(10_000)));
        session.restore(Spend {
            input_tokens: 7_000,
            ..Default::default()
        });
        let goal = Arc::new(SpendNode::goal(Some(5_000), session.clone()));
        let turn = Arc::new(SpendNode::turn(Some(4_000), Some(goal.clone())));
        let delegate = SpendNode::turn(None, Some(turn.clone()));
        delegate.add(call(1_000, 500));
        assert_eq!(
            delegate.remaining(),
            Some((
                Scope::Session,
                Remaining {
                    tokens: 1_500,
                    limit: 10_000
                }
            ))
        );
        assert_eq!(goal.read().counted_tokens(), 1_500);
        assert_eq!(turn.remaining().unwrap().1.tokens, 1_500);
        session.set_limit(None);
        assert_eq!(
            delegate.remaining(),
            Some((
                Scope::Turn,
                Remaining {
                    tokens: 2_500,
                    limit: 4_000
                }
            ))
        );
        assert_eq!(SpendNode::default().remaining(), None);
    }

    #[test]
    fn a_ceiling_reached_anywhere_up_the_chain_ends_the_turn() {
        let session = Arc::new(SpendNode::session(Some(1_000)));
        let turn = SpendNode::turn(Some(1_000_000), Some(session.clone()));
        turn.add(call(1_000, 0));
        assert_eq!(
            turn.judge(),
            (Scope::Session, Spending::Exhausted { limit: 1_000 })
        );
        let next = SpendNode::turn(None, Some(session));
        assert_eq!(next.remaining().unwrap().1.tokens, 0);
    }

    #[test]
    fn delegates_spending_at_once_lose_nothing() {
        let shared = Arc::new(turn_capped(40_000));
        let usage = crate::interface::model::Usage {
            input_tokens: 3,
            output_tokens: 2,
            cache_creation_input_tokens: 1,
            cache_read_input_tokens: 7,
        };
        let spenders: Vec<_> = (0..8)
            .map(|_| {
                let shared = shared.clone();
                std::thread::spawn(move || {
                    for _ in 0..1_000 {
                        shared.add(usage);
                    }
                })
            })
            .collect();
        for spender in spenders {
            spender.join().unwrap();
        }
        assert_eq!(
            shared.read(),
            Spend {
                input_tokens: 24_000,
                output_tokens: 16_000,
                cache_creation_tokens: 8_000,
                cache_read_tokens: 56_000,
            }
        );
        assert_eq!(shared.judge().1, Spending::Exhausted { limit: 40_000 });
    }

    #[test]
    fn a_delegates_call_is_spent_by_every_node_above_it_once() {
        let session = Arc::new(SpendNode::session(None));
        let root = Arc::new(SpendNode::turn(None, Some(session.clone())));
        let child_session = Arc::new(SpendNode::session(None));
        let child = Arc::new(
            SpendNode::turn(None, Some(root.clone())).counting_into(child_session.clone()),
        );
        let grandchild = SpendNode::turn(None, Some(child.clone()));
        root.add(call(1, 0));
        child.add(call(10, 0));
        grandchild.add(call(100, 0));
        assert_eq!(root.read().input_tokens, 111);
        assert_eq!(root.delegated().input_tokens, 110);
        assert_eq!(child.read().input_tokens, 110);
        assert_eq!(child.delegated().input_tokens, 100);
        assert_eq!(grandchild.whole_turn().input_tokens, 111);
        assert_eq!(session.read().input_tokens, 111);
        assert_eq!(session.delegated().input_tokens, 0);
        assert_eq!(child_session.read().input_tokens, 10);
        assert_eq!(
            grandchild.find(Scope::Session).unwrap().read().input_tokens,
            111
        );
    }

    #[test]
    fn no_ceiling_configured_means_none() {
        let turn = SpendNode::default();
        turn.add(call(u32::MAX, 0));
        assert_eq!(turn.judge().1, Spending::WithinBudget);
        assert_eq!(turn.remaining(), None);
    }

    /// The reading every ceiling and quota counts, and what it leaves out.
    #[test]
    fn a_cache_read_is_not_counted_and_a_cache_write_is() {
        let spend = Spend {
            input_tokens: 400,
            output_tokens: 600,
            cache_creation_tokens: 2_000,
            cache_read_tokens: 30_000,
        };
        assert_eq!(spend.counted_tokens(), 3_000);
        assert_eq!(spend.all_tokens(), 33_000);
    }

    /// A ceiling an operator set does not move with the cache hit rate, and a
    /// policy that wants the cache counted has the parts.
    #[test]
    fn the_engines_ceiling_does_not_move_when_the_cache_fills() {
        let turn = turn_capped(1_000);
        turn.add(crate::interface::model::Usage {
            input_tokens: 400,
            output_tokens: 300,
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 90_000,
        });
        assert!(
            matches!(turn.judge().1, Spending::WithinBudget),
            "the cache moved a 1000-token ceiling that nobody asked to move"
        );

        struct CountsTheCache;
        impl BudgetPolicy for CountsTheCache {
            fn on_usage(&self, spend: &Spend) -> Spending {
                if spend.all_tokens() >= 1_000 {
                    Spending::Exhausted { limit: 1_000 }
                } else {
                    Spending::WithinBudget
                }
            }
        }
        assert_eq!(
            CountsTheCache.on_usage(&turn.whole_turn()),
            Spending::Exhausted { limit: 1_000 },
            "the parts a policy needs to reach the other answer are not reachable"
        );
    }

    #[test]
    fn a_spend_is_written_with_the_wire_names_of_a_usage() {
        let spend = Spend::from(crate::interface::model::Usage {
            input_tokens: 11,
            output_tokens: 7,
            cache_creation_input_tokens: 5,
            cache_read_input_tokens: 23,
        });
        assert_eq!(
            serde_json::to_value(spend).unwrap(),
            serde_json::to_value(spend.as_usage()).unwrap()
        );
    }

    fn progress(accumulated: u64, continuations: u32, this: u64, last: u64) -> OutputProgress {
        OutputProgress {
            accumulated,
            target: 100_000,
            continuations,
            this_delta: this,
            last_delta: last,
        }
    }

    #[test]
    fn ninety_percent_of_the_output_target_counts_as_reached() {
        let p = engine();
        assert!(matches!(
            p.on_output_target(&progress(89_000, 0, 1_000, 0)),
            OutputTarget::KeepGoing { .. }
        ));
        assert_eq!(
            p.on_output_target(&progress(90_000, 0, 1_000, 0)),
            OutputTarget::Reached
        );
    }

    /// The rule that stops the engine nagging a model that has nothing left
    /// to add: three continuations *and* two consecutive small answers.
    #[test]
    fn diminishing_returns_take_all_three_conditions() {
        let p = engine();
        assert_eq!(
            p.on_output_target(&progress(10_000, 3, 400, 400)),
            OutputTarget::Reached
        );
        assert!(matches!(
            p.on_output_target(&progress(10_000, 2, 400, 400)),
            OutputTarget::KeepGoing { .. },
        ),);
        assert!(matches!(
            p.on_output_target(&progress(10_000, 3, 600, 400)),
            OutputTarget::KeepGoing { .. }
        ));
    }

    /// There is no cap on how many times the model can be asked to continue.
    /// A count alone never ends it — only the count together with two small
    /// answers does.
    #[test]
    fn a_long_run_of_substantial_answers_keeps_going() {
        assert!(matches!(
            engine().on_output_target(&progress(10_000, 20, 1_000, 1_000)),
            OutputTarget::KeepGoing { .. }
        ));
    }

    #[test]
    fn the_scene_decides_the_threshold_until_a_deployment_caps_it() {
        let scene = TokenBudget {
            compact_threshold: 150_000,
            compact_keep_recent: 4,
        };
        let engine_only = EngineBudget::default().context_budget(&scene);
        assert_eq!(engine_only.compact_threshold, 150_000);
        assert_eq!(
            engine_only.hard_cap, None,
            "the shape of the gap this fills"
        );

        let capped = Capped {
            inner: EngineBudget::default(),
            context_hard_cap: 120_000,
        }
        .context_budget(&scene);
        assert_eq!(capped.hard_cap, Some(120_000));
        assert_eq!(
            capped.compact_threshold, 150_000,
            "a ceiling is not a trigger — the scene still says when to compact"
        );
    }

    #[test]
    fn the_settings_compaction_numbers_override_the_scenes_when_set() {
        let scene = TokenBudget {
            compact_threshold: 150_000,
            compact_keep_recent: 20,
        };
        let configured = EngineBudget {
            compact_threshold: Some(60_000),
            compact_keep_recent: Some(4),
        }
        .context_budget(&scene);
        assert_eq!(
            (configured.compact_threshold, configured.compact_keep_recent),
            (60_000, 4)
        );

        let half = EngineBudget {
            compact_keep_recent: Some(4),
            ..EngineBudget::default()
        }
        .context_budget(&scene);
        assert_eq!(
            (half.compact_threshold, half.compact_keep_recent),
            (150_000, 4)
        );
    }
}
