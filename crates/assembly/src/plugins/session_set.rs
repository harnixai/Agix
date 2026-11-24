//! The plugins one session draws on: its own `plugins.allow` and
//! `plugins.enabled` (`options.settings`) laid over the set the pool really
//! has active, with the dependencies of what it names taken from that same
//! set. Every contribution a package makes to a session — scripts, commands,
//! agent types, skills, MCP servers, functions, scenes — is gated on
//! [`SessionPlugins::permits`]; the set is computed once per engine build.

use crate::session_pool::api::ClampedOption;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The keys of the `plugins` section a session's own settings may set.
pub(crate) const SESSION_KEYS: [&str; 2] = ["allow", "enabled"];

/// One package of the pool's active set, as a session's set is drawn from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivePackage {
    pub name: String,
    pub version: String,
    /// The packages of the same active set this one depends on, each at a
    /// version its requirement takes.
    pub dependencies: Vec<String>,
}

/// One package a session draws on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPlugin {
    pub name: String,
    pub version: String,
    /// The packages of the session's set that depend on this one, when the
    /// session's list does not name it and it is there only as their
    /// dependency.
    pub required_by: Vec<String>,
}

/// The plugins a session draws on. [`Default`] is a session that set
/// nothing: every package the pool has active, as it changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionPlugins {
    narrowed: bool,
    packages: Vec<SessionPlugin>,
}

impl SessionPlugins {
    /// No plugin at all.
    pub fn none() -> Self {
        Self {
            narrowed: true,
            packages: Vec::new(),
        }
    }

    /// The set a session whose own settings are `layer` draws on, over the
    /// pool's `active` packages, of a pool whose policy has plugins
    /// `pool_enabled`; with what in `layer` asked for more than that, as
    /// `options` fields.
    ///
    /// A name the pool does not have active is left out and reported; the
    /// rest apply, so a list none of whose names is active leaves the session
    /// no plugin, not every plugin. A `plugins` section that does not read
    /// leaves it none too.
    pub fn resolve(
        layer: Option<&Map<String, Value>>,
        pool_enabled: bool,
        active: &[ActivePackage],
    ) -> (Self, Vec<ClampedOption>) {
        let Some(asked) = Asked::of(layer) else {
            tracing::warn!(
                "the session's `plugins` settings do not read; the session gets no plugin"
            );
            return (Self::none(), Vec::new());
        };
        let mut clamped = Vec::new();
        if asked.enabled == Some(true) && !pool_enabled {
            clamped.push(ClampedOption {
                field: "settings.plugins.enabled".into(),
                requested: Value::Bool(true),
                applied: Value::Bool(false),
            });
        }
        if asked.enabled == Some(false) {
            return (Self::none(), clamped);
        }
        let allow = asked.allow.unwrap_or_default();
        if allow.is_empty() {
            let packages = active
                .iter()
                .map(|p| SessionPlugin {
                    name: p.name.clone(),
                    version: p.version.clone(),
                    required_by: Vec::new(),
                })
                .collect();
            return (
                Self {
                    narrowed: false,
                    packages,
                },
                clamped,
            );
        }
        let is_active = |name: &str| active.iter().any(|p| p.name == name);
        let listed: Vec<String> = allow.iter().filter(|n| is_active(n)).cloned().collect();
        if listed.len() != allow.len() {
            clamped.push(ClampedOption {
                field: "settings.plugins.allow".into(),
                requested: Value::from(allow.clone()),
                applied: Value::from(listed.clone()),
            });
        }
        (
            Self {
                narrowed: true,
                packages: with_dependencies(&listed, active),
            },
            clamped,
        )
    }

    /// Whether the session draws on the package `plugin`.
    pub fn permits(&self, plugin: &str) -> bool {
        !self.narrowed || self.packages.iter().any(|p| p.name == plugin)
    }

    /// Whether the session's own settings narrowed the pool's set. Not
    /// narrowed, [`Self::permits`] follows the pool as its set changes.
    pub fn is_narrowed(&self) -> bool {
        self.narrowed
    }

    /// The packages the session draws on, in name order, as they were when
    /// the set was computed.
    pub fn packages(&self) -> &[SessionPlugin] {
        &self.packages
    }

