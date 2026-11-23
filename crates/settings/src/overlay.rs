//! A layer laid over settings already loaded, in memory: what a host changed
//! in the settings it was given ([`Subject::Host`]), or a session's own
//! settings — `options.settings` of `session.create`, shaped like a settings
//! file — for that session, its sub-agents and its team members
//! ([`Subject::Session`]). [`LoadedSettings::overlay`] is the one place either
//! is merged: its rule lists join the rules, the managed layer is put back on
//! top, the providers are settled, and a value that does not fit is reported
//! in `ignored`.
//!
//! A session's layer holds only keys decided per session ([`crate::scope`]);
//! [`session_layer_problems`] names the rest. By default it may only tighten:
//! a value the project tier's table ([`crate::trust::PROJECT_TIER`]) orders as
//! looser than the settings' is held to the settings', and its allow rules are
//! held back — unless the settings set `permissions.client_may_loosen`. That
//! is a safe default, not a boundary: whoever reaches the socket may change
//! the settings themselves.

use base::interface::config_source::Tier;
use base::permission::{LoadedRule, PermissionMode, RuleBehavior};
use serde_json::{Map, Value};

use crate::loaded::seed_section_defaults;
use crate::merge::merge_fitting;
use crate::permissions::{self, read_rule_list, take_rule_lists, RULE_LISTS};
use crate::scope::{keys_outside, Scope};
use crate::section::Sections;
use crate::{renamed, trust, IgnoredValue, LoadedSettings, SchemaKeys, Settings};

/// Whose layer [`LoadedSettings::overlay`] lays over the settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Subject {
    /// What a host set in memory: taken as it is.
    Host,
    /// A session's own settings: its session-scoped keys, tightening only
    /// unless the settings let a client loosen them.
    Session,
}

impl Subject {
    fn name(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Session => Tier::Session.name(),
        }
    }
}

/// A key of a session's layer the settings held to their own value, because
/// the one asked for would have loosened them.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldKey {
    /// Dotted: `turn.token_budget`, `permissions.allow`.
    pub key: String,
    pub requested: Value,
    /// What is in force instead.
    pub applied: Value,
}

/// What in `layer` no session may be given, whatever the settings say, as
/// dotted keys (empty for the layer itself) with why: a key that is not a
/// setting or was renamed, one decided for more than a session, a value that
/// does not fit its setting, a program-only permission mode, a rule string
/// that does not read.
pub fn session_layer_problems(layer: &Value, sections: &Sections) -> Vec<(String, String)> {
    let Some(fields) = layer.as_object() else {
        return vec![(String::new(), "must be an object".into())];
    };
    let mut problems = Problems::default();
    for (old, new) in renamed::old_keys_in(layer) {
        problems.add(old.to_string(), format!("renamed to `{new}`"));
    }
    for key in SchemaKeys::of(sections).unknown_in(&[], layer) {
        if key != "$schema" {
            problems.add(key, "not a settings key".into());
        }
    }
    for (key, scope) in keys_outside(layer, Scope::Session, sections) {
        problems.add(
            key,
            format!(
                "{}: set it in the settings, not in a session's options",
                scope.decided()
            ),
        );
    }
    let mut defaults = serde_json::to_value(Settings::defaults_for("")).unwrap_or_default();
    seed_section_defaults(&mut defaults, sections);
    for (key, error) in merge_fitting(&mut defaults, layer.clone(), sections) {
        problems.add(key, error);
    }
    let mode = layer
        .pointer("/permissions/mode")
        .and_then(|v| serde_json::from_value::<PermissionMode>(v.clone()).ok());
    if mode.is_some_and(|mode| !mode.is_user_settable()) {
        problems.add(
            "permissions.mode".into(),
            "this mode is set by the engine, not by a host".into(),
        );
    }
    if let Some(block) = fields.get(permissions::KEY).and_then(Value::as_object) {
        for (list, _, _) in RULE_LISTS {
            if let Some(rules) = block.get(*list) {
                for (key, error) in read_rule_list(list, rules.clone()).1 {
                    problems.add(key, error);
                }
            }
        }
    }
    problems.0
}

#[derive(Default)]
struct Problems(Vec<(String, String)>);

impl Problems {
    /// `key`, unless it or a key it is in or holds is already reported.
    fn add(&mut self, key: String, error: String) {
        let nested = |a: &str, b: &str| a == b || a.starts_with(&format!("{b}.")) || b.is_empty();
        if !self
            .0
            .iter()
            .any(|(seen, _)| nested(&key, seen) || nested(seen, &key))
        {
            self.0.push((key, error));
        }
    }
}

