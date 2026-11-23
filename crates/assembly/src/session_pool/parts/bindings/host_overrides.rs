//! 宿主在内存里改过的设置：宿主拿到守护进程自己那一对的设置之后、交给池之前改掉的值，
//! 作为一层叠在池从盘上读入的每份设置上，和托管层在同一个位置。守护进程自己那一对拿到宿主改的全部；
//! 别的对只拿宿主新加的——列表里只有它新加的元素，守护进程自己项目层里来的不跨对；宿主加的权限
//! 规则只回到守护进程自己那一对上。

use base::permission::LoadedRule;
use serde_json::{Map, Value};
use settings::overlay::Subject;
use settings::LoadedSettings;

/// What the host's settings hold that the same pair read from disk does not:
/// the values it set in memory, as JSON layers, and the rules it added.
#[derive(Debug, Default, Clone)]
pub(in crate::session_pool) struct HostOverrides {
    /// Every value the host set differently, a list as the host has it.
    own: Option<Map<String, Value>>,
    /// The values the host set differently, a list only with the entries the
    /// host added to it.
    added: Option<Map<String, Value>>,
    rules: Vec<LoadedRule>,
}

impl HostOverrides {
    /// What `host` sets differently from `disk`. A key the host removed from a
    /// map, or a rule it removed, is not carried: a layer can only add or
    /// replace.
    pub(in crate::session_pool) fn between(disk: &LoadedSettings, host: &LoadedSettings) -> Self {
        let (own, added) = match (
            serde_json::to_value(&disk.values),
            serde_json::to_value(&host.values),
        ) {
            (Ok(disk), Ok(host)) => (
                changed(&disk, &host, Lists::AsTheHostHasThem),
                changed(&disk, &host, Lists::WhatTheHostAdded),
            ),
            _ => (None, None),
        };
        let on_disk: Vec<_> = disk.rules.iter().map(rule_key).collect();
        let rules = host
            .rules
            .iter()
            .filter(|rule| !on_disk.contains(&rule_key(rule)))
            .cloned()
            .collect();
        Self {
            own: own.and_then(into_object),
            added: added.and_then(into_object),
            rules,
        }
    }

    /// `settings` of a pair other than this daemon's own, with what the host
    /// added: a list is the pair's own followed by what the host added to
    /// it.
    pub(in crate::session_pool) fn apply(&self, settings: LoadedSettings) -> LoadedSettings {
        let Some(added) = &self.added else {
            return settings;
        };
        let mut layer = added.clone();
        if let Ok(Value::Object(theirs)) = serde_json::to_value(&settings.values) {
            extend_lists(&mut layer, &theirs);
        }
        overlay(&settings, &layer)
    }

    /// This daemon's own pair, read from disk, as the host made it: every
    /// value it set, and the rules it added.
    pub(in crate::session_pool) fn apply_to_own(&self, settings: LoadedSettings) -> LoadedSettings {
        let mut settings = match &self.own {
            Some(own) => overlay(&settings, own),
            None => settings,
        };
        settings.rules.extend(self.rules.iter().cloned());
        settings
    }
}

fn overlay(settings: &LoadedSettings, layer: &Map<String, Value>) -> LoadedSettings {
    settings
        .overlay(layer, Subject::Host, &crate::settings_sections::all())
        .0
}

