//! Why a settings key has its value (`config.explain`): each layer the loader
//! folded — the built-in defaults, the global, scene and project files (each
//! with its `settings.local.json`), the `cli` layer — and a host's in-memory
//! layer, with what became of the key in each; the managed layer over them;
//! and a session's own layer over those.
//!
//! Nothing is merged again here. The layers are the ones the source handed
//! the loader, and what the loader did with them is read off what it recorded
//! while folding them — the withheld project keys, the values left out — and
//! off the key table ([`crate::keys`]) its gates read.

use std::collections::BTreeSet;

use base::interface::config_source::{ConfigLayer, Tier};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::keys::{Scope, Table, Trust, Writers};
use crate::loaded::{seed_section_defaults, TierView};
use crate::overlay::value_at;
use crate::permissions::{self, rule_lists, RULE_LISTS};
use crate::section::Sections;
use crate::{KeyKind, LoadedSettings, SchemaKeys, SessionConfig, Settings};

/// How many keys the explanation of a block goes into.
pub const MAX_KEYS: usize = 64;

/// What the settings were made of, and what came of them.
pub struct Sources<'a> {
    /// The layers as the source handed them to the loader, lowest first.
    pub layers: &'a [ConfigLayer],
    /// What a host laid over the loaded settings in memory.
    pub host: Option<&'a Map<String, Value>>,
    /// The loaded settings with the host's layer and the managed layer on
    /// them.
    pub settings: &'a LoadedSettings,
    /// A session over `settings`.
    pub session: Option<&'a SessionConfig>,
}

