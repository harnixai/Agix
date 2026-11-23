//! The deployment's settings layer — what an IT department or an unattended
//! host puts above everything a user, a project, a host or a session can
//! write.
//!
//! It is read from a fixed system path (`Layout::managed_settings_file`) that the
//! user's data directory, the project and the model have no say in; a daemon
//! started with `--managed-settings <path>` reads that file instead. It may
//! set any settings key, and each one it sets is what every session gets:
//! [`LoadedSettings::with_managed`](crate::LoadedSettings::with_managed) lays
//! it over every other layer, again after a host's or a session's. Its
//! `permissions` rule lists are the exception: they stand beside every other
//! tier's rules, and the permission gate holds its deny and ask rules even in
//! `bypassPermissions` (`permissions::policy::ManagedLayer`).
//!
//! A file that is there but cannot be honoured — unreadable, not JSON, a key
//! no setting reads, a value that does not fit its setting — is not skipped
//! the way a broken user tier is: it sets [`ManagedSettings::error`], none of
//! it is applied, and the permission gate refuses every tool call until it is
//! fixed. Skipping it, or the part that does not read, would silently drop
//! what the deployment wrote. Semantics: `docs/ARCHITECTURE.md` §6.1.

use base::permission::{ManagedSettings, PermissionMode};
use serde_json::{Map, Value};
use std::path::Path;

use crate::loaded::seed_section_defaults;
use crate::merge::merge_fitting;
use crate::permissions::{self, RULE_LISTS};
use crate::section::Sections;
use crate::{renamed, SchemaKeys, Settings};

/// The key of its `permissions` block that takes `bypassPermissions` away.
pub const DISABLE_BYPASS: &str = "disable_bypass";

/// The file at `path`. `None` when it does not exist and `required` is
/// false; a `required` file that does not exist is an error, so a path the
/// operator named cannot quietly mean "no managed settings".
pub fn read(path: &Path, required: bool, sections: &Sections) -> Option<ManagedSettings> {
    let origin = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => return None,
        Err(e) => return Some(broken(origin, format!("cannot be read: {e}"))),
    };
    Some(match serde_json::from_str(&text) {
        Ok(value) => from_value(origin, value, sections),
        Err(e) => broken(origin, format!("is not valid JSON: {e}")),
    })
}

pub fn from_value(origin: impl Into<String>, value: Value, sections: &Sections) -> ManagedSettings {
    let origin = origin.into();
    let Value::Object(mut fields) = value else {
        return broken(origin, "is not a JSON object".into());
    };
    fields.remove("$schema");
    if let Some(refusal) = unread_keys(&fields, sections) {
        return broken(origin, refusal);
    }
    let mut block = match fields.remove(permissions::KEY) {
        None => Map::new(),
        Some(Value::Object(block)) => block,
        Some(other) => {
            return broken(
                origin,
                format!("{}: must be an object, not {other}", permissions::KEY),
            )
        }
    };
    let prefix = format!("{}.", permissions::KEY);
    let mut rules = Vec::new();
    for (list, _, _) in RULE_LISTS {
        let Some(value) = block.remove(*list) else {
            continue;
        };
        let (read, unread) = permissions::read_rule_list(list, value);
        if let Some((at, error)) = unread.into_iter().next() {
            return broken(origin, format!("{at}: {error}"));
        }
        rules.extend(read);
    }
    let disable_bypass = match block.remove(DISABLE_BYPASS) {
        None => false,
        Some(Value::Bool(b)) => b,
        Some(other) => {
            return broken(
                origin,
                format!("{prefix}{DISABLE_BYPASS} must be true or false, not {other}"),
            )
        }
    };
    if !block.is_empty() {
        fields.insert(permissions::KEY.into(), Value::Object(block));
    }
    if let Some((key, error)) = unfitting_value(&fields, sections) {
        return broken(origin, format!("{key}: {error}"));
    }
    ManagedSettings {
        origin,
        error: None,
        rules,
        disable_bypass,
        values: fields,
    }
}

/// Why `fields` cannot stand, when it holds a key no setting reads: an old
/// one named with what replaced it.
fn unread_keys(fields: &Map<String, Value>, sections: &Sections) -> Option<String> {
    let mut layer = Value::Object(fields.clone());
    if let Some(block) = layer
        .get_mut(permissions::KEY)
        .and_then(Value::as_object_mut)
    {
        block.remove(DISABLE_BYPASS);
    }
    let old = renamed::old_keys_in(&layer);
    let nested = |a: &str, b: &str| {
        a == b || a.starts_with(&format!("{b}.")) || b.starts_with(&format!("{a}."))
    };
    let mut refused: Vec<String> = old
        .iter()
        .map(|(old, new)| format!("{old} (renamed to {new})"))
        .collect();
    for key in SchemaKeys::of(sections).unknown_in(&[], &layer) {
        if !old.iter().any(|(old, _)| nested(old, &key)) {
            refused.push(key);
        }
    }
    if refused.is_empty() {
        return None;
    }
    refused.sort_unstable();
    Some(format!(
        "sets keys no setting reads: {}",
        refused.join(", ")
    ))
}