fn into_object(value: Value) -> Option<Map<String, Value>> {
    match value {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// Each list of `layer` but `providers` — which the overlay puts first itself
/// — after the one at the same place in `theirs`, without what that already
/// holds.
fn extend_lists(layer: &mut Map<String, Value>, theirs: &Map<String, Value>) {
    for (key, value) in layer.iter_mut() {
        match (value, theirs.get(key)) {
            (Value::Object(inner), Some(Value::Object(below))) => extend_lists(inner, below),
            (Value::Array(added), Some(Value::Array(below))) if key != "providers" => {
                let mut list = below.clone();
                list.extend(added.drain(..).filter(|item| !below.contains(item)));
                *added = list;
            }
            _ => {}
        }
    }
}

fn rule_key(
    rule: &LoadedRule,
) -> (
    &base::permission::PermissionRuleEntry,
    base::interface::config_source::Tier,
    bool,
) {
    (&rule.rule, rule.tier, rule.machine_local)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lists {
    AsTheHostHasThem,
    WhatTheHostAdded,
}

fn changed(from: &Value, to: &Value, lists: Lists) -> Option<Value> {
    match (from, to) {
        (Value::Object(from), Value::Object(to)) => {
            let layer: Map<String, Value> = to
                .iter()
                .filter_map(|(key, value)| {
                    let diff = match from.get(key) {
                        Some(old) => changed(old, value, lists)?,
                        None => value.clone(),
                    };
                    Some((key.clone(), diff))
                })
                .collect();
            (!layer.is_empty()).then_some(Value::Object(layer))
        }
        (Value::Array(from), Value::Array(to)) if lists == Lists::WhatTheHostAdded => {
            let added: Vec<Value> = to
                .iter()
                .filter(|item| !from.contains(item))
                .cloned()
                .collect();
            (!added.is_empty()).then_some(Value::Array(added))
        }
        _ => (from != to).then(|| to.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::config_source::Tier;
    use base::permission::PermissionRuleEntry;
    use settings::Settings;

    fn loaded(values: Settings) -> LoadedSettings {
        LoadedSettings::new(values, Default::default())
    }

    fn rule(tool: &str) -> LoadedRule {
        LoadedRule::new(
            PermissionRuleEntry {
                tool: tool.into(),
                action: base::permission::RuleBehavior::Allow,
                matching: Default::default(),
            },
            Tier::Project,
            false,
        )
    }

    #[test]
    fn only_what_the_host_changed_is_carried_and_it_lands_on_other_settings() {
        let disk = Settings::defaults_for("m");
        let mut host = disk.clone();
        host.memory.enabled = false;
        host.model.max_tokens = 7;
        let overrides = HostOverrides::between(&loaded(disk.clone()), &loaded(host));
        let layer = Some(
            serde_json::json!({"memory": {"enabled": false}, "model": {"max_tokens": 7}})
                .as_object()
                .cloned()
                .unwrap(),
        );
        assert_eq!(overrides.own, layer);
        assert_eq!(overrides.added, layer);

        let mut other = Settings::defaults_for("other-model");
        other.prompt.language = Some("fr".into());
        let applied = overrides.apply(loaded(other));
        assert!(!applied.values.memory.enabled);
        assert_eq!(applied.values.model.max_tokens, 7);
        assert_eq!(applied.values.model.model_name, "other-model");
        assert_eq!(applied.values.prompt.language.as_deref(), Some("fr"));

        let same = HostOverrides::between(&loaded(disk.clone()), &loaded(disk));
        assert!(same.own.is_none() && same.added.is_none());
    }

    fn provider(id: &str) -> base::provider::ProviderConfig {
        serde_json::from_value(serde_json::json!({
            "id": id, "base_url": "https://x.invalid",
            "tiers": {"strong": "m"},
        }))
        .unwrap()
    }

    #[test]
    fn the_own_projects_providers_stay_on_the_own_pair() {
        let mut disk = Settings::defaults_for("m");
        disk.providers = vec![provider("own-project")];
        let mut disk = loaded(disk);
        disk.provider_origins.insert(
            "own-project".into(),
            settings::ProviderOrigin {
                tier: Tier::Project,
                origin: "/p/.agix/settings.json".into(),
                machine_local: false,
            },
        );
        let mut host = disk.clone();
        host.values.providers.push(provider("host"));
        let overrides = HostOverrides::between(&disk, &host);

        let ids = |settings: &LoadedSettings| -> Vec<String> {
            settings
                .values
                .providers
                .iter()
                .map(|p| p.id.clone())
                .collect()
        };
        let other = overrides.apply(loaded(Settings::defaults_for("m")));
        assert_eq!(ids(&other), ["host"]);
        assert_eq!(ids(&overrides.apply_to_own(disk)), ["own-project", "host"]);
    }

    #[test]
    fn another_pair_keeps_its_own_list_and_gets_what_the_host_added() {
        let mut disk = Settings::defaults_for("m");
        disk.files.extra_dirs = vec!["/own-project/extra".into()];
        let mut host = disk.clone();
        host.files.extra_dirs.push("/host/extra".into());
        let overrides = HostOverrides::between(&loaded(disk), &loaded(host));

        let mut other = Settings::defaults_for("m");
        other.files.extra_dirs = vec!["/other/extra".into()];
        let applied = overrides.apply(loaded(other));
        assert_eq!(
            applied.values.files.extra_dirs,
            [
                std::path::PathBuf::from("/other/extra"),
                std::path::PathBuf::from("/host/extra"),
            ]
        );
    }

    #[test]
    fn the_rules_the_host_added_land_only_on_its_own_pair() {
        let mut disk = loaded(Settings::defaults_for("m"));
        disk.rules = vec![rule("Read")];
        let mut host = disk.clone();
        host.rules.push(rule("Bash(touch ruled.txt)"));
        let overrides = HostOverrides::between(&disk, &host);

        let tools = |settings: &LoadedSettings| -> Vec<String> {
            settings.rules.iter().map(|r| r.rule.tool.clone()).collect()
        };
        assert_eq!(
            tools(&overrides.apply_to_own(disk.clone())),
            ["Read", "Bash(touch ruled.txt)"]
        );
        assert_eq!(tools(&overrides.apply(disk)), ["Read"]);
    }
}
