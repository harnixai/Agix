//! The one table of settings keys: for every key, where it is decided
//! ([`Scope`]), what a project that is not trusted may do with it ([`Trust`]),
//! whether the engine reads it only at start, and which tiers' files may set
//! it ([`Writers`]). [`KEYS`] holds the core keys; a registered section adds
//! its rows under its own key ([`crate::section::Section::KEYS`]).
//!
//! A row names a key dotted, with `*` for one map key. For each property a key
//! takes the longest row it starts with that sets the property, so a property
//! set on a short row (`agents` is decided per session) holds for every key
//! beneath it until a longer row sets it again. [`key_gaps`] finds a settings
//! key some property misses and a row that names no key.
//!
//! The trust gate ([`crate::tier_gate`]), the session overlay
//! ([`crate::overlay`]), the loader's tier filter and the restart report all
//! read this table.

use serde_json::{Map, Value};

use crate::schema::schema_keys;
use crate::section::Sections;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
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

/// What a project that is not trusted may do with a key. A key no row gives
/// one is withheld; a row shorter than the keys it governs must be
/// `Withheld`, so a field added beneath it later cannot pass unclassified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Free,
    /// Kept when the value is at least as tight as what is merged beneath it.
    Tighten(Order),
    Withheld,
}

/// How to tell whether a value is at least as tight as the one beneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    TrueTighter,
    FalseTighter,
    /// `PermissionMode::permissiveness`.
    Mode,
    /// `deny_all` < `allowlist`, `proxy` < `unrestricted`; `allowlist` and
    /// `proxy` cannot replace each other.
    NetworkMode,
    /// `allowlist` < `inherit`.
    EnvMode,
    Subset,
    /// A subset, where an empty list admits everything.
    SubsetEmptyIsAll,
    Superset,
    /// A number, where `null` is no limit.
    Lower,
    /// A number at least the one beneath: a minimum, where `null` is none.
    Higher,
    /// A number, where `0` is no limit.
    LowerZeroIsWidest,
    /// A list of providers the tiers beneath already define.
    ProviderDefinedBelow,
}

/// Which settings files may set a key. A key no row gives one is read from
/// every tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writers {
    AnyTier,
    /// The global tier's files, and the layers over every file (`cli`,
    /// managed, a host's); the loader drops it from the scene and project
    /// tiers.
    GlobalOnly,
}

/// One row of the table. Built with [`Key::at`] and the setters, each of
/// which sets one property; a property left unset is taken from a shorter row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Key {
    /// Dotted; in a section's rows relative to its key, `""` the key itself.
    pub path: &'static str,
    pub scope: Option<Scope>,
    pub trust: Option<Trust>,
    /// This key itself — not the keys beneath it — is read only when the
    /// process starts or its pool is built, so a changed value needs a restart.
    pub read_at_start: bool,
    pub writers: Option<Writers>,
}

impl Key {
    pub const fn at(path: &'static str) -> Self {
        Self {
            path,
            scope: None,
            trust: None,
            read_at_start: false,
            writers: None,
        }
    }

    pub const fn scope(self, scope: Scope) -> Self {
        Self {
            scope: Some(scope),
            ..self
        }
    }

    pub const fn free(self) -> Self {
        Self {
            trust: Some(Trust::Free),
            ..self
        }
    }

    pub const fn tighten(self, order: Order) -> Self {
        Self {
            trust: Some(Trust::Tighten(order)),
            ..self
        }
    }

    pub const fn withheld(self) -> Self {
        Self {
            trust: Some(Trust::Withheld),
            ..self
        }
    }

    pub const fn read_at_start(self) -> Self {
        Self {
            read_at_start: true,
            ..self
        }
    }

    pub const fn global_only(self) -> Self {
        Self {
            writers: Some(Writers::GlobalOnly),
            ..self
        }
    }
}