/// What became of a key in one layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SettingOutcome {
    /// This layer's value is the one in force. For the lists that join
    /// across layers (`providers`, the `permissions` rule lists) every layer
    /// that sets one is.
    Applied,
    /// A higher layer set the key.
    Overridden,
    /// An object a higher layer merged its own keys into; explain a key
    /// inside it to see which layer each comes from.
    Merged,
    NotSet,
    /// The project is not trusted, and this is what only a trusted project
    /// may set (or loosen).
    WithheldUntrusted,
    /// Only the global tier's files (and the layers over every file) may set
    /// it.
    DroppedNotGlobal,
    /// The value does not fit its setting; `error` says why.
    Ignored,
    /// The managed layer set it to its own value.
    ManagedOverrode,
    /// The managed layer's value: a value beneath it is stricter, and stands.
    HeldTighterBelow,
    /// The session's value: it would loosen the settings, which do not let a
    /// client loosen them (`permissions.client_may_loosen`); held to theirs.
    Clamped,
    /// The session's value: the key is decided for the process or the
    /// (project, scene) pair, not for a session.
    NotSessionScope,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SettingExplanation {
    /// As asked: a key, or a block whose keys are explained one by one.
    pub key: String,
    pub keys: Vec<KeyExplanation>,
    /// The block holds more than [`MAX_KEYS`] keys; the first are explained.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KeyExplanation {
    pub key: String,
    /// What is in force: the session's when one was named, else the pair's.
    pub value: Value,
    /// Where the key is decided; `None` only for a key no row of the key
    /// table names.
    pub scope: Option<Scope>,
    /// The engine reads it only when the process starts.
    pub read_at_start: bool,
    /// For a key read only at start: `value` is not what the process started
    /// with, and takes effect after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_pending: Option<bool>,
    /// Lowest first: `default`, then each file and the `cli` layer in the
    /// order the loader folded them, then `host`.
    pub layers: Vec<LayerOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<ManagedOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionOutcome>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LayerOutcome {
    /// `default`, `global`, `scene`, `project`, `cli` or `host`.
    pub tier: String,
    /// The file, `built-in`, `cli` or `host`.
    pub origin: String,
    /// The tier's `settings.local.json`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub machine_local: bool,
    /// What the layer says, absent when it says nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    pub outcome: SettingOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ManagedOutcome {
    pub origin: String,
    pub value: Value,
    /// `applied`, `held_tighter_below`, or `ignored` when the value does
    /// not fit.
    pub outcome: SettingOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionOutcome {
    /// What the session's own layer asks for; `null` when it asks nothing.
    pub requested: Option<Value>,
    pub applied: Value,
    /// `not_set`, `applied`, `clamped`, `managed_overrode`,
    /// `not_session_scope`, `ignored`, or `overridden` when the session
    /// decided otherwise since (`session.setPermissionMode`).
    pub outcome: SettingOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `key`, dotted, explained over `sources`; a block, key by key. Refused
/// when no setting reads `key`.
pub fn explain(
    sources: &Sources<'_>,
    key: &str,
    sections: &Sections,
) -> Result<SettingExplanation, String> {
    let segments: Vec<&str> = key.split('.').collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(format!("`{key}` is not a dotted settings key"));
    }
    let explainer = Explainer::new(sources, sections);
    let mut leaves = explainer.leaves(&segments)?;
    let truncated = leaves.len() > MAX_KEYS;
    leaves.truncate(MAX_KEYS);
    Ok(SettingExplanation {
        key: key.to_string(),
        keys: leaves.iter().map(|leaf| explainer.key(leaf)).collect(),
        truncated,
    })
}

/// The settings as in force, the `permissions` rule lists of every layer
/// among them.
fn in_force(settings: &LoadedSettings) -> Value {
    let mut value = serde_json::to_value(&settings.values).unwrap_or_default();
    for (list, rules) in rule_lists(settings.rules.iter().map(|r| &r.rule)) {
        value[permissions::KEY][list] = rules;
    }
    value
}

/// The lists every layer adds to rather than replaces.
fn joins(key: &str) -> bool {
    key == crate::loaded::PROVIDERS
        || RULE_LISTS
            .iter()
            .any(|(list, _, _)| key == format!("{}.{list}", permissions::KEY))
}

/// Whether `outer`, dotted, is `key` or holds it.
fn covers(outer: &str, key: &str) -> bool {
    outer.is_empty() || key == outer || key.starts_with(&format!("{outer}."))
}

struct Explainer<'a> {
    sources: &'a Sources<'a>,
    schema: SchemaKeys,
    table: Table,
    tiers: TierView,
    defaults: Value,
    pair: Value,
    managed: Option<(&'a str, Value)>,
    session: Option<Value>,
}

impl<'a> Explainer<'a> {
    fn new(sources: &'a Sources<'a>, sections: &Sections) -> Self {
        let paths = &sources.settings.paths;
        let mut defaults = serde_json::to_value(Settings::defaults()).unwrap_or_default();
        seed_section_defaults(&mut defaults, sections);
        let managed = sources
            .settings
            .managed
            .as_ref()
            .filter(|m| m.error.is_none())
            .map(|m| (m.origin.as_str(), Value::Object(crate::managed::as_file(m))));
        let session = sources.session.map(|config| {
            let mut value = in_force(config.settings());
            value[permissions::KEY]["mode"] = config.applied("permissions.mode");
            value
        });
        Self {
            schema: SchemaKeys::of(sections),
            table: Table::of(sections),
            tiers: TierView::of(
                &paths.global_data_dir,
                &paths.user_data_dir,
                &paths.local_data_dir,
            ),
            defaults,
            pair: in_force(sources.settings),
            managed,
            session,
            sources,
        }
    }

    fn leaves(&self, segments: &[&str]) -> Result<Vec<String>, String> {
        let dotted = segments.join(".");
        match self.schema.kind(segments) {
            KeyKind::Value => Ok(vec![dotted]),
            KeyKind::Section => {
                let mut out = Vec::new();
                let mut path = segments.iter().map(|s| s.to_string()).collect();
                self.leaves_under(&mut path, &mut out);
                Ok(out)
            }
            KeyKind::Unknown if self.schema.holds(segments) => Ok(vec![dotted]),
            KeyKind::Unknown => Err(match crate::renamed::renamed_to(&dotted) {
                Some(new) => format!("`{dotted}` was renamed to `{new}`"),
                None => format!("no setting reads `{dotted}`"),
            }),
        }
    }

    /// The keys beneath the block at `path` that some layer, the defaults or
    /// what is in force holds, in order, one past [`MAX_KEYS`] at most.
    fn leaves_under(&self, path: &mut Vec<String>, out: &mut Vec<String>) {
        let children: BTreeSet<String> = self
            .values_at(&path.join("."))
            .iter()
            .filter_map(Value::as_object)
            .flat_map(|fields| fields.keys().cloned())
            .collect();
        for child in children {
            if out.len() > MAX_KEYS {
                return;
            }
            path.push(child);
            let kind = {
                let segments: Vec<&str> = path.iter().map(String::as_str).collect();
                self.schema.kind(&segments)
            };
            match kind {
                KeyKind::Section => self.leaves_under(path, out),
                KeyKind::Value => out.push(path.join(".")),
                KeyKind::Unknown => {}
            }
            path.pop();
        }
    }

    fn values_at(&self, key: &str) -> Vec<Value> {
        let mut values = vec![value_at(&self.defaults, key), value_at(&self.pair, key)];
        values.extend(self.sources.layers.iter().map(|l| value_at(&l.value, key)));
        if let Some(host) = self.sources.host {
            values.push(value_at(&Value::Object(host.clone()), key));
        }
        if let Some((_, file)) = &self.managed {
            values.push(value_at(file, key));
        }
        if let Some(session) = &self.session {
            values.push(value_at(session, key));
        }
        values
    }

    fn key(&self, key: &str) -> KeyExplanation {
        let segments: Vec<&str> = key.split('.').collect();
        let mut layers = self.layers(key, &segments);
        let managed = self.managed(key, &segments, &mut layers);
        let session = self.session(key, &segments);
        let value = match (&self.session, &session) {
            (Some(in_session), Some(_)) => value_at(in_session, key),
            _ => value_at(&self.pair, key),
        };
        let read_at_start = self
            .table
            .governing(&segments, |k| k.read_at_start.then_some(()))
            .is_some_and(|(row, _)| row.len() == segments.len());
        KeyExplanation {
            key: key.to_string(),
            value,
            scope: self.table.governing(&segments, |k| k.scope).map(|(_, s)| s),
            read_at_start,
            restart_pending: None,
            layers,
            managed,
            session,
        }
    }

    fn layers(&self, key: &str, segments: &[&str]) -> Vec<LayerOutcome> {
        let default = value_at(&self.defaults, key);
        let mut out = vec![LayerOutcome {
            tier: "default".into(),
            origin: "built-in".into(),
            machine_local: false,
            outcome: if default.is_null() {
                SettingOutcome::NotSet
            } else {
                SettingOutcome::Applied
            },
            value: Some(default).filter(|v| !v.is_null()),
            error: None,
        }];
        for layer in self
            .sources
            .layers
            .iter()
            .filter(|l| l.tier != Tier::Managed)
        {
            out.push(self.file_layer(layer, key, segments));
        }
        if let Some(host) = self.sources.host {
            let host = Value::Object(host.clone());
            out.push(self.judged(("host", "host", false), &host, key, || None));
        }
        if !joins(key) {
            settle(&mut out);
        }
        out
    }

    fn file_layer(&self, layer: &ConfigLayer, key: &str, segments: &[&str]) -> LayerOutcome {
        let from = (
            layer.tier.name(),
            layer.origin.as_str(),
            layer.machine_local,
        );
        let is_global = self.tiers.is_global(layer.tier);
        let mut judged = self.judged(from, &layer.value, key, || {
            if !is_global && self.global_only(segments) {
                Some(SettingOutcome::DroppedNotGlobal)
            } else if self.withheld(layer, key) {
                Some(SettingOutcome::WithheldUntrusted)
            } else {
                None
            }
        });
        // A scene or project directory that is the global one is read once,
        // as the global tier.
        if is_global && layer.tier != Tier::Cli {
            judged.tier = Tier::Global.name().to_string();
        }
        judged
    }

    /// What the layer `(tier, origin, machine_local)` holding `layer` says at
    /// `key`, and what became of it: left out when the loader recorded it as
    /// not fitting, not set, what `gate` says, else applied until [`settle`]
    /// ranks it.
    fn judged(
        &self,
        (tier, origin, machine_local): (&str, &str, bool),
        layer: &Value,
        key: &str,
        gate: impl Fn() -> Option<SettingOutcome>,
    ) -> LayerOutcome {
        let raw = value_at(layer, key);
        let ignored = self
            .sources
            .settings
            .ignored
            .iter()
            .find(|i| i.tier == tier && i.origin == origin && covers(&i.key, key));
        let (outcome, value, error) = match ignored {
            Some(ignored) => {
                let value = Some(&ignored.key)
                    .filter(|k| !k.is_empty())
                    .map(|k| value_at(layer, k));
                (SettingOutcome::Ignored, value, Some(ignored.error.clone()))
            }
            None if raw.is_null() => (SettingOutcome::NotSet, None, None),
            None => (gate().unwrap_or(SettingOutcome::Applied), Some(raw), None),
        };
        LayerOutcome {
            tier: tier.to_string(),
            origin: origin.to_string(),
            machine_local,
            value,
            outcome,
            error,
        }
    }

    fn global_only(&self, segments: &[&str]) -> bool {
        matches!(
            self.table.governing(segments, |k| k.writers),
            Some((_, Writers::GlobalOnly))
        )
    }

    fn withheld(&self, layer: &ConfigLayer, key: &str) -> bool {
        let settings = self.sources.settings;
        self.tiers.is_own_project(layer.tier)
            && settings.trust().is_untrusted()
            && settings.withheld().iter().any(|w| {
                w.origin == layer.origin
                    && (covers(&w.key, key) || w.key.starts_with(&format!("{key}(")))
            })
    }

    fn managed(
        &self,
        key: &str,
        segments: &[&str],
        layers: &mut [LayerOutcome],
    ) -> Option<ManagedOutcome> {
        let (origin, file) = self.managed.as_ref()?;
        let value = value_at(file, key);
        if value.is_null() {
            return None;
        }
        let in_force = value_at(&self.pair, key);
        let below = |from: SettingOutcome, to: SettingOutcome, layers: &mut [LayerOutcome]| {
            for layer in layers.iter_mut().filter(|l| l.outcome == from) {
                layer.outcome = to;
            }
        };
        let outcome = if joins(key) {
            SettingOutcome::Applied
        } else if value.is_object() && in_force.is_object() {
            below(SettingOutcome::Applied, SettingOutcome::Merged, layers);
            SettingOutcome::Applied
        } else if in_force == value {
            below(
                SettingOutcome::Applied,
                SettingOutcome::ManagedOverrode,
                layers,
            );
            SettingOutcome::Applied
        } else if matches!(
            self.table.governing(segments, |k| k.trust),
            Some((_, Trust::Tighten(_)))
        ) {
            SettingOutcome::HeldTighterBelow
        } else {
            SettingOutcome::Ignored
        };
        Some(ManagedOutcome {
            origin: origin.to_string(),
            value,
            outcome,
        })
    }

    fn session(&self, key: &str, segments: &[&str]) -> Option<SessionOutcome> {
        let config = self.sources.session?;
        let applied = value_at(self.session.as_ref()?, key);
        let requested = config.requested(key);
        let (outcome, error) = match &requested {
            None => (SettingOutcome::NotSet, None),
            Some(asked) => self.session_outcome(config, key, segments, asked, &applied),
        };
        Some(SessionOutcome {
            requested,
            applied,
            outcome,
            error,
        })
    }

    fn session_outcome(
        &self,
        config: &SessionConfig,
        key: &str,
        segments: &[&str],
        asked: &Value,
        applied: &Value,
    ) -> (SettingOutcome, Option<String>) {
        let scope = self.table.governing(segments, |k| k.scope).map(|(_, s)| s);
        if scope.is_some_and(|scope| scope != Scope::Session) {
            return (SettingOutcome::NotSessionScope, None);
        }
        let held = config.held().iter().any(|h| covers(&h.key, key))
            || (key == "permissions.mode" && asked != applied);
        if held {
            let managed = self
                .managed
                .as_ref()
                .is_some_and(|(_, file)| value_at(file, key) == *applied);
            let outcome = if managed {
                SettingOutcome::ManagedOverrode
            } else {
                SettingOutcome::Clamped
            };
            return (outcome, None);
        }
        let session = Tier::Session.name();
        if let Some(ignored) = config
            .settings()
            .ignored
            .iter()
            .find(|i| i.tier == session && covers(&i.key, key))
        {
            return (SettingOutcome::Ignored, Some(ignored.error.clone()));
        }
        let joined = joins(key)
            && asked.as_array().is_some_and(|items| {
                items
                    .iter()
                    .all(|i| applied.as_array().is_some_and(|a| a.contains(i)))
            });
        if joined || asked == applied {
            (SettingOutcome::Applied, None)
        } else {
            (SettingOutcome::Overridden, None)
        }
    }
}

/// Rank the layers that set a key: the highest is applied; one beneath is
/// merged into while every layer over it that sets the key sets an object,
/// else overridden.
fn settle(layers: &mut [LayerOutcome]) {
    let setting: Vec<usize> = (0..layers.len())
        .filter(|&i| layers[i].outcome == SettingOutcome::Applied)
        .collect();
    let is_object = |l: &LayerOutcome| l.value.as_ref().is_some_and(Value::is_object);
    for (n, &i) in setting.iter().enumerate() {
        let above = &setting[n + 1..];
        if above.is_empty() {
            break;
        }
        let merged = is_object(&layers[i]) && above.iter().all(|&j| is_object(&layers[j]));
        layers[i].outcome = if merged {
            SettingOutcome::Merged
        } else {
            SettingOutcome::Overridden
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::config_source::InMemoryLayers;
    use serde_json::json;

    fn explained(layers: &InMemoryLayers, key: &str) -> KeyExplanation {
        let settings = LoadedSettings::load_from(crate::LoadRequest {
            source: layers,
            global_dir: "/explain/g".into(),
            scene_dir: "/explain/s".into(),
            local_dir: "/explain/p/.agix".into(),
            scope: "code",
            default_model: "m",
            sections: &Sections::none(),
            layout: Default::default(),
        });
        let sources = Sources {
            layers: &layers.0,
            host: None,
            settings: &settings,
            session: None,
        };
        let mut out = explain(&sources, key, &Sections::none()).unwrap();
        assert_eq!(out.keys.len(), 1, "{out:?}");
        out.keys.remove(0)
    }

    fn outcomes(explained: &KeyExplanation) -> Vec<(&str, SettingOutcome)> {
        explained
            .layers
            .iter()
            .map(|l| (l.tier.as_str(), l.outcome))
            .collect()
    }

    #[test]
    fn rule_lists_join_and_an_untrusted_projects_allow_rules_are_withheld() {
        let layers = InMemoryLayers::new()
            .with(
                Tier::Global,
                false,
                "g",
                json!({"permissions": {"allow": ["Read"]}}),
            )
            .with(
                Tier::Project,
                false,
                "p",
                json!({"permissions": {"allow": ["Bash"], "deny": ["Write"]}}),
            )
            .with(
                Tier::Cli,
                false,
                "cli",
                json!({"permissions": {"deny": ["Edit"]}}),
            );
        let allow = explained(&layers, "permissions.allow");
        assert_eq!(allow.value, json!(["Read"]));
        assert_eq!(
            outcomes(&allow),
            [
                ("default", SettingOutcome::NotSet),
                ("global", SettingOutcome::Applied),
                ("project", SettingOutcome::WithheldUntrusted),
                ("cli", SettingOutcome::NotSet),
            ]
        );
        let deny = explained(&layers, "permissions.deny");
        assert_eq!(deny.value, json!(["Write", "Edit"]));
        assert_eq!(deny.layers[2].outcome, SettingOutcome::Applied);
        assert_eq!(deny.layers[3].outcome, SettingOutcome::Applied);
    }

    #[test]
    fn an_object_a_higher_layer_adds_to_is_merged_and_a_value_that_does_not_fit_is_ignored() {
        let layers = InMemoryLayers::new()
            .with(
                Tier::Global,
                false,
                "g",
                json!({"hooks": {"Stop": []}, "turn": {"max_model_calls": "many"}}),
            )
            .with(Tier::Scene, true, "s", json!({"hooks": {"PreToolUse": []}}));
        let hooks = explained(&layers, "hooks");
        assert_eq!(
            outcomes(&hooks),
            [
                ("default", SettingOutcome::NotSet),
                ("global", SettingOutcome::Merged),
                ("scene", SettingOutcome::Applied),
            ]
        );
        assert!(hooks.layers[2].machine_local);

        let calls = explained(&layers, "turn.max_model_calls");
        assert_eq!(calls.layers[1].outcome, SettingOutcome::Ignored);
        assert!(calls.layers[1].error.is_some(), "{calls:?}");
        assert_eq!(calls.layers[0].outcome, SettingOutcome::Applied);
        assert_eq!(calls.value, json!(25));
    }
}
