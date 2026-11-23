//! Session goals: an objective the engine keeps working on across turns, one
//! automatic round at a time, until the model marks it complete or blocked or
//! its rounds or its token budget run out.
//!
//! A goal's rounds spend into a node of its own under the session's
//! (`SessionLedger::goal_spent`); what that node has spent is passed in where
//! a judgement needs it, as `spent`.
//!
//! This module is the state machine and its authority rules. The tools that
//! drive it from the model live in `tools::goal`; whoever owns the turn loop
//! (`runtime::Agent::run`, or the daemon's session pool) asks
//! [`GoalSlot::next_round`] when a turn ends, and claims the round it returns
//! with [`GoalSlot::claim_round`] when that round's turn starts.

use serde::{Deserialize, Serialize};

/// Log namespace and event under which a goal snapshot is recorded.
pub const GOAL_NS: &str = "agix.goal";
pub const GOAL_EVENT: &str = "change";

pub const DEFAULT_MAX_ROUNDS: u32 = 256;
pub const MAX_ROUNDS_LIMIT: u32 = 10_000;
/// Automatic rounds that must have run since the goal was last armed before
/// one of them may report the goal blocked.
pub const BLOCKED_MIN_ROUNDS: u32 = 3;

const GOAL_TURN_PREFIX: &str = "goal-";

/// Goal snapshots to write to the session log, oldest first; `None` records
/// a cleared goal. See `SessionState::unrecorded_goal_changes` in `session-state`.
pub struct UnrecordedGoalChanges {
    pub changes: Vec<Option<Goal>>,
    _writing: tokio::sync::OwnedMutexGuard<()>,
}