/// Every core settings key. The `permissions` rule lists are free because the
/// loader drops the allow rules (`allow`, `allow_exact`) of a project that is
/// not trusted itself.
pub const KEYS: &[Key] = {
    use Order::*;
    use Scope::{Pair, Process, Session};
    const fn at(path: &'static str) -> Key {
        Key::at(path)
    }
    &[
        at("$schema").scope(Session).free(),
        at("agents").scope(Session),
        at("agents.max_concurrent").tighten(Lower),
        at("agents.max_depth").tighten(Lower),
        at("app").scope(Process).withheld().global_only(),
        at("app.history").read_at_start(),
        at("app.session_cap").read_at_start(),
        at("app.session_idle_timeout_secs").read_at_start(),
        at("app.system_proxy").read_at_start(),
        at("app.worker_threads").read_at_start(),
        at("compaction").scope(Session),
        at("compaction.clear_on_cache_expiry").free(),
        at("compaction.inline_result_chars").withheld(),
        at("compaction.keep_recent").free(),
        at("compaction.reactive").free(),
        at("compaction.threshold_tokens").free(),
        at("files").scope(Session),
        at("files.allow_write").tighten(Subset),
        at("files.deny_write").tighten(Superset),
        at("files.extra_dirs").tighten(Subset),
        at("files.read_dirs").tighten(Subset),
        at("goal").scope(Session),
        at("goal.max_rounds").tighten(Lower),
        at("hooks").scope(Pair).withheld(),
        at("mcp").scope(Pair),
        at("mcp.servers.*").withheld(),
        at("mcp.servers.*.enabled").tighten(FalseTighter),
        at("memory").scope(Session),
        at("memory.enabled").free(),
        at("model.effort").scope(Session).free(),
        at("model.max_tokens").scope(Session).free(),
        at("model.order").scope(Pair).tighten(ProviderDefinedBelow),
        at("model.tasks").scope(Pair),
        at("model.tasks.compact").free(),
        at("model.tasks.main").free(),
        at("model.tasks.memory").free(),
        at("model.tasks.subagent").free(),
        at("model.tasks.title").free(),
        at("model.tasks.web_fetch").free(),
        at("model.tasks.web_search").free(),
        at("model.thinking").scope(Session).free(),
        at("permissions").scope(Session),
        at("permissions.allow").free(),
        at("permissions.allow_exact").free(),
        at("permissions.ask").free(),
        at("permissions.client_may_loosen")
            .scope(Pair)
            .tighten(FalseTighter),
        at("permissions.deny").free(),
        at("permissions.mode").tighten(Mode),
        at("permissions.prompt_timeout_secs").tighten(LowerZeroIsWidest),
        at("prompt").scope(Session),
        at("prompt.append").free(),
        at("prompt.instruction_files").withheld(),
        at("prompt.language").free(),
        at("prompt.override").withheld(),
        at("prompt.path_style").free(),
        at("providers").scope(Pair).withheld(),
        at("recorder").scope(Pair).withheld(),
        at("sandbox").scope(Session),
        at("sandbox.allow_read").tighten(Subset),
        at("sandbox.cache_dirs.*").tighten(Subset),
        at("sandbox.deny_read").tighten(Superset),
        at("sandbox.enabled").tighten(TrueTighter),
        at("sandbox.env.allow").tighten(Subset),
        at("sandbox.env.mode").tighten(EnvMode),
        at("sandbox.network.mode").tighten(NetworkMode),
        at("sandbox.network.proxy").withheld(),
        at("sandbox.reap_on_close").tighten(TrueTighter),
        at("sandbox.require").tighten(TrueTighter),
        at("sandbox.unattended_needs_network_policy").tighten(TrueTighter),
        at("schedule").scope(Session),
        at("schedule.max_model_jobs").tighten(Lower),
        at("schedule.min_model_interval_minutes").tighten(Higher),
        at("session").scope(Session),
        at("session.token_budget").tighten(Lower),
        at("skills").scope(Session),
        at("skills.allow_shell").tighten(FalseTighter),
        at("telemetry.enabled").scope(Session).tighten(TrueTighter),
        at("telemetry.otlp_endpoint").scope(Process).withheld(),
        at("turn").scope(Session),
        at("turn.max_model_calls").tighten(Lower),
        at("turn.parallel_tools").tighten(Lower),
        at("turn.token_budget").tighten(Lower),
        at("web").scope(Session),
        at("web.allowed_domains").tighten(Subset),
    ]
};