    /// Those of `items` whose package the session draws on.
    pub fn retain<T>(&self, items: Vec<T>, plugin_of: impl Fn(&T) -> &str) -> Vec<T> {
        if !self.narrowed {
            return items;
        }
        items
            .into_iter()
            .filter(|item| self.permits(plugin_of(item)))
            .collect()
    }
}

/// `layer` without the `plugins` keys [`SessionPlugins::resolve`] judges, so
/// the settings overlay does not judge them again by its own order.
pub(crate) fn without_session_keys(layer: &Map<String, Value>) -> Map<String, Value> {
    let mut layer = layer.clone();
    if let Some(Value::Object(plugins)) = layer.get_mut("plugins") {
        for key in SESSION_KEYS {
            plugins.remove(key);
        }
        if plugins.is_empty() {
            layer.remove("plugins");
        }
    }
    layer
}

/// What a session's own settings ask of `plugins`; `None` when they set it
/// to something that does not read.
struct Asked {
    enabled: Option<bool>,
    allow: Option<Vec<String>>,
}

impl Asked {
    fn of(layer: Option<&Map<String, Value>>) -> Option<Self> {
        let plugins = match layer.and_then(|layer| layer.get("plugins")) {
            None => {
                return Some(Self {
                    enabled: None,
                    allow: None,
                })
            }
            Some(Value::Object(plugins)) => plugins,
            Some(_) => return None,
        };
        let enabled = match plugins.get("enabled") {
            None => None,
            Some(Value::Bool(enabled)) => Some(*enabled),
            Some(_) => return None,
        };
        let allow = match plugins.get("allow") {
            None => None,
            Some(Value::Array(names)) => {
                let mut seen = BTreeSet::new();
                let names: Option<Vec<String>> = names
                    .iter()
                    .map(|n| n.as_str().map(str::to_string))
                    .collect();
                Some(
                    names?
                        .into_iter()
                        .filter(|n| seen.insert(n.clone()))
                        .collect(),
                )
            }
            Some(_) => return None,
        };
        Some(Self { enabled, allow })
    }
}

