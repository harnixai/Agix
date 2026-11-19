//! Merging one settings layer into the running result: value by value, so a
//! value that does not fit its setting is left out alone.

use crate::section::Sections;
use crate::Settings;

/// Merge `over` into `merged` — a settings object that deserializes — one
/// value at a time, leaving out each value that would stop the result from
/// deserializing. Returns the dotted keys left out, with why. An object
/// whose values all fail is reported value by value, unless an object has
/// no place there at all — then it is the value left out.
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
    if !any_fit && !fits_at(merged, path, serde_json::json!({}), sections) {
        ignored.truncate(before);
        ignored.push((path.join("."), error));
    }
    any_fit
}

/// Whether `over` at `path`, merged into `merged`, still reads.
fn fits_at(
    merged: &serde_json::Value,
    path: &[String],
    over: serde_json::Value,
    sections: &Sections,
) -> bool {
    let nested = path
        .iter()
        .rev()
        .fold(over, |inner, key| serde_json::json!({ key: inner }));
    let mut candidate = merged.clone();
    merge_json_values(&mut candidate, nested);
    fit(&candidate, sections).is_ok()
}

fn fit(candidate: &serde_json::Value, sections: &Sections) -> Result<(), String> {
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