/// [`KEYS`] and each registered section's rows under its key, the paths split.
pub(crate) struct Table(Vec<(Vec<String>, Key)>);

fn agrees(row: &[String], path: &[&str]) -> bool {
    row.iter().zip(path).all(|(r, p)| r == "*" || r == p)
}

impl Table {
    pub(crate) fn of(sections: &Sections) -> Self {
        let core = KEYS.iter().map(|key| (key.path.to_string(), *key));
        let sectioned = sections.iter().flat_map(|spec| {
            spec.keys.iter().map(|key| {
                let path = if key.path.is_empty() {
                    spec.key.to_string()
                } else {
                    format!("{}.{}", spec.key, key.path)
                };
                (path, *key)
            })
        });
        Self(
            core.chain(sectioned)
                .map(|(path, key)| (path.split('.').map(String::from).collect(), key))
                .collect(),
        )
    }

    /// The longest row `path` starts with that sets `property`: its path and
    /// the property.
    pub(crate) fn governing<T>(
        &self,
        path: &[&str],
        property: impl Fn(&Key) -> Option<T>,
    ) -> Option<(&[String], T)> {
        self.0
            .iter()
            .filter(|(row, _)| row.len() <= path.len() && agrees(row, path))
            .filter_map(|(row, key)| property(key).map(|value| (row.as_slice(), value)))
            .max_by_key(|(row, _)| row.len())
    }

    /// Whether a row beneath `path` sets `property`, so `path` is a section
    /// to look into for it rather than a value to judge whole.
    pub(crate) fn sets_beneath(&self, path: &[&str], property: impl Fn(&Key) -> bool) -> bool {
        self.0
            .iter()
            .any(|(row, key)| row.len() > path.len() && agrees(row, path) && property(key))
    }
}

/// The scope of the dotted `key`; `None` for one no row gives one.
pub fn scope_of(key: &[&str], sections: &Sections) -> Option<Scope> {
    Table::of(sections)
        .governing(key, |k| k.scope)
        .map(|(_, scope)| scope)
}

/// The keys of `layer` whose scope is not `scope`, dotted at the row that
/// gives them theirs (`model.order`, `hooks`), each with that scope. A key no
/// row gives a scope is not among them.
pub fn keys_outside(layer: &Value, scope: Scope, sections: &Sections) -> Vec<(String, Scope)> {
    let table = Table::of(sections);
    let mut found = Vec::new();
    walk_scopes(&table, layer, &mut Vec::new(), scope, &mut found);
    found
}

fn walk_scopes(
    table: &Table,
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
        let beneath = table.sets_beneath(&segments, |k| k.scope.is_some());
        match table.governing(&segments, |k| k.scope) {
            Some((row, own)) if own != scope && !beneath => {
                let at = segments[..row.len()].join(".");
                if !found.iter().any(|(k, _)| *k == at) {
                    found.push((at, own));
                }
            }
            _ => walk_scopes(table, inner, path, scope, found),
        }
        path.pop();
    }
}

/// The keys read only at start, dotted, each as `settings` has it (`null`
/// when unset).
pub fn read_at_start(settings: &crate::Settings, sections: &Sections) -> Vec<(String, Value)> {
    let values = serde_json::to_value(settings).unwrap_or_default();
    Table::of(sections)
        .0
        .iter()
        .filter(|(_, key)| key.read_at_start)
        .map(|(row, _)| {
            let value = row
                .iter()
                .try_fold(&values, |node, segment| node.get(segment))
                .cloned()
                .unwrap_or(Value::Null);
            (row.join("."), value)
        })
        .collect()
}

/// The row that makes the dotted `key` one only the global tier's files may
/// set; `None` when every tier may.
pub fn global_only_at(key: &[&str], sections: &Sections) -> Option<String> {
    match Table::of(sections).governing(key, |k| k.writers) {
        Some((row, Writers::GlobalOnly)) => Some(row.join(".")),
        _ => None,
    }
}