/// `listed` and every package of `active` they depend on, directly or not,
/// in `active`'s order.
fn with_dependencies(listed: &[String], active: &[ActivePackage]) -> Vec<SessionPlugin> {
    let by_name: BTreeMap<&str, &ActivePackage> =
        active.iter().map(|p| (p.name.as_str(), p)).collect();
    let mut members: BTreeSet<&str> = BTreeSet::new();
    let mut queue: Vec<&str> = listed.iter().map(String::as_str).collect();
    while let Some(name) = queue.pop() {
        let Some(package) = by_name.get(name) else {
            continue;
        };
        if !members.insert(package.name.as_str()) {
            continue;
        }
        queue.extend(package.dependencies.iter().map(String::as_str));
    }
    active
        .iter()
        .filter(|p| members.contains(p.name.as_str()))
        .map(|p| SessionPlugin {
            name: p.name.clone(),
            version: p.version.clone(),
            required_by: if listed.contains(&p.name) {
                Vec::new()
            } else {
                active
                    .iter()
                    .filter(|q| members.contains(q.name.as_str()))
                    .filter(|q| q.dependencies.contains(&p.name))
                    .map(|q| q.name.clone())
                    .collect()
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn package(name: &str, dependencies: &[&str]) -> ActivePackage {
        ActivePackage {
            name: name.into(),
            version: "1.0.0".into(),
            dependencies: dependencies.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn layer(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn names(set: &SessionPlugins) -> Vec<&str> {
        set.packages().iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn a_session_that_sets_nothing_follows_the_pool() {
        let active = [package("a", &[]), package("b", &[])];
        let (set, clamped) = SessionPlugins::resolve(None, true, &active);
        assert!(!set.is_narrowed());
        assert_eq!(names(&set), ["a", "b"]);
        assert!(set.permits("installed-later"));
        assert!(clamped.is_empty());
        let (set, _) = SessionPlugins::resolve(
            Some(&layer(json!({"plugins": {"allow": []}}))),
            true,
            &active,
        );
        assert!(!set.is_narrowed(), "an empty list is no list");
    }

    #[test]
    fn a_list_is_intersected_with_the_active_set_and_what_is_not_there_reported() {
        let active = [package("a", &[]), package("b", &[])];
        let asked = layer(json!({"plugins": {"allow": ["a", "c"]}}));
        let (set, clamped) = SessionPlugins::resolve(Some(&asked), true, &active);
        assert_eq!(names(&set), ["a"]);
        assert!(!set.permits("b") && !set.permits("c"));
        assert_eq!(
            clamped,
            [ClampedOption {
                field: "settings.plugins.allow".into(),
                requested: json!(["a", "c"]),
                applied: json!(["a"]),
            }]
        );
    }

    #[test]
    fn a_list_of_nothing_active_leaves_no_plugin_rather_than_every_one() {
        let active = [package("a", &[])];
        let asked = layer(json!({"plugins": {"allow": ["ghost"]}}));
        let (set, clamped) = SessionPlugins::resolve(Some(&asked), true, &active);
        assert!(set.is_narrowed());
        assert!(set.packages().is_empty());
        assert!(!set.permits("a"));
        assert_eq!(clamped[0].applied, json!([]));
    }

    #[test]
    fn dependencies_come_along_from_the_active_set_only_and_cycles_end() {
        let active = [
            package("a", &["b"]),
            package("b", &["c", "a"]),
            package("c", &[]),
            package("d", &[]),
        ];
        let asked = layer(json!({"plugins": {"allow": ["a"]}}));
        let (set, clamped) = SessionPlugins::resolve(Some(&asked), true, &active);
        assert_eq!(names(&set), ["a", "b", "c"]);
        assert!(clamped.is_empty());
        let by = |n: &str| {
            set.packages()
                .iter()
                .find(|p| p.name == n)
                .unwrap()
                .required_by
                .clone()
        };
        assert!(by("a").is_empty(), "listed itself");
        assert_eq!(by("b"), ["a"]);
        assert_eq!(by("c"), ["b"]);

        let missing = [package("a", &["gone"])];
        let (set, _) = SessionPlugins::resolve(Some(&asked), true, &missing);
        assert_eq!(names(&set), ["a"]);
    }

    #[test]
    fn enabled_false_is_no_plugin_and_true_cannot_loosen_the_pool() {
        let active = [package("a", &[])];
        let off = layer(json!({"plugins": {"enabled": false, "allow": ["a"]}}));
        let (set, clamped) = SessionPlugins::resolve(Some(&off), true, &active);
        assert!(set.packages().is_empty() && !set.permits("a"));
        assert!(clamped.is_empty());

        let on = layer(json!({"plugins": {"enabled": true}}));
        let (set, clamped) = SessionPlugins::resolve(Some(&on), false, &[]);
        assert!(set.packages().is_empty());
        assert_eq!(clamped[0].field, "settings.plugins.enabled");
        assert_eq!(clamped[0].applied, json!(false));
    }

    #[test]
    fn a_plugins_section_that_does_not_read_leaves_no_plugin() {
        let active = [package("a", &[])];
        for unread in [
            json!({"plugins": "a"}),
            json!({"plugins": {"allow": "a"}}),
            json!({"plugins": {"allow": [1]}}),
            json!({"plugins": {"allow": null}}),
            json!({"plugins": {"enabled": "yes"}}),
        ] {
            let (set, _) = SessionPlugins::resolve(Some(&layer(unread.clone())), true, &active);
            assert!(!set.permits("a"), "{unread}");
        }
    }

    #[test]
    fn the_overlay_is_given_the_layer_without_the_keys_judged_here() {
        let given = layer(json!({
            "plugins": {"allow": ["a"], "enabled": true},
            "model": {"effort": "high"},
        }));
        assert_eq!(
            Value::Object(without_session_keys(&given)),
            json!({"model": {"effort": "high"}})
        );
        let kept = layer(json!({"plugins": {"enable_on_install": true, "allow": ["a"]}}));
        assert_eq!(
            Value::Object(without_session_keys(&kept)),
            json!({"plugins": {"enable_on_install": true}})
        );
    }
}
