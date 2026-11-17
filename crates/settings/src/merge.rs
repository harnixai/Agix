//! Merging one settings layer into the running result: value by value, so a
//! value that does not fit its setting is left out alone.

use crate::section::Sections;
use crate::Settings;
use base::permission::PermissionRuleEntry;

/// One layer's `permission_rules`, rule by rule: a rule that does not read is
/// left out by itself (`permission_rules[i]`, with why), so a mistyped allow
/// does not take the deny rules beside it along. A value that is not a list
/// is left out as a whole.
pub(crate) fn read_permission_rules(
    rules: serde_json::Value,
) -> (Vec<PermissionRuleEntry>, Vec<(String, String)>) {
    let serde_json::Value::Array(entries) = rules else {
        let error = serde_json::from_value::<Vec<PermissionRuleEntry>>(rules)
            .err()
            .map_or_else(|| "expected a list".to_string(), |e| e.to_string());
        return (Vec::new(), vec![("permission_rules".to_string(), error)]);
    };
    let mut kept = Vec::new();
    let mut unread = Vec::new();
    for (index, entry) in entries.into_iter().enumerate() {
        match serde_json::from_value::<PermissionRuleEntry>(entry) {
            Ok(rule) => match rule_text_problem(&rule.tool) {
                None => kept.push(rule),
                Some(problem) => unread.push((
                    format!("permission_rules[{index}]"),
                    format!("{:?}: {problem}", rule.tool),
                )),
            },
            Err(e) => unread.push((format!("permission_rules[{index}]"), e.to_string())),
        }
    }
    (kept, unread)
}

/// What keeps a rule string such as `Bash(rm:*` from being a rule, in the
/// terms `permissions::rule::parse_rule_string` rejects it by.
fn rule_text_problem(tool: &str) -> Option<&'static str> {
    let tool = tool.trim();
    if tool.is_empty() {
        return Some("rule string is empty");
    }
    let open = tool.find('(')?;
    if !tool.ends_with(')') {
        return Some("rule has unbalanced parens");
    }
    tool[..open]
        .trim()
        .is_empty()
        .then_some("rule names no tool before the parens")
}

/// Merge `over` into `merged` — a settings object that deserializes — one
/// value at a time, leaving out each value that would stop the result from
/// deserializing. Returns the dotted keys left out, with why. An object
/// whose values all fail is reported as itself rather than value by value.
pub(crate) fn merge_fitting(
    merged: &mut serde_json::Value,
    over: serde_json::Value,
    sections: &Sections,
) -> Vec<(String, String)> {
    let mut ignored = Vec::new();
    merge_fitting_at(merged, &mut Vec::new(), over, sections, &mut ignored);
    ignored
}

fn merge_fitting_at(
    merged: &mut serde_json::Value,
    path: &mut Vec<String>,
    over: serde_json::Value,
    sections: &Sections,
    ignored: &mut Vec<(String, String)>,
) -> bool {
    let nested = path
        .iter()
        .rev()
        .fold(over.clone(), |inner, key| serde_json::json!({ key: inner }));
    let mut candidate = merged.clone();
    merge_json_values(&mut candidate, nested);
    let error = match fit(&candidate, sections) {
        Ok(()) => {
            *merged = candidate;
            return true;
        }
        Err(e) => e,
    };
    let serde_json::Value::Object(fields) = over else {
        ignored.push((path.join("."), error));
        return false;
    };
    let before = ignored.len();
    let mut any_fit = false;
    for (key, value) in fields {
        path.push(key);
        any_fit |= merge_fitting_at(merged, path, value, sections, ignored);
        path.pop();
    }
    if !any_fit {
        ignored.truncate(before);
        ignored.push((path.join("."), error));
    }
    any_fit
}

pub(crate) fn fit(candidate: &serde_json::Value, sections: &Sections) -> Result<(), String> {
    use serde::Deserialize as _;
    Settings::deserialize(candidate).map_err(|e| e.to_string())?;
    for spec in sections.iter() {
        if let Some(value) = candidate.get(spec.key) {
            spec.check(value)
                .map_err(|e| format!("{}: {e}", spec.key))?;
        }
    }
    Ok(())
}

/// Recursively merge `over` into `base` in place: objects merge key-by-key
/// (recursing into shared keys), anything else (arrays, scalars, null) fully
/// replaces the corresponding `base` value. This is the same algorithm
/// `LoadedSettings::load()` uses to merge global/scene/project tiers, exposed here
/// so other crates (e.g. `daemon`'s `config.setProvider` RPC) can apply a
/// partial patch to a single settings.json tier with identical semantics.
pub fn merge_json_values(base: &mut serde_json::Value, over: serde_json::Value) {
    match (base, over) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(over_map)) => {
            for (k, v) in over_map {
                match base_map.get_mut(&k) {
                    Some(existing) => merge_json_values(existing, v),
                    None => {
                        base_map.insert(k, v);
                    }
                }
            }
        }
        (base_slot, over_val) => {
            *base_slot = over_val;
        }
    }
}