impl LoadedSettings {
    /// These settings with `layer` laid over them, and the keys of a
    /// session's layer they held to their own value.
    ///
    /// A session's layer is judged against these settings as they are, so a
    /// caller that lays it over each time the session's engine is built
    /// follows the settings as they change. What [`session_layer_problems`]
    /// refuses is left out here with a warning: a caller that skipped that
    /// check gets no more than one that made it.
    pub fn overlay(
        &self,
        layer: &Map<String, Value>,
        subject: Subject,
        sections: &Sections,
    ) -> (Self, Vec<HeldKey>) {
        let loosen = subject == Subject::Host || self.values.permissions.client_may_loosen;
        let requested = Value::Object(layer.clone());
        let mut layer = layer.clone();
        let mut lower = serde_json::to_value(&self.values).unwrap_or_default();
        let mut held = Vec::new();
        if subject == Subject::Session {
            let outside = keys_outside(&requested, Scope::Session, sections);
            if !outside.is_empty() {
                tracing::warn!(keys = ?outside, "a session's own settings may not set these; ignoring them");
            }
            for (key, _) in outside {
                remove_dotted(&mut layer, &key);
            }
        }
        if !loosen {
            for key in trust::tighten_layer(&mut layer, &lower, sections) {
                held.push(HeldKey {
                    requested: value_at(&requested, &key),
                    applied: value_at(&lower, &key),
                    key,
                });
            }
        }
        let mut out = self.clone();
        out.take_rules(&mut layer, subject, loosen, &mut held);
        if let Some(providers) = layer.remove("providers") {
            lower["providers"] = crate::loaded::providers_first(&providers, &lower);
        }
        for (key, error) in merge_fitting(&mut lower, Value::Object(layer), sections) {
            out.ignore_overlaid(subject, key, error);
        }
        match serde_json::from_value::<Settings>(lower) {
            Ok(mut values) => {
                values.sections.retain(|key, _| sections.get(key).is_some());
                let before = std::mem::replace(&mut out.values, values);
                out.settle_providers();
                if let Err(e) = out.place_worktrees() {
                    out.ignore_overlaid(subject, "app.worktree_dir".into(), e.to_string());
                    out.values.app.worktree_dir = before.app.worktree_dir;
                }
            }
            Err(e) => {
                tracing::warn!(layer = subject.name(), error = %e, "the layer does not read; the settings are left as they were");
            }
        }
        let managed = out.managed.take();
        let out = out.with_managed(managed);
        if subject == Subject::Session {
            held.extend(held_by_managed(&out, &requested, &held));
        }
        (out, held)
    }

    /// The rule lists of `layer`'s `permissions`, taken out of it into these
    /// settings' rules; while a client may not loosen them, a list holding an
    /// allow rule into `held` instead.
    fn take_rules(
        &mut self,
        layer: &mut Map<String, Value>,
        subject: Subject,
        loosen: bool,
        held: &mut Vec<HeldKey>,
    ) {
        let Some(block) = layer
            .get_mut(permissions::KEY)
            .and_then(Value::as_object_mut)
        else {
            return;
        };
        let tier = match subject {
            Subject::Host => Tier::Cli,
            Subject::Session => Tier::Session,
        };
        for (list, value) in take_rule_lists(block) {
            let (rules, unread) = read_rule_list(list, value.clone());
            for (key, error) in unread {
                self.ignore_overlaid(subject, key, error);
            }
            if !loosen && rules.iter().any(|r| r.action == RuleBehavior::Allow) {
                held.push(HeldKey {
                    key: format!("{}.{list}", permissions::KEY),
                    requested: value,
                    applied: Value::Array(Vec::new()),
                });
                continue;
            }
            self.rules.extend(
                rules
                    .into_iter()
                    .map(|rule| LoadedRule::new(rule, tier, false)),
            );
        }
    }

    fn ignore_overlaid(&mut self, subject: Subject, key: String, error: String) {
        tracing::warn!(
            layer = subject.name(),
            key,
            error,
            "settings value does not fit its setting, ignoring it"
        );
        self.ignored.push(IgnoredValue {
            tier: subject.name(),
            origin: subject.name().to_string(),
            key,
            error,
        });
    }
}

