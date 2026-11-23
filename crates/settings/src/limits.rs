//! The limits that hold across turns: what a session may spend over its
//! whole life (`session`).

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
    /// across restarts. Each call asks for no more output than is left; with
    /// less than 1024 left no call is sent, and the turn, and every turn
    /// after it, ends with `budget_exceeded`. `None` = unlimited.
    pub token_budget: Option<u64>,
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