/// The first value of `fields` that does not fit its setting, as a dotted
/// key with why.
fn unfitting_value(fields: &Map<String, Value>, sections: &Sections) -> Option<(String, String)> {
    let mut layer = fields.clone();
    if let Some(providers) = layer.remove(crate::loaded::PROVIDERS) {
        if let Some(problem) = crate::loaded::read_providers(providers)
            .1
            .into_iter()
            .next()
        {
            return Some(problem);
        }
    }
    let mode = layer
        .get(permissions::KEY)
        .and_then(|block| block.get("mode"))
        .and_then(|mode| serde_json::from_value::<PermissionMode>(mode.clone()).ok());
    if mode.is_some_and(|mode| !mode.is_user_settable()) {
        return Some((
            format!("{}.mode", permissions::KEY),
            "this mode is set by the engine, not by a settings file".into(),
        ));
    }
    let mut defaults = serde_json::to_value(Settings::defaults_for("")).unwrap_or_default();
    seed_section_defaults(&mut defaults, sections);
    merge_fitting(&mut defaults, Value::Object(layer), sections)
        .into_iter()
        .next()
}

/// Two managed layers as one: both sets of rules, either one's switch, the
/// later one's values over the earlier one's, and broken if either is.
pub fn followed_by(mut earlier: ManagedSettings, later: ManagedSettings) -> ManagedSettings {
    earlier.origin = format!("{}, {}", earlier.origin, later.origin);
    earlier.error = earlier.error.or(later.error);
    earlier.rules.extend(later.rules);
    earlier.disable_bypass |= later.disable_bypass;
    let mut values = Value::Object(std::mem::take(&mut earlier.values));
    crate::merge::merge_json_values(&mut values, Value::Object(later.values));
    if let Value::Object(values) = values {
        earlier.values = values;
    }
    earlier
}

/// What the global tier says about trust, with what a managed layer that can
/// be honoured says in its place.
pub fn trust_under(
    global: crate::trust::ProjectTrustSettings,
    managed: Option<&ManagedSettings>,
) -> crate::trust::ProjectTrustSettings {
    match managed
        .filter(|m| m.error.is_none())
        .and_then(|m| m.values.get("app"))
    {
        Some(app) => global.decided_by(app),
        None => global,
    }
}

/// The layer as a settings file would hold it: its values, with the rule
/// lists and `disable_bypass` back in its `permissions` block.
pub fn as_file(managed: &ManagedSettings) -> Map<String, Value> {
    let mut file = managed.values.clone();
    let mut block = match file.remove(permissions::KEY) {
        Some(Value::Object(block)) => block,
        _ => Map::new(),
    };
    block.extend(permissions::rule_lists(managed.rules.iter()));
    block.insert(DISABLE_BYPASS.into(), Value::Bool(managed.disable_bypass));
    file.insert(permissions::KEY.into(), Value::Object(block));
    file
}

/// The layer as `config.get` and `daemon.doctor` show it: where it came from,
/// why it cannot be honoured if it cannot, and what it sets in the file's own
/// shape.
pub fn report(managed: &ManagedSettings) -> Value {
    let mut report = Map::new();
    report.insert("origin".into(), Value::String(managed.origin.clone()));
    if let Some(error) = &managed.error {
        report.insert("error".into(), Value::String(error.clone()));
    }
    report.extend(as_file(managed));
    Value::Object(report)
}

