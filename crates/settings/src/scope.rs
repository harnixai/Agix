//! Where each settings key is decided: once for the process, once for each
//! (project, scene) pair a session runs on, or by each session. A key is read
//! where its scope says — the providers' routing and the MCP servers are built
//! for a pair, the turn limits and the sandbox for a session — so a layer that
//! applies to less than that scope cannot set it: a session's own settings
//! ([`crate::overlay`]) hold only [`Scope::Session`] keys. [`SCOPES`] names
//! every key, as [`crate::tier_gate::PROJECT_TIER`] does.

use serde_json::Value;

use crate::schema::schema_keys;
use crate::section::Sections;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Read once, when the process starts or its pool is built.
    Process,
    /// Read when a (project, scene) pair is bound, for every session on it.
    Pair,
    /// Read when a session's engine is built.
    Session,
}

impl Scope {
    /// How a refusal names it.
    pub fn decided(self) -> &'static str {
        match self {
            Self::Process => "it is decided once for the process",
            Self::Pair => "it is decided for each project and scene, not each session",
            Self::Session => "it is decided for each session",
        }
    }
}

/// Every settings key, dotted, with `*` for one map key. A key takes the scope
/// of the longest entry it starts with.
pub const SCOPES: &[(&str, Scope)] = {
    use Scope::{Pair, Process, Session};
    &[
        ("$schema", Session),
        ("agents", Session),
        ("app", Process),
        ("compaction", Session),
        ("files", Session),
        ("goal", Session),
        ("hooks", Pair),
        ("mcp", Pair),
        ("memory", Session),
        ("model.effort", Session),
        ("model.max_tokens", Session),
        ("model.order", Pair),
        ("model.tasks", Pair),
        ("model.thinking", Session),
        ("permissions", Session),
        ("permissions.client_may_loosen", Pair),
        ("prompt", Session),
        ("providers", Pair),
        ("recorder", Pair),
        ("sandbox", Session),
        ("schedule", Session),
        ("session", Session),
        ("skills", Session),
        ("telemetry.enabled", Session),
        ("telemetry.otlp_endpoint", Process),
        ("turn", Session),
        ("web", Session),
    ]
};

/// The keys the engine reads only when the process starts or its pool is
/// built, dotted: a changed value reaches a running process only by
/// restarting it.
pub const READ_AT_START: &[&str] = &[
    "app.history",
    "app.session_cap",
    "app.session_idle_timeout_secs",
    "app.system_proxy",
    "app.worker_threads",
];

/// Each of [`READ_AT_START`] as `settings` has it; `null` when unset.
pub fn read_at_start(settings: &crate::Settings) -> Vec<(&'static str, Value)> {
    let values = serde_json::to_value(settings).unwrap_or_default();
    READ_AT_START
        .iter()
        .map(|key| {
            let value = key
                .split('.')
                .try_fold(&values, |node, segment| node.get(segment))
                .cloned()
                .unwrap_or(Value::Null);
            (*key, value)
        })
        .collect()
}

type Entries = Vec<(Vec<String>, Scope)>;

fn entries(sections: &Sections) -> Entries {
    SCOPES
        .iter()
        .map(|(key, scope)| (key.to_string(), *scope))
        .chain(
            sections
                .iter()
                .map(|spec| (spec.key.to_string(), spec.scope)),
        )
        .map(|(key, scope)| (key.split('.').map(String::from).collect(), scope))
        .collect()
}

fn governing<'a>(entries: &'a Entries, path: &[&str]) -> Option<&'a (Vec<String>, Scope)> {
    entries
        .iter()
        .filter(|(key, _)| {
            key.len() <= path.len() && key.iter().zip(path).all(|(k, p)| k == "*" || k == p)
        })
        .max_by_key(|(key, _)| key.len())
}

/// The scope of the dotted `key`; `None` for one no entry names.
pub fn scope_of(key: &[&str], sections: &Sections) -> Option<Scope> {
    governing(&entries(sections), key).map(|(_, scope)| *scope)
}

/// The keys of `layer` whose scope is not `scope`, dotted at the entry that
/// gives them theirs (`model.order`, `hooks`), each with that scope. A key no
/// entry names is not among them.
pub fn keys_outside(layer: &Value, scope: Scope, sections: &Sections) -> Vec<(String, Scope)> {
    let entries = entries(sections);
    let mut found = Vec::new();
    walk(&entries, layer, &mut Vec::new(), scope, &mut found);
    found
}

fn walk(
    entries: &Entries,
    value: &Value,
    path: &mut Vec<String>,
    scope: Scope,
    found: &mut Vec<(String, Scope)>,
) {
    let Some(fields) = value.as_object() else {
        return;
    };
    for (key, inner) in fields {
        path.push(key.clone());
        let segments: Vec<&str> = path.iter().map(String::as_str).collect();
        let beneath = entries.iter().any(|(entry, _)| {
            entry.len() > segments.len()
                && entry.iter().zip(&segments).all(|(e, s)| e == "*" || e == s)
        });
        match governing(entries, &segments) {
            Some((entry, own)) if *own != scope && !beneath => {
                let at = segments[..entry.len()].join(".");
                if !found.iter().any(|(k, _)| *k == at) {
                    found.push((at, *own));
                }
            }
            _ => walk(entries, inner, path, scope, found),
        }
        path.pop();
    }
}

/// What [`SCOPES`] fails to cover in `schema`, as messages: a key with no
/// entry, and an entry that names no key.
pub fn scope_gaps(schema: &Value, sections: &Sections) -> Vec<String> {
    let entries = entries(sections);
    let mut gaps = Vec::new();
    let mut used = std::collections::HashSet::new();
    for key in schema_keys(schema) {
        let segments: Vec<&str> = key.iter().map(String::as_str).collect();
        match governing(&entries, &segments) {
            Some((entry, _)) => {
                used.insert(entry.join("."));
            }
            None => gaps.push(format!("`{}` has no scope", key.join("."))),
        }
    }
    for (entry, _) in &entries {
        let entry = entry.join(".");
        if !entry.starts_with('$') && !used.contains(&entry) {
            gaps.push(format!("the scope entry `{entry}` names no settings key"));
        }
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn what_is_read_at_start_is_decided_for_the_process() {
        for key in READ_AT_START {
            let path: Vec<&str> = key.split('.').collect();
            assert_eq!(
                scope_of(&path, &Sections::none()),
                Some(Scope::Process),
                "{key}"
            );
        }
        let read = read_at_start(&crate::Settings::defaults_for("m"));
        assert_eq!(read.len(), READ_AT_START.len());
        assert_eq!(read[1], ("app.session_cap", json!(32)));
    }

    #[test]
    fn every_settings_key_has_a_scope() {
        let sections = Sections::none();
        let schema = crate::settings_json_schema(&sections);
        assert_eq!(scope_gaps(&schema, &sections), Vec::<String>::new());
    }

    #[test]
    fn a_layer_names_the_keys_decided_elsewhere_where_they_are_decided() {
        let layer = json!({
            "model": {"order": ["a"], "effort": "low", "tasks": {"main": "fast"}},
            "mcp": {"servers": {"gh": {"enabled": false}}},
            "permissions": {"mode": "plan", "client_may_loosen": true},
            "app": {"trust_all_projects": true},
            "turn": {"token_budget": 1},
        });
        let mut found: Vec<_> = keys_outside(&layer, Scope::Session, &Sections::none())
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                "app",
                "mcp",
                "model.order",
                "model.tasks",
                "permissions.client_may_loosen"
            ]
        );
    }
}