/// The keys of a session's `requested` layer the managed layer set to
/// something else, beyond those already `held`.
fn held_by_managed(out: &LoadedSettings, requested: &Value, held: &[HeldKey]) -> Vec<HeldKey> {
    let Some(managed) = out.managed.as_ref().filter(|m| m.error.is_none()) else {
        return Vec::new();
    };
    let applied = serde_json::to_value(&out.values).unwrap_or_default();
    let mut keys = Vec::new();
    leaf_keys(
        &Value::Object(managed.values.clone()),
        &mut Vec::new(),
        &mut keys,
    );
    keys.into_iter()
        .filter(|key| !held.iter().any(|h| h.key == *key))
        .filter_map(|key| {
            let requested = value_at(requested, &key);
            let applied = value_at(&applied, &key);
            (!requested.is_null() && requested != applied).then_some(HeldKey {
                key,
                requested,
                applied,
            })
        })
        .collect()
}

fn leaf_keys(value: &Value, path: &mut Vec<String>, out: &mut Vec<String>) {
    match value.as_object() {
        Some(fields) if !fields.is_empty() => {
            for (key, inner) in fields {
                path.push(key.clone());
                leaf_keys(inner, path, out);
                path.pop();
            }
        }
        _ => out.push(path.join(".")),
    }
}

fn remove_dotted(layer: &mut Map<String, Value>, dotted: &str) {
    match dotted.split_once('.') {
        None => {
            layer.remove(dotted);
        }
        Some((head, rest)) => {
            if let Some(Value::Object(inner)) = layer.get_mut(head) {
                remove_dotted(inner, rest);
            }
        }
    }
}

