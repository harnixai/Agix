//! The deployment's settings layer — what an IT department or an unattended
//! host puts above everything a user, a project or a plugin can write.
//!
//! It is read from a fixed system path (`Layout::managed_settings_file`) that the
//! user's data directory, the project and the model have no say in; a daemon
//! started with `--managed-settings <path>` reads that file instead. Only
//! [`MANAGED_KEYS`] may appear in it, and in its `permissions` block only
//! [`MANAGED_PERMISSION_KEYS`]. A file that is there but cannot be
//! honoured — unreadable, not JSON, a key outside those lists, a value of the
//! wrong shape — is not skipped the way a broken user tier is: it sets
//! [`ManagedSettings::error`], and the permission gate then refuses every tool
//! call until it is fixed. Skipping it would silently drop every deny the
//! deployment wrote.
//!
//! What the layer means for a tool call is the permission gate's business
//! (`permissions::policy::ManagedLayer`); this module only says what the file
//! holds. Semantics: `docs/ARCHITECTURE.md` §6.1.

use base::permission::ManagedSettings;
use serde_json::{Map, Value};
use std::path::Path;

use crate::permissions::{self, RULE_LISTS};

/// The keys a managed settings file may set. `$schema` is the editor's and is
/// accepted too.
pub const MANAGED_KEYS: &[&str] = &[permissions::KEY];

/// The keys of its `permissions` block a managed settings file may set.
pub const MANAGED_PERMISSION_KEYS: &[&str] =
    &["allow", "allow_exact", "ask", "deny", "disable_bypass"];

/// The file at `path`. `None` when it does not exist and `required` is
/// false; a `required` file that does not exist is an error, so a path the
/// operator named cannot quietly mean "no managed settings".
pub fn read(path: &Path, required: bool) -> Option<ManagedSettings> {
    let origin = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => return None,
        Err(e) => return Some(broken(origin, format!("cannot be read: {e}"))),
    };
    Some(match serde_json::from_str(&text) {
        Ok(value) => from_value(origin, value),
        Err(e) => broken(origin, format!("is not valid JSON: {e}")),
    })
}

pub fn from_value(origin: impl Into<String>, value: Value) -> ManagedSettings {
    let origin = origin.into();
    let Value::Object(mut fields) = value else {
        return broken(origin, "is not a JSON object".into());
    };
    fields.remove("$schema");
    if let Some(refusal) = refused_keys(&fields, MANAGED_KEYS, "") {
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
    if let Some(refusal) = refused_keys(&block, MANAGED_PERMISSION_KEYS, &prefix) {
        return broken(origin, refusal);
    }
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
    let disable_bypass = match block.remove("disable_bypass") {
        None => false,
        Some(Value::Bool(b)) => b,
        Some(other) => {
            return broken(
                origin,
                format!("{prefix}disable_bypass must be true or false, not {other}"),
            )
        }
    };
    ManagedSettings {
        origin,
        error: None,
        rules,
        disable_bypass,
    }
}

/// Why `fields` cannot stand, when it holds a key outside `allowed`.
fn refused_keys(fields: &Map<String, Value>, allowed: &[&str], prefix: &str) -> Option<String> {
    let mut refused: Vec<String> = fields
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .map(|key| {
            let dotted = format!("{prefix}{key}");
            match crate::renamed::renamed_to(&dotted) {
                Some(new) => format!("{dotted} (renamed to {new})"),
                None => dotted,
            }
        })
        .collect();
    if refused.is_empty() {
        return None;
    }
    refused.sort_unstable();
    let allowed: Vec<String> = allowed.iter().map(|key| format!("{prefix}{key}")).collect();
    Some(format!(
        "sets keys managed settings may not set: {} (allowed: {})",
        refused.join(", "),
        allowed.join(", ")
    ))
}

/// The layer as `config.get` and `daemon.doctor` show it: where it came from,
/// why it cannot be honoured if it cannot, and its `permissions` block in the
/// file's own shape.
pub fn report(managed: &ManagedSettings) -> Value {
    let mut block = permissions::rule_lists(managed.rules.iter());
    block.insert("disable_bypass".into(), Value::Bool(managed.disable_bypass));
    let mut report = Map::new();
    report.insert("origin".into(), Value::String(managed.origin.clone()));
    if let Some(error) = &managed.error {
        report.insert("error".into(), Value::String(error.clone()));
    }
    report.insert(permissions::KEY.into(), Value::Object(block));
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

    #[test]
    fn rules_and_the_bypass_switch_are_read() {
        let m = from_value(
            "it",
            json!({
                "$schema": "x",
                "permissions": {
                    "deny": ["Bash(curl:*)"],
                    "ask": ["Bash(git push:*)"],
                    "allow_exact": ["Bash(ls)"],
                    "disable_bypass": true,
                },
            }),
        );
        assert_eq!(m.error, None);
        assert!(m.disable_bypass);
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
    fn a_bypass_session_can_be_asked_when_the_layer_asks_denies_or_takes_bypass_away() {
        let rule = |list: &str| json!({"permissions": { list: ["Bash"] }});
        let asks = |value| from_value("it", value).asks_under_bypass();
        assert!(asks(rule("ask")));
        assert!(asks(rule("deny")));
        assert!(asks(json!({"permissions": {"disable_bypass": true}})));
        assert!(!asks(rule("allow")));
        assert!(!asks(json!({})));
        assert!(!asks(json!({"permissions": {"mode": "default"}})));
    }

    #[test]
    fn a_key_outside_the_allowed_set_breaks_the_whole_layer() {
        for (key, value) in [
            ("sandbox", json!({})),
            ("permision", json!({})),
            ("permission_rules", json!([])),
        ] {
            let m = from_value(
                "it",
                json!({ key: value, "permissions": {"deny": ["Bash"]} }),
            );
            let error = m.error.unwrap_or_else(|| panic!("{key} was accepted"));
            assert!(error.contains(key), "{error}");
            assert!(m.rules.is_empty());
        }
        for key in ["mode", "prompt_timeout_secs", "client_may_loosen"] {
            let m = from_value("it", json!({"permissions": { key: 1, "deny": ["Bash"] }}));
            let error = m.error.unwrap_or_else(|| panic!("{key} was accepted"));
            assert!(error.contains(&format!("permissions.{key}")), "{error}");
            assert!(m.rules.is_empty());
        }
    }

    #[test]
    fn an_old_key_is_refused_with_its_new_name() {
        let m = from_value("it", json!({"disable_bypass_permissions_mode": true}));
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
            json!({"permissions": ["deny"]}),
            json!(["not", "an", "object"]),
        ] {
            assert!(from_value("it", value.clone()).error.is_some(), "{value}");
        }
    }

    #[test]
    fn an_absent_file_is_no_layer_unless_it_was_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("managed-settings.json");
        assert!(read(&path, false).is_none());
        assert!(read(&path, true).unwrap().error.is_some());

        std::fs::write(&path, "{not json").unwrap();
        let broken = read(&path, false).unwrap();
        assert!(broken.error.unwrap().contains("not valid JSON"));
    }
}