/// Take out of `layer`, a file of a tier other than the global one, what
/// only the global tier's files may set, and return it dotted.
pub(crate) fn remove_global_only(
    layer: &mut Map<String, Value>,
    sections: &Sections,
) -> Vec<String> {
    let table = Table::of(sections);
    let mut removed = Vec::new();
    remove_global_only_in(&table, layer, &mut Vec::new(), &mut removed);
    removed
}

fn remove_global_only_in(
    table: &Table,
    section: &mut Map<String, Value>,
    path: &mut Vec<String>,
    removed: &mut Vec<String>,
) {
    let keys: Vec<String> = section.keys().cloned().collect();
    for key in keys {
        path.push(key.clone());
        let segments: Vec<&str> = path.iter().map(String::as_str).collect();
        let look_inside =
            section[&key].is_object() && table.sets_beneath(&segments, |k| k.writers.is_some());
        let global_only = matches!(
            table.governing(&segments, |k| k.writers),
            Some((_, Writers::GlobalOnly))
        );
        if look_inside {
            if let Some(Value::Object(inner)) = section.get_mut(&key) {
                remove_global_only_in(table, inner, path, removed);
            }
        } else if global_only {
            section.remove(&key);
            removed.push(path.join("."));
        }
        path.pop();
    }
}

/// What the table fails to say about `schema`, as messages: a settings key
/// with no scope or no trust, one whose trust comes from a shorter row that
/// is not `Withheld`, and a row that names no key. A settings field added
/// without a row fails the test that calls this, rather than passing through
/// an untrusted project unexamined.
pub fn key_gaps(schema: &Value, sections: &Sections) -> Vec<String> {
    let table = Table::of(sections);
    let mut gaps = Vec::new();
    let mut used = std::collections::HashSet::new();
    for key in schema_keys(schema) {
        let segments: Vec<&str> = key.iter().map(String::as_str).collect();
        let dotted = key.join(".");
        match table.governing(&segments, |k| k.scope) {
            Some((row, _)) => {
                used.insert(row.join("."));
            }
            None => gaps.push(format!("`{dotted}` has no scope")),
        }
        match table.governing(&segments, |k| k.trust) {
            Some((row, trust)) => {
                if row.len() != key.len() && trust != Trust::Withheld {
                    gaps.push(format!(
                        "`{dotted}` takes its trust from the shorter `{}`, which is not withheld",
                        row.join(".")
                    ));
                }
                used.insert(row.join("."));
            }
            None => gaps.push(format!("`{dotted}` has no trust")),
        }
        if let Some((row, _)) = table.governing(&segments, |k| k.writers) {
            used.insert(row.join("."));
        }
        if let Some((row, _)) = table.governing(&segments, |k| k.read_at_start.then_some(())) {
            if row.len() == key.len() {
                used.insert(row.join("."));
            }
        }
    }
    for (row, _) in &table.0 {
        let row = row.join(".");
        if !row.starts_with('$') && !used.contains(&row) {
            gaps.push(format!("the row `{row}` names no settings key"));
        }
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_settings_key_has_a_row() {
        let sections = Sections::none();
        let schema = crate::settings_json_schema(&sections);
        assert!(schema_keys(&schema).len() > 40);
        assert_eq!(key_gaps(&schema, &sections), Vec::<String>::new());
    }

    #[test]
    fn what_is_read_at_start_is_decided_for_the_process() {
        let sections = Sections::none();
        let read = read_at_start(&crate::Settings::defaults_for("m"), &sections);
        assert_eq!(read.len(), 5);
        for (key, _) in &read {
            let path: Vec<&str> = key.split('.').collect();
            assert_eq!(scope_of(&path, &sections), Some(Scope::Process), "{key}");
        }
        assert_eq!(read[1], ("app.session_cap".to_string(), json!(32)));
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

    #[test]
    fn only_the_global_tier_sets_the_app_block() {
        let sections = Sections::none();
        assert_eq!(
            global_only_at(&["app", "history"], &sections).as_deref(),
            Some("app")
        );
        assert_eq!(global_only_at(&["turn"], &sections), None);
        let mut layer = json!({"app": {"history": "project"}, "turn": {}})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(remove_global_only(&mut layer, &sections), ["app"]);
        assert_eq!(Value::Object(layer), json!({"turn": {}}));
    }
}