fn value_at(root: &Value, dotted: &str) -> Value {
    dotted
        .split('.')
        .try_fold(root, |node, segment| node.get(segment))
        .cloned()
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn loaded(f: impl FnOnce(&mut Settings)) -> LoadedSettings {
        let mut values = Settings::defaults_for("m");
        f(&mut values);
        LoadedSettings::new(values, Default::default())
    }

    fn problems(layer: Value) -> Vec<String> {
        let mut keys: Vec<String> = session_layer_problems(&layer, &Sections::none())
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        keys.sort();
        keys
    }

    fn over(settings: &LoadedSettings, layer: Value) -> (LoadedSettings, Vec<HeldKey>) {
        settings.overlay(
            layer.as_object().unwrap(),
            Subject::Session,
            &Sections::none(),
        )
    }

    #[test]
    fn what_is_decided_for_more_than_a_session_is_refused() {
        assert_eq!(
            problems(json!({
                "providers": [{"id": "x"}],
                "hooks": {},
                "mcp": {"servers": {"gh": {"command": "gh"}}},
                "app": {"trust_all_projects": true},
                "model": {"order": ["x"], "effort": "low"},
                "prompt": {"override": "anything", "append": "fine"},
                "turn": {"token_budget": 10},
            })),
            ["app", "hooks", "mcp", "model.order", "providers"]
        );
    }

    #[test]
    fn a_key_that_is_no_setting_or_does_not_fit_is_refused() {
        assert_eq!(
            problems(json!({
                "permission_mode": "plan",
                "turnn": {},
                "turn": {"token_budget": "lots", "max_model_calls": 3},
                "permissions": {"mode": "bubble", "deny": ["Bash(rm", "Write"]},
            })),
            [
                "permission_mode",
                "permissions.deny[0]",
                "permissions.mode",
                "turn.token_budget",
                "turnn",
            ]
        );
        assert_eq!(problems(json!(3)), [""]);
        assert!(problems(json!({"sandbox": {"network": {"mode": "deny_all"}}})).is_empty());
    }

    #[test]
    fn a_looser_value_is_held_to_the_settings_and_a_tighter_one_kept() {
        let settings = loaded(|s| s.turn.token_budget = Some(1000));
        let (out, held) = over(
            &settings,
            json!({"turn": {"token_budget": 5000, "max_model_calls": 3}, "agents": {"max_depth": 0}}),
        );
        assert_eq!(
            held,
            [HeldKey {
                key: "turn.token_budget".into(),
                requested: json!(5000),
                applied: json!(1000),
            }]
        );
        assert_eq!(out.values.turn.token_budget, Some(1000));
        assert_eq!(out.values.turn.max_model_calls, 3);
        assert_eq!(out.values.agents.max_depth, 0);
        assert_eq!(out.values.model.model_name, "m");
    }

    #[test]
    fn allow_rules_wait_for_the_settings_to_let_a_client_loosen() {
        let layer = json!({"permissions": {"allow": ["Bash"], "deny": ["Write"], "ask": ["Read"]}});
        let (out, held) = over(&loaded(|_| {}), layer.clone());
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].key, "permissions.allow");
        let rules: Vec<_> = out
            .rules
            .iter()
            .map(|r| (r.rule.tool.as_str(), r.rule.action, r.tier))
            .collect();
        assert_eq!(
            rules,
            [
                ("Read", RuleBehavior::Ask, Tier::Session),
                ("Write", RuleBehavior::Deny, Tier::Session),
            ]
        );

        let trusting = loaded(|s| s.permissions.client_may_loosen = true);
        let (out, held) = over(&trusting, layer);
        assert!(held.is_empty(), "{held:?}");
        assert_eq!(out.rules.len(), 3);
    }

    #[test]
    fn under_client_may_loosen_a_key_may_loosen_but_not_be_one_decided_elsewhere() {
        let trusting = loaded(|s| {
            s.permissions.client_may_loosen = true;
            s.turn.token_budget = Some(1000);
        });
        let (out, held) = over(
            &trusting,
            json!({"turn": {"token_budget": 5000}, "hooks": {"Stop": []}}),
        );
        assert!(held.is_empty());
        assert_eq!(out.values.turn.token_budget, Some(5000));
        assert!(out.values.hooks.is_none());
    }

    #[test]
    fn the_managed_layer_stays_on_top_of_a_session_that_may_loosen() {
        let mut trusting = loaded(|s| s.permissions.client_may_loosen = true);
        trusting = trusting.with_managed(Some(base::permission::ManagedSettings {
            disable_bypass: true,
            ..Default::default()
        }));
        let (out, _) = over(
            &trusting,
            json!({"permissions": {"mode": "bypassPermissions"}}),
        );
        assert_eq!(out.values.permissions.mode, PermissionMode::Default);
        assert!(out.bypass_disabled());
    }

    #[test]
    fn a_managed_value_holds_over_a_session_that_may_loosen() {
        let trusting = loaded(|s| s.permissions.client_may_loosen = true).with_managed(Some(
            crate::managed::from_value(
                "it",
                json!({"turn": {"token_budget": 1000}, "sandbox": {"require": true}}),
                &Sections::none(),
            ),
        ));
        assert_eq!(trusting.values.turn.token_budget, Some(1000));
        let (out, held) = over(
            &trusting,
            json!({"turn": {"token_budget": 5000, "max_model_calls": 3}, "sandbox": {"require": false}}),
        );
        assert_eq!(out.values.turn.token_budget, Some(1000));
        assert!(out.values.sandbox.require);
        assert_eq!(out.values.turn.max_model_calls, 3);
        let mut keys: Vec<_> = held.iter().map(|h| h.key.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["sandbox.require", "turn.token_budget"]);
        assert_eq!(
            held.iter()
                .find(|h| h.key == "turn.token_budget")
                .unwrap()
                .applied,
            json!(1000)
        );
    }

    #[test]
    fn a_tighter_value_beneath_the_managed_layer_stands() {
        let tight = loaded(|s| s.turn.token_budget = Some(500)).with_managed(Some(
            crate::managed::from_value(
                "it",
                json!({"turn": {"token_budget": 1000}, "model": {"effort": "low"}}),
                &Sections::none(),
            ),
        ));
        assert_eq!(tight.values.turn.token_budget, Some(500));
        let (out, held) = over(&tight, json!({"turn": {"token_budget": 200}}));
        assert_eq!(out.values.turn.token_budget, Some(200));
        assert!(held.is_empty(), "{held:?}");

        let loose = loaded(|s| s.turn.token_budget = Some(5000)).with_managed(Some(
            crate::managed::from_value(
                "it",
                json!({"turn": {"token_budget": 1000}}),
                &Sections::none(),
            ),
        ));
        assert_eq!(loose.values.turn.token_budget, Some(1000));
    }

    #[test]
    fn a_host_layer_is_taken_as_it_is_and_its_providers_go_first() {
        let settings = loaded(|s| s.turn.token_budget = Some(1000));
        let (out, held) = settings.overlay(
            json!({"turn": {"token_budget": 5000}, "memory": {"enabled": false}})
                .as_object()
                .unwrap(),
            Subject::Host,
            &Sections::none(),
        );
        assert!(held.is_empty());
        assert_eq!(out.values.turn.token_budget, Some(5000));
        assert!(!out.values.memory.enabled);

        let (out, _) = settings.overlay(
            json!({"memory": {"enabled": 5}}).as_object().unwrap(),
            Subject::Host,
            &Sections::none(),
        );
        assert_eq!(out.ignored.len(), 1);
        assert_eq!(out.ignored[0].key, "memory.enabled");
        assert_eq!(out.ignored[0].tier, "host");
    }
}