fn broken(origin: String, error: String) -> ManagedSettings {
    tracing::error!(origin = %origin, error = %error, "managed settings cannot be honoured; every tool call is refused until they are fixed");
    ManagedSettings {
        error: Some(format!("{origin} {error}")),
        origin,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::permission::RuleBehavior;
    use serde_json::json;

    fn read_value(value: Value) -> ManagedSettings {
        from_value("it", value, &Sections::none())
    }

    #[test]
    fn rules_and_the_bypass_switch_are_read() {
        let m = read_value(json!({
            "$schema": "x",
            "permissions": {
                "deny": ["Bash(curl:*)"],
                "ask": ["Bash(git push:*)"],
                "allow_exact": ["Bash(ls)"],
                "disable_bypass": true,
            },
        }));
        assert_eq!(m.error, None);
        assert!(m.disable_bypass);
        assert!(m.values.is_empty(), "{:?}", m.values);
        let read: Vec<_> = m
            .rules
            .iter()
            .map(|r| (r.tool.as_str(), r.action))
            .collect();
        assert_eq!(
            read,
            [
                ("Bash(ls)", RuleBehavior::Allow),
                ("Bash(git push:*)", RuleBehavior::Ask),
                ("Bash(curl:*)", RuleBehavior::Deny),
            ]
        );
        assert!(!m.rules[0].matching.is_pattern());
    }

    #[test]
    fn every_other_setting_is_kept_as_the_file_has_it() {
        let m = read_value(json!({
            "app": {"trust_all_projects": false, "session_cap": 4},
            "sandbox": {"require": true},
            "permissions": {"mode": "plan", "deny": ["Bash"], "prompt_timeout_secs": 5},
            "providers": [{"id": "corp", "tiers": {"strong": "m"}}],
        }));
        assert_eq!(m.error, None);
        assert_eq!(
            Value::Object(m.values.clone()),
            json!({
                "app": {"trust_all_projects": false, "session_cap": 4},
                "sandbox": {"require": true},
                "permissions": {"mode": "plan", "prompt_timeout_secs": 5},
                "providers": [{"id": "corp", "tiers": {"strong": "m"}}],
            })
        );
        assert_eq!(m.rules.len(), 1);
        assert_eq!(
            Value::Object(as_file(&m))["permissions"],
            json!({"mode": "plan", "prompt_timeout_secs": 5, "deny": ["Bash"], "disable_bypass": false})
        );
    }

    #[test]
    fn two_layers_add_their_rules_and_the_later_ones_values_win() {
        let both = followed_by(
            read_value(
                json!({"turn": {"token_budget": 1, "max_model_calls": 2}, "permissions": {"deny": ["Bash"]}}),
            ),
            read_value(
                json!({"turn": {"token_budget": 3}, "permissions": {"ask": ["Read"], "disable_bypass": true}}),
            ),
        );
        assert_eq!(both.error, None);
        assert_eq!(both.rules.len(), 2);
        assert!(both.disable_bypass);
        assert_eq!(
            Value::Object(both.values),
            json!({"turn": {"token_budget": 3, "max_model_calls": 2}})
        );
    }

    #[test]
    fn a_bypass_session_can_be_asked_when_the_layer_asks_denies_or_takes_bypass_away() {
        let rule = |list: &str| json!({"permissions": { list: ["Bash"] }});
        let asks = |value| read_value(value).asks_under_bypass();
        assert!(asks(rule("ask")));
        assert!(asks(rule("deny")));
        assert!(asks(json!({"permissions": {"disable_bypass": true}})));
        assert!(!asks(rule("allow")));
        assert!(!asks(json!({})));
        assert!(!asks(json!({"permissions": {"mode": "default"}})));
    }

    #[test]
    fn a_key_no_setting_reads_breaks_the_whole_layer() {
        for (key, value) in [
            ("permision", json!({})),
            ("permission_rules", json!([])),
            ("paths", json!({})),
        ] {
            let m = read_value(json!({ key: value, "permissions": {"deny": ["Bash"]} }));
            let error = m.error.unwrap_or_else(|| panic!("{key} was accepted"));
            assert!(error.contains(key), "{error}");
            assert!(m.rules.is_empty());
        }
        let m = read_value(json!({"permissions": {"modes": "plan", "deny": ["Bash"]}}));
        let error = m.error.unwrap();
        assert!(error.contains("permissions.modes"), "{error}");
        assert!(m.rules.is_empty());
    }

    #[test]
    fn an_old_key_is_refused_with_its_new_name() {
        let m = read_value(json!({"disable_bypass_permissions_mode": true}));
        let error = m.error.unwrap();
        assert!(
            error.contains("renamed to permissions.disable_bypass"),
            "{error}"
        );
        assert!(!m.disable_bypass);
    }

    #[test]
    fn a_value_of_the_wrong_shape_breaks_the_layer() {
        for value in [
            json!({"permissions": {"disable_bypass": "yes"}}),
            json!({"permissions": {"deny": "Bash"}}),
            json!({"permissions": {"deny": ["Bash", 5]}}),
            json!({"permissions": {"deny": ["Bash(rm:*"]}}),
            json!({"permissions": {"mode": "bubble"}}),
            json!({"permissions": {"mode": "loose"}}),
            json!({"permissions": ["deny"]}),
            json!({"turn": {"token_budget": "lots"}}),
            json!({"providers": [{"id": "x", "base": "y"}]}),
            json!({"providers": {"x": {}}}),
            json!(["not", "an", "object"]),
        ] {
            let m = read_value(value.clone());
            assert!(m.error.is_some(), "{value}");
            assert!(m.values.is_empty() && m.rules.is_empty(), "{value}");
        }
    }

    #[test]
    fn an_absent_file_is_no_layer_unless_it_was_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("managed-settings.json");
        let none = Sections::none();
        assert!(read(&path, false, &none).is_none());
        assert!(read(&path, true, &none).unwrap().error.is_some());

        std::fs::write(&path, "{not json").unwrap();
        let broken = read(&path, false, &none).unwrap();
        assert!(broken.error.unwrap().contains("not valid JSON"));
    }
}
