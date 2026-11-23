//! The limits that hold across turns: what a session may spend over its
//! whole life (`session`), how far a goal the model sets may run (`goal`),
//! and what the model may schedule for itself (`schedule`).

use serde::{Deserialize, Serialize};

/// The limits of a session over all its turns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct SessionSettings {
    /// Maximum cumulative tokens a session may spend from when it was
    /// created, counted as `turn.token_budget` is (input not read from the
    /// cache, cache writes and output) over every turn — its delegates', the
    /// calls made on their side, scheduled and goal turns included — and
    /// across restarts. Each call is judged as under `turn.token_budget`,
    /// its input estimated and taken off what is left; with less than 1024
    /// remaining no call is sent, and the turn, and every turn after it,
    /// ends with `budget_exceeded`. `None` = unlimited.
    pub token_budget: Option<u64>,
}

/// What the model may give a goal it sets for itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct GoalSettings {
    /// The most automatic rounds a goal the model sets or edits
    /// (`GoalCreate`, `GoalUpdate`) may have; a larger `max_rounds` is
    /// lowered to it. A goal a host sets is not held to it.
    pub max_rounds: u32,
}

impl Default for GoalSettings {
    fn default() -> Self {
        Self {
            max_rounds: base::context::goal::MAX_ROUNDS_LIMIT,
        }
    }
}

/// What the model may schedule for itself with `CronCreate`. Jobs a host
/// creates (`schedule.create`) are not held to these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct ScheduleSettings {
    /// The most jobs the model's own `CronCreate` calls may hold in a session
    /// at once; one it deletes, or one that has fired its last, makes room.
    /// `0`: the model schedules none.
    pub max_model_jobs: u32,
    /// The fewest minutes between two fires of a recurring job the model
    /// schedules, over the seven days it lives. `1` adds nothing to the
    /// cron expression's own once-a-minute resolution.
    pub min_model_interval_minutes: u32,
}

impl Default for ScheduleSettings {
    fn default() -> Self {
        Self {
            max_model_jobs: 20,
            min_model_interval_minutes: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::section::Sections;
    use crate::trust::gate_layer;
    use serde_json::{json, Value};

    fn settings(json: Value) -> crate::Settings {
        let mut merged = serde_json::to_value(crate::Settings::defaults_for("m")).unwrap();
        crate::merge_json_values(&mut merged, json);
        serde_json::from_value(merged).unwrap()
    }

    /// What an untrusted project tier keeps of `layer`, over `lower`.
    fn kept_by_an_untrusted_project(layer: Value, lower: Value) -> (Value, Vec<String>) {
        let mut below = serde_json::to_value(crate::Settings::defaults_for("m")).unwrap();
        crate::merge_json_values(&mut below, lower);
        let mut layer = layer.as_object().unwrap().clone();
        let withheld = gate_layer(&mut layer, &below, true, &Sections::none());
        (Value::Object(layer), withheld)
    }

    #[test]
    fn the_session_block_reads_and_defaults_to_no_limit() {
        assert_eq!(
            crate::Settings::defaults_for("m").session.token_budget,
            None
        );
        let read = settings(json!({ "session": { "token_budget": 50_000 } }));
        assert_eq!(read.session.token_budget, Some(50_000));
    }

    #[test]
    fn the_goal_block_defaults_to_the_most_rounds_a_goal_can_have() {
        assert_eq!(crate::Settings::defaults_for("m").goal.max_rounds, 10_000);
        assert_eq!(
            settings(json!({ "goal": { "max_rounds": 8 } }))
                .goal
                .max_rounds,
            8
        );
        let (_, withheld) =
            kept_by_an_untrusted_project(json!({ "goal": { "max_rounds": 20_000 } }), json!({}));
        assert_eq!(withheld, ["goal.max_rounds"]);
    }

    #[test]
    fn the_schedule_block_defaults_leave_the_model_twenty_jobs_at_any_interval() {
        let defaults = crate::Settings::defaults_for("m").schedule;
        assert_eq!(
            (defaults.max_model_jobs, defaults.min_model_interval_minutes),
            (20, 1)
        );
        let read = settings(json!({
            "schedule": { "max_model_jobs": 1, "min_model_interval_minutes": 10 }
        }));
        assert_eq!(
            (
                read.schedule.max_model_jobs,
                read.schedule.min_model_interval_minutes
            ),
            (1, 10)
        );
    }

    #[test]
    fn an_untrusted_project_may_only_tighten_what_the_model_schedules() {
        let (kept, withheld) = kept_by_an_untrusted_project(
            json!({ "schedule": { "max_model_jobs": 2, "min_model_interval_minutes": 30 } }),
            json!({}),
        );
        assert!(withheld.is_empty(), "{withheld:?}");
        assert_eq!(kept["schedule"]["min_model_interval_minutes"], 30);

        let (_, withheld) = kept_by_an_untrusted_project(
            json!({ "schedule": { "max_model_jobs": 50, "min_model_interval_minutes": 5 } }),
            json!({ "schedule": { "min_model_interval_minutes": 10 } }),
        );
        assert_eq!(
            withheld,
            [
                "schedule.max_model_jobs",
                "schedule.min_model_interval_minutes"
            ]
        );
    }

    #[test]
    fn an_untrusted_project_may_only_lower_the_session_budget() {
        let (kept, withheld) = kept_by_an_untrusted_project(
            json!({ "session": { "token_budget": 1_000 } }),
            json!({}),
        );
        assert!(withheld.is_empty(), "{withheld:?}");
        assert_eq!(kept["session"]["token_budget"], 1_000);

        let (_, withheld) = kept_by_an_untrusted_project(
            json!({ "session": { "token_budget": 9_000 } }),
            json!({ "session": { "token_budget": 5_000 } }),
        );
        assert_eq!(withheld, ["session.token_budget"]);
        let (_, withheld) = kept_by_an_untrusted_project(
            json!({ "session": { "token_budget": null } }),
            json!({ "session": { "token_budget": 5_000 } }),
        );
        assert_eq!(withheld, ["session.token_budget"]);
    }
}