impl UnrecordedGoalChanges {
    pub fn new(changes: Vec<Option<Goal>>, writing: tokio::sync::OwnedMutexGuard<()>) -> Self {
        Self {
            changes,
            _writing: writing,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GoalPhase {
    Active,
    Paused,
    Blocked,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Goal {
    pub id: String,
    pub revision: u64,
    pub objective: String,
    pub phase: GoalPhase,
    pub max_rounds: u32,
    /// The most tokens its rounds may spend, counted as every budget is,
    /// their delegates and side calls included. `None`: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    pub rounds_started: u32,
    /// Rounds started since the goal was last armed; what the blocked lower
    /// bound counts.
    #[serde(default)]
    pub rounds_since_armed: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
}

/// Who started the turn a goal operation happens in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TurnOrigin {
    /// A person, through a client or the host.
    #[default]
    Human,
    /// An automatic goal round.
    GoalRound { revision: u64, round: u32 },
    /// Anything else that starts turns on its own, such as a scheduled job.
    Automation,
}

impl TurnOrigin {
    /// Read from the turn id the turn was started under.
    pub fn from_turn_id(turn_id: &str) -> Self {
        if let Some(rest) = turn_id.strip_prefix(GOAL_TURN_PREFIX) {
            let mut parts = rest.splitn(3, '-');
            if let (Some(revision), Some(round)) = (parts.next(), parts.next()) {
                if let (Ok(revision), Ok(round)) = (revision.parse(), round.parse()) {
                    return Self::GoalRound { revision, round };
                }
            }
        }
        if turn_id.starts_with("schedule-") || turn_id.starts_with("cron-") {
            return Self::Automation;
        }
        Self::Human
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GoalAction {
    Edit {
        objective: Option<String>,
        max_rounds: Option<u32>,
        token_budget: Option<u64>,
    },
    Pause,
    Resume,
    Complete,
    Blocked {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GoalError {
    #[error("this session has no goal")]
    NoGoal,
    #[error("the session already has an unfinished goal ({0}); complete or clear it first")]
    AlreadyActive(String),
    #[error("goal id {given} does not match the current goal {current}")]
    WrongGoal { given: String, current: String },
    #[error("revision {given} is stale; the goal is at revision {current} — read it again")]
    StaleRevision { given: u64, current: u64 },
    #[error("{0}")]
    NotAllowed(String),
    #[error("cannot {action} a goal that is {phase:?}")]
    BadTransition {
        action: &'static str,
        phase: GoalPhase,
    },
    #[error("{0}")]
    Invalid(String),
}

/// A session's goal together with whether automatic rounds are armed.
/// Arming is never persisted: a goal read back from the log is disarmed until
/// a person resumes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalSlot {
    pub goal: Option<Goal>,
    pub armed: bool,
    /// The turn id of the round handed out and not yet started. At most one
    /// round is outstanding, so turns that end while it waits start no other.
    pub pending_round: Option<String>,
    /// The least revision the next goal may take, so a goal created after a
    /// clear never repeats a revision an earlier round still carries.
    pub next_revision: u64,
}

/// A goal as hosts and the model see it: the goal, whether automatic rounds
/// are armed, and what its rounds have spent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct GoalView {
    #[serde(flatten)]
    pub goal: Goal,
    pub armed: bool,
    pub tokens_used: u64,
}

/// How large a goal may be made, and by whom: `max_rounds` above
/// `max_rounds_limit` is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalLimits {
    pub max_rounds: Option<u32>,
    pub token_budget: Option<u64>,
    pub max_rounds_limit: u32,
}

impl GoalLimits {
    /// What a host asks for, held to the format's limit.
    pub fn asked(max_rounds: Option<u32>, token_budget: Option<u64>) -> Self {
        Self {
            max_rounds,
            token_budget,
            max_rounds_limit: MAX_ROUNDS_LIMIT,
        }
    }
}

/// The next automatic round to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalRound {
    pub turn_id: String,
    pub message: String,
}

impl GoalSlot {
    pub fn view(&self, spent: u64) -> Option<GoalView> {
        self.goal.clone().map(|goal| GoalView {
            goal,
            armed: self.armed,
            tokens_used: spent,
        })
    }

    /// The goal as read back from a log: disarmed.
    pub fn restored(goal: Option<Goal>) -> Self {
        Self {
            goal,
            ..Self::default()
        }
    }

    pub fn create(
        &mut self,
        objective: String,
        limits: GoalLimits,
        origin: TurnOrigin,
    ) -> Result<Goal, GoalError> {
        require_human(origin, "create a goal")?;
        let objective = non_empty(objective)?;
        let max_rounds = checked_max_rounds(
            limits
                .max_rounds
                .unwrap_or(DEFAULT_MAX_ROUNDS.min(limits.max_rounds_limit)),
            limits.max_rounds_limit,
        )?;
        let token_budget = checked_token_budget(limits.token_budget)?;
        if let Some(goal) = &self.goal {
            if goal.phase != GoalPhase::Complete {
                return Err(GoalError::AlreadyActive(goal.id.clone()));
            }
        }
        let revision = self
            .goal
            .as_ref()
            .map_or(1, |g| g.revision + 1)
            .max(self.next_revision);
        let goal = Goal {
            id: crate::id::Id::new().to_string(),
            revision,
            objective,
            phase: GoalPhase::Active,
            max_rounds,
            token_budget,
            rounds_started: 0,
            rounds_since_armed: 0,
            blocked_reason: None,
        };
        self.goal = Some(goal.clone());
        self.armed = true;
        self.pending_round = None;
        Ok(goal)
    }

    /// `spent` is what the goal's rounds have spent; `max_rounds_limit` the
    /// most rounds an edit may give it.
    pub fn update(
        &mut self,
        goal_id: &str,
        revision: u64,
        action: GoalAction,
        origin: TurnOrigin,
        spent: u64,
        max_rounds_limit: u32,
    ) -> Result<Goal, GoalError> {
        let goal = self.goal.as_mut().ok_or(GoalError::NoGoal)?;
        if goal.id != goal_id {
            return Err(GoalError::WrongGoal {
                given: goal_id.to_string(),
                current: goal.id.clone(),
            });
        }
        if goal.revision != revision {
            return Err(GoalError::StaleRevision {
                given: revision,
                current: goal.revision,
            });
        }
        match action {
            GoalAction::Edit {
                objective,
                max_rounds,
                token_budget,
            } => {
                let max_rounds = max_rounds
                    .map(|n| checked_max_rounds(n, max_rounds_limit))
                    .transpose()?;
                let token_budget = checked_token_budget(token_budget)?;
                edit_goal(goal, origin, objective, max_rounds, token_budget)?
            }
            GoalAction::Pause => pause_goal(goal, &mut self.armed, origin)?,
            GoalAction::Resume => resume_goal(goal, &mut self.armed, origin, spent)?,
            GoalAction::Complete => complete_goal(goal, &mut self.armed, origin)?,
            GoalAction::Blocked { reason } => block_goal(goal, &mut self.armed, origin, reason)?,
        }
        goal.revision += 1;
        self.pending_round = None;
        Ok(goal.clone())
    }

    pub fn clear(&mut self, origin: TurnOrigin) -> Result<(), GoalError> {
        require_human(origin, "clear a goal")?;
        let Some(goal) = self.goal.take() else {
            return Err(GoalError::NoGoal);
        };
        self.next_revision = self.next_revision.max(goal.revision + 1);
        self.armed = false;
        self.pending_round = None;
        Ok(())
    }

    /// Stop automatic rounds without changing the goal — a round that ended
    /// abnormally (cancelled, errored, out of budget) leaves it for a person
    /// to resume.
    pub fn disarm(&mut self) -> bool {
        self.pending_round = None;
        std::mem::replace(&mut self.armed, false)
    }

    /// Mark an active goal blocked whose budget its rounds have spent, `next`
    /// input included, as one whose rounds are all used is. Whether it changed.
    pub fn block_if_spent(&mut self, spent: u64, next: u64) -> bool {
        let Some(goal) = self.goal.as_mut() else {
            return false;
        };
        let Some(budget) = goal.token_budget.filter(|&b| spent_out(b, spent + next)) else {
            return false;
        };
        if goal.phase != GoalPhase::Active {
            return false;
        }
        goal.phase = GoalPhase::Blocked;
        goal.blocked_reason = Some(format!("token budget: {spent} of {budget} tokens used"));
        goal.revision += 1;
        self.armed = false;
        self.pending_round = None;
        true
    }

    /// The next automatic round, or `None` when the goal is not active and
    /// armed or a round is already outstanding. A goal whose rounds or
    /// token budget (against `spent`) are used up is marked blocked instead;
    /// the second value says whether the goal changed. The round counts as
    /// started only once it is claimed.
    pub fn next_round(&mut self, spent: u64) -> (Option<GoalRound>, bool) {
        let Some(goal) = self.goal.as_mut() else {
            return (None, false);
        };
        if !self.armed || goal.phase != GoalPhase::Active || self.pending_round.is_some() {
            return (None, false);
        }
        if goal.rounds_started >= goal.max_rounds {
            goal.phase = GoalPhase::Blocked;
            goal.blocked_reason = Some(format!("round limit: all {} rounds used", goal.max_rounds));
            goal.revision += 1;
            self.armed = false;
            return (None, true);
        }
        if self.block_if_spent(spent, 0) {
            return (None, true);
        }
        let Some(goal) = self.goal.as_mut() else {
            return (None, false);
        };
        let number = goal.rounds_started + 1;
        let round = GoalRound {
            turn_id: format!(
                "{GOAL_TURN_PREFIX}{}-{number}-{}",
                goal.revision,
                crate::id::Id::new()
            ),
            message: round_message(goal, number),
        };
        self.pending_round = Some(round.turn_id.clone());
        (Some(round), false)
    }

    /// Start the outstanding round `turn_id`, counting it. `false` when it is
    /// no longer wanted — the goal was paused, cleared, changed or disarmed
    /// since it was handed out — and the turn must not run.
    pub fn claim_round(&mut self, turn_id: &str) -> bool {
        if self.pending_round.as_deref() != Some(turn_id) {
            return false;
        }
        self.pending_round = None;
        let Some(goal) = self.goal.as_mut() else {
            return false;
        };
        if !self.armed || goal.phase != GoalPhase::Active {
            return false;
        }
        goal.rounds_started += 1;
        goal.rounds_since_armed += 1;
        true
    }

    /// Give up the outstanding round `turn_id` without running it, so the
    /// next turn to end may hand out another.
    pub fn abandon_round(&mut self, turn_id: &str) {
        if self.pending_round.as_deref() == Some(turn_id) {
            self.pending_round = None;
        }
    }
}

fn round_message(goal: &Goal, round: u32) -> String {
    let budget = goal
        .token_budget
        .map(|b| format!("Token budget: {b}\n"))
        .unwrap_or_default();
    format!(
        "<goal_round>\nObjective: {}\nRound: {}/{}\n{budget}\nContinue working toward the objective in this \
         session. Treat the workspace, tool results and session state as authoritative. Before \
         claiming completion, gather evidence that the whole objective is met, then mark it \
         complete with GoalUpdate (goal_id {}, read the current revision with GoalGet). If work \
         remains, leave the goal active for the next round. Mark it blocked only when the same \
         blocker has held for {BLOCKED_MIN_ROUNDS} consecutive rounds — difficulty or remaining \
         work is not a blocker.\n</goal_round>",
        goal.objective, round, goal.max_rounds, goal.id
    )
}

fn require_human(origin: TurnOrigin, what: &str) -> Result<(), GoalError> {
    match origin {
        TurnOrigin::Human => Ok(()),
        _ => Err(GoalError::NotAllowed(format!(
            "only a direct request from the user can {what}"
        ))),
    }
}

fn require_human_or_this_round(
    origin: TurnOrigin,
    revision: u64,
    what: &str,
) -> Result<(), GoalError> {
    match origin {
        TurnOrigin::Human => Ok(()),
        TurnOrigin::GoalRound { revision: r, .. } if r == revision => Ok(()),
        TurnOrigin::GoalRound { .. } => Err(GoalError::NotAllowed(format!(
            "this round belongs to an earlier revision of the goal; it cannot {what}"
        ))),
        TurnOrigin::Automation => Err(GoalError::NotAllowed(format!(
            "only the user or a round of this goal can {what}"
        ))),
    }
}

fn non_empty(s: String) -> Result<String, GoalError> {
    let s = s.trim().to_string();
    if s.is_empty() {
        Err(GoalError::Invalid("must not be empty".into()))
    } else {
        Ok(s)
    }
}

fn checked_max_rounds(n: u32, limit: u32) -> Result<u32, GoalError> {
    if (1..=limit).contains(&n) {
        Ok(n)
    } else {
        Err(GoalError::Invalid(format!(
            "max_rounds must be between 1 and {limit}"
        )))
    }
}

/// Whether `budget` leaves too little after `spent` for another model call.
fn spent_out(budget: u64, spent: u64) -> bool {
    budget.saturating_sub(spent) < crate::interface::budget_policy::MIN_CALL_TOKENS
}

fn checked_token_budget(budget: Option<u64>) -> Result<Option<u64>, GoalError> {
    match budget {
        Some(0) => Err(GoalError::Invalid("token_budget must be at least 1".into())),
        budget => Ok(budget),
    }
}

fn edit_goal(
    goal: &mut Goal,
    origin: TurnOrigin,
    objective: Option<String>,
    max_rounds: Option<u32>,
    token_budget: Option<u64>,
) -> Result<(), GoalError> {
    require_human(origin, "edit a goal")?;
    if goal.phase == GoalPhase::Complete {
        return Err(GoalError::BadTransition {
            action: "edit",
            phase: goal.phase,
        });
    }
    if let Some(objective) = objective {
        goal.objective = non_empty(objective)?;
    }
    if let Some(max) = max_rounds {
        goal.max_rounds = max;
    }
    if token_budget.is_some() {
        goal.token_budget = token_budget;
    }
    goal.rounds_since_armed = 0;
    Ok(())
}

fn pause_goal(goal: &mut Goal, armed: &mut bool, origin: TurnOrigin) -> Result<(), GoalError> {
    require_human(origin, "pause a goal")?;
    if goal.phase != GoalPhase::Active {
        return Err(GoalError::BadTransition {
            action: "pause",
            phase: goal.phase,
        });
    }
    goal.phase = GoalPhase::Paused;
    *armed = false;
    Ok(())
}

fn resume_goal(
    goal: &mut Goal,
    armed: &mut bool,
    origin: TurnOrigin,
    spent: u64,
) -> Result<(), GoalError> {
    require_human(origin, "resume a goal")?;
    let resumable = matches!(goal.phase, GoalPhase::Paused | GoalPhase::Blocked)
        || (goal.phase == GoalPhase::Active && !*armed);
    if !resumable {
        return Err(GoalError::BadTransition {
            action: "resume",
            phase: goal.phase,
        });
    }
    if goal.rounds_started >= goal.max_rounds {
        return Err(GoalError::NotAllowed(format!(
            "all {} rounds are used; edit the goal to raise max_rounds before resuming",
            goal.max_rounds
        )));
    }
    if let Some(budget) = goal.token_budget.filter(|&b| spent_out(b, spent)) {
        return Err(GoalError::NotAllowed(format!(
            "{spent} of its {budget} tokens are used; edit the goal to raise token_budget \
             before resuming"
        )));
    }
    goal.phase = GoalPhase::Active;
    goal.blocked_reason = None;
    goal.rounds_since_armed = 0;
    *armed = true;
    Ok(())
}

fn complete_goal(goal: &mut Goal, armed: &mut bool, origin: TurnOrigin) -> Result<(), GoalError> {
    require_human_or_this_round(origin, goal.revision, "complete a goal")?;
    if goal.phase == GoalPhase::Complete {
        return Err(GoalError::BadTransition {
            action: "complete",
            phase: goal.phase,
        });
    }
    goal.phase = GoalPhase::Complete;
    *armed = false;
    Ok(())
}

fn block_goal(
    goal: &mut Goal,
    armed: &mut bool,
    origin: TurnOrigin,
    reason: String,
) -> Result<(), GoalError> {
    require_human_or_this_round(origin, goal.revision, "mark a goal blocked")?;
    if goal.phase != GoalPhase::Active {
        return Err(GoalError::BadTransition {
            action: "block",
            phase: goal.phase,
        });
    }
    if matches!(origin, TurnOrigin::GoalRound { .. })
        && goal.rounds_since_armed < BLOCKED_MIN_ROUNDS
    {
        return Err(GoalError::NotAllowed(format!(
            "a goal can be marked blocked only after the same blocker has held for \
             {BLOCKED_MIN_ROUNDS} consecutive rounds (this is round {}); keep \
             working on it, or describe the blocker and let the next round retry",
            goal.rounds_since_armed
        )));
    }
    goal.phase = GoalPhase::Blocked;
    goal.blocked_reason = Some(non_empty(reason)?);
    *armed = false;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    trait Up {
        fn up(
            &mut self,
            id: &str,
            rev: u64,
            action: GoalAction,
            origin: TurnOrigin,
        ) -> Result<Goal, GoalError>;
    }

    impl Up for GoalSlot {
        fn up(
            &mut self,
            id: &str,
            rev: u64,
            action: GoalAction,
            origin: TurnOrigin,
        ) -> Result<Goal, GoalError> {
            self.update(id, rev, action, origin, 0, MAX_ROUNDS_LIMIT)
        }
    }

    fn active() -> GoalSlot {
        let mut slot = GoalSlot::default();
        slot.create(
            "ship it".into(),
            GoalLimits::asked(Some(5), None),
            TurnOrigin::Human,
        )
        .unwrap();
        slot
    }

    fn ids(slot: &GoalSlot) -> (String, u64) {
        let g = slot.goal.as_ref().unwrap();
        (g.id.clone(), g.revision)
    }

    fn start_round(slot: &mut GoalSlot) -> Option<GoalRound> {
        let round = slot.next_round(0).0?;
        assert!(slot.claim_round(&round.turn_id));
        Some(round)
    }

    fn round_origin(slot: &GoalSlot) -> TurnOrigin {
        TurnOrigin::GoalRound {
            revision: slot.goal.as_ref().unwrap().revision,
            round: 1,
        }
    }

    #[test]
    fn a_round_turn_id_reads_back_as_its_origin() {
        let mut slot = active();
        let (round, changed) = slot.next_round(0);
        let round = round.unwrap();
        assert!(!changed);
        assert_eq!(
            TurnOrigin::from_turn_id(&round.turn_id),
            TurnOrigin::GoalRound {
                revision: 1,
                round: 1
            }
        );
        assert_eq!(TurnOrigin::from_turn_id("t-123"), TurnOrigin::Human);
        assert_eq!(
            TurnOrigin::from_turn_id("schedule-x-y"),
            TurnOrigin::Automation
        );
        assert!(round.message.contains("Round: 1/5"));
    }

    #[test]
    fn only_a_person_creates_edits_pauses_resumes_and_clears() {
        let mut slot = GoalSlot::default();
        for origin in [
            TurnOrigin::Automation,
            TurnOrigin::GoalRound {
                revision: 1,
                round: 1,
            },
        ] {
            assert!(matches!(
                slot.create("x".into(), GoalLimits::asked(None, None), origin),
                Err(GoalError::NotAllowed(_))
            ));
        }
        let mut slot = active();
        let (id, rev) = ids(&slot);
        let round = round_origin(&slot);
        for action in [
            GoalAction::Pause,
            GoalAction::Resume,
            GoalAction::Edit {
                objective: Some("y".into()),
                max_rounds: None,
                token_budget: None,
            },
        ] {
            assert!(matches!(
                slot.up(&id, rev, action, round),
                Err(GoalError::NotAllowed(_))
            ));
        }
        assert!(matches!(slot.clear(round), Err(GoalError::NotAllowed(_))));
    }

    #[test]
    fn a_round_may_complete_its_own_revision() {
        let mut slot = active();
        slot.next_round(0);
        let (id, rev) = ids(&slot);
        let stale = TurnOrigin::GoalRound {
            revision: rev - 1,
            round: 1,
        };
        assert!(slot.up(&id, rev, GoalAction::Complete, stale).is_err());
        let goal = slot
            .up(&id, rev, GoalAction::Complete, round_origin(&slot))
            .unwrap();
        assert_eq!(goal.phase, GoalPhase::Complete);
        assert!(!slot.armed);
        assert_eq!(slot.next_round(0), (None, false));
    }

    #[test]
    fn a_round_may_block_only_after_the_lower_bound() {
        let mut slot = active();
        for round in 1..=BLOCKED_MIN_ROUNDS {
            start_round(&mut slot);
            let (id, rev) = ids(&slot);
            let result = slot.up(
                &id,
                rev,
                GoalAction::Blocked {
                    reason: "no access".into(),
                },
                round_origin(&slot),
            );
            if round < BLOCKED_MIN_ROUNDS {
                assert!(matches!(result, Err(GoalError::NotAllowed(_))), "{round}");
            } else {
                assert_eq!(result.unwrap().phase, GoalPhase::Blocked);
            }
        }
    }

    #[test]
    fn a_person_may_block_at_once() {
        let mut slot = active();
        let (id, rev) = ids(&slot);
        let goal = slot
            .up(
                &id,
                rev,
                GoalAction::Blocked {
                    reason: "waiting on legal".into(),
                },
                TurnOrigin::Human,
            )
            .unwrap();
        assert_eq!(goal.phase, GoalPhase::Blocked);
    }

    #[test]
    fn a_stale_revision_or_another_goal_id_is_refused() {
        let mut slot = active();
        let (id, rev) = ids(&slot);
        assert!(matches!(
            slot.up(&id, rev + 1, GoalAction::Pause, TurnOrigin::Human),
            Err(GoalError::StaleRevision { .. })
        ));
        assert!(matches!(
            slot.up("other", rev, GoalAction::Pause, TurnOrigin::Human),
            Err(GoalError::WrongGoal { .. })
        ));
    }

    #[test]
    fn rounds_run_out_into_blocked() {
        let mut slot = GoalSlot::default();
        slot.create(
            "x".into(),
            GoalLimits::asked(Some(2), None),
            TurnOrigin::Human,
        )
        .unwrap();
        assert!(start_round(&mut slot).is_some());
        assert!(start_round(&mut slot).is_some());
        let (round, changed) = slot.next_round(0);
        assert!(round.is_none() && changed);
        let goal = slot.goal.as_ref().unwrap();
        assert_eq!(goal.phase, GoalPhase::Blocked);
        assert!(goal
            .blocked_reason
            .as_deref()
            .unwrap()
            .contains("round limit"));
        let (id, rev) = ids(&slot);
        assert!(matches!(
            slot.up(&id, rev, GoalAction::Resume, TurnOrigin::Human),
            Err(GoalError::NotAllowed(_))
        ));
        slot.up(
            &id,
            rev,
            GoalAction::Edit {
                objective: None,
                max_rounds: Some(4),
                token_budget: None,
            },
            TurnOrigin::Human,
        )
        .unwrap();
        let (id, rev) = ids(&slot);
        slot.up(&id, rev, GoalAction::Resume, TurnOrigin::Human)
            .unwrap();
        assert!(slot.next_round(0).0.is_some());
    }

    #[test]
    fn a_spent_token_budget_blocks_the_goal_until_it_is_raised() {
        let mut slot = GoalSlot::default();
        slot.create(
            "x".into(),
            GoalLimits::asked(None, Some(5_000)),
            TurnOrigin::Human,
        )
        .unwrap();
        assert!(start_round(&mut slot).is_some());
        assert!(!slot.block_if_spent(3_976, 0), "1024 left pays for a call");
        assert!(!slot.block_if_spent(3_000, 976), "nor with the input");
        let (round, changed) = slot.next_round(4_000);
        assert!(round.is_none() && changed);
        let goal = slot.goal.as_ref().unwrap();
        assert_eq!(goal.phase, GoalPhase::Blocked);
        assert_eq!(
            goal.blocked_reason.as_deref(),
            Some("token budget: 4000 of 5000 tokens used")
        );
        let (id, rev) = ids(&slot);
        assert!(matches!(
            slot.update(&id, rev, GoalAction::Resume, TurnOrigin::Human, 4_000, 10),
            Err(GoalError::NotAllowed(_))
        ));
        let edit = GoalAction::Edit {
            objective: None,
            max_rounds: None,
            token_budget: Some(9_000),
        };
        slot.update(&id, rev, edit, TurnOrigin::Human, 4_000, 10)
            .unwrap();
        let (id, rev) = ids(&slot);
        slot.update(&id, rev, GoalAction::Resume, TurnOrigin::Human, 4_000, 10)
            .unwrap();
        assert!(slot.next_round(4_000).0.is_some());
    }

    #[test]
    fn a_goal_is_held_to_the_rounds_its_setter_may_give() {
        let mut slot = GoalSlot::default();
        let limits = GoalLimits {
            max_rounds: Some(50),
            token_budget: None,
            max_rounds_limit: 10,
        };
        assert!(matches!(
            slot.create("x".into(), limits, TurnOrigin::Human),
            Err(GoalError::Invalid(_))
        ));
        let limits = GoalLimits {
            max_rounds: None,
            ..limits
        };
        let goal = slot.create("x".into(), limits, TurnOrigin::Human).unwrap();
        assert_eq!(goal.max_rounds, 10, "the default is held to the limit too");
        assert!(matches!(
            slot.create(
                "y".into(),
                GoalLimits::asked(None, Some(0)),
                TurnOrigin::Human
            ),
            Err(GoalError::AlreadyActive(_) | GoalError::Invalid(_))
        ));
    }

    #[test]
    fn a_restored_goal_waits_for_a_person() {
        let mut slot = active();
        let mut restored = GoalSlot::restored(slot.goal.take());
        assert_eq!(restored.next_round(0), (None, false));
        let (id, rev) = ids(&restored);
        restored
            .up(&id, rev, GoalAction::Resume, TurnOrigin::Human)
            .unwrap();
        assert!(restored.next_round(0).0.is_some());
    }

    #[test]
    fn a_new_goal_waits_for_the_last_one_to_finish() {
        let mut slot = active();
        assert!(matches!(
            slot.create(
                "again".into(),
                GoalLimits::asked(None, None),
                TurnOrigin::Human
            ),
            Err(GoalError::AlreadyActive(_))
        ));
        let (id, rev) = ids(&slot);
        slot.up(&id, rev, GoalAction::Complete, TurnOrigin::Human)
            .unwrap();
        let next = slot
            .create(
                "again".into(),
                GoalLimits::asked(None, None),
                TurnOrigin::Human,
            )
            .unwrap();
        assert_eq!(next.max_rounds, DEFAULT_MAX_ROUNDS);
        assert!(next.revision > rev);
    }

    #[test]
    fn only_one_round_is_outstanding_at_a_time() {
        let mut slot = active();
        let first = slot.next_round(0).0.unwrap();
        assert_eq!(slot.next_round(0), (None, false));
        assert!(slot.claim_round(&first.turn_id));
        assert!(!slot.claim_round(&first.turn_id));
        let second = slot.next_round(0).0.unwrap();
        assert!(second.message.contains("Round: 2/5"));
        assert_ne!(first.turn_id, second.turn_id);
    }

    #[test]
    fn rounds_count_only_once_claimed() {
        let mut slot = active();
        let round = slot.next_round(0).0.unwrap();
        let goal = slot.goal.as_ref().unwrap();
        assert_eq!((goal.rounds_started, goal.rounds_since_armed), (0, 0));
        assert!(slot.claim_round(&round.turn_id));
        let goal = slot.goal.as_ref().unwrap();
        assert_eq!((goal.rounds_started, goal.rounds_since_armed), (1, 1));
    }

    #[test]
    fn a_round_handed_out_before_a_pause_clear_or_change_cannot_start() {
        let mut slot = active();
        let round = slot.next_round(0).0.unwrap();
        let (id, rev) = ids(&slot);
        slot.up(&id, rev, GoalAction::Pause, TurnOrigin::Human)
            .unwrap();
        assert!(!slot.claim_round(&round.turn_id));

        let mut slot = active();
        let round = slot.next_round(0).0.unwrap();
        slot.clear(TurnOrigin::Human).unwrap();
        assert!(!slot.claim_round(&round.turn_id));

        let mut slot = active();
        let round = slot.next_round(0).0.unwrap();
        let (id, rev) = ids(&slot);
        slot.up(
            &id,
            rev,
            GoalAction::Edit {
                objective: Some("ship it twice".into()),
                max_rounds: None,
                token_budget: None,
            },
            TurnOrigin::Human,
        )
        .unwrap();
        assert!(!slot.claim_round(&round.turn_id));
        let next = slot.next_round(0).0.unwrap();
        assert!(slot.claim_round(&next.turn_id));

        let mut slot = active();
        let round = slot.next_round(0).0.unwrap();
        slot.disarm();
        assert!(!slot.claim_round(&round.turn_id));
        assert_eq!(slot.goal.as_ref().unwrap().rounds_started, 0);
    }

    #[test]
    fn an_abandoned_round_lets_the_next_one_be_handed_out() {
        let mut slot = active();
        let round = slot.next_round(0).0.unwrap();
        slot.abandon_round("goal-1-1-someone-else");
        assert_eq!(slot.next_round(0), (None, false));
        slot.abandon_round(&round.turn_id);
        assert!(slot.next_round(0).0.is_some());
    }

    #[test]
    fn a_goal_created_after_a_clear_never_reuses_a_revision() {
        let mut slot = active();
        let stale = round_origin(&slot);
        slot.clear(TurnOrigin::Human).unwrap();
        slot.create(
            "another".into(),
            GoalLimits::asked(None, None),
            TurnOrigin::Human,
        )
        .unwrap();
        let (id, rev) = ids(&slot);
        assert!(matches!(
            slot.up(&id, rev, GoalAction::Complete, stale),
            Err(GoalError::NotAllowed(_))
        ));
    }
}
