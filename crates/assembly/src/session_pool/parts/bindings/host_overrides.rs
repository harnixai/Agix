//! 宿主在内存里改过的设置：宿主拿到守护进程自己那一对的设置之后、交给池之前改掉的值，
//! 作为一层叠在池从盘上读入的每份设置上，和托管层在同一个位置。守护进程自己那一对拿到宿主改的全部，
//! 但列表跟着盘：宿主没加的元素盘上没了就不再有（撤销信任后项目自己的 provider 不回来），盘上后加的照样有；
//! 别的对只拿宿主新加的——列表里只有它新加的元素，守护进程自己项目层里来的不跨对（宿主改过的项目
//! provider 也不跨）；宿主加的权限
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
    /// The entries the host took out of a list.
    removed: Option<Map<String, Value>>,
    /// The ids of the providers this daemon's own project defined.
    project_providers: Vec<String>,
    rules: Vec<LoadedRule>,
}

impl HostOverrides {
    /// What `host` sets differently from `disk`. A key the host removed from a
    /// map, or a rule it removed, is not carried: a layer can only add or
    /// replace.
    pub(in crate::session_pool) fn between(disk: &LoadedSettings, host: &LoadedSettings) -> Self {
        let (own, added, removed) = match (
            serde_json::to_value(&disk.values),
            serde_json::to_value(&host.values),
        ) {
            (Ok(disk), Ok(host)) => (
                changed(&disk, &host, Lists::AsTheHostHasThem, None),
                changed(&disk, &host, Lists::WhatTheHostAdded, None),
                changed(&host, &disk, Lists::WhatTheHostAdded, None).and_then(only_lists),
            ),
            _ => (None, None, None),
        };
        let on_disk: Vec<_> = disk.rules.iter().map(rule_key).collect();
        let rules = host
            .rules
            .iter()
            .filter(|rule| !on_disk.contains(&rule_key(rule)))
            .cloned()
            .collect();
        let project_providers = disk
            .provider_origins
            .iter()
            .filter(|(_, origin)| origin.tier == base::interface::config_source::Tier::Project)
            .map(|(id, _)| id.clone())
            .collect();
        Self {
            own: own.and_then(into_object),
            added: added.and_then(into_object),
            removed: removed.and_then(into_object),
            project_providers,
            rules,
        }
    }

    /// `settings` of a pair other than this daemon's own, with what the host
    /// added: a list is the pair's own followed by what the host added to
    /// it.
    pub(in crate::session_pool) fn apply(&self, settings: LoadedSettings) -> LoadedSettings {
        match self.layer_for_other(&settings) {
            Some(layer) => overlay(&settings, &layer),
            None => settings,
        }
    }

    /// This daemon's own pair, read from disk, as the host made it: every
    /// value it set, and the rules it added. A list is the host's, less what
    /// it did not add and the disk no longer has, then what the disk has that
    /// the host neither had nor took out.
    pub(in crate::session_pool) fn apply_to_own(&self, settings: LoadedSettings) -> LoadedSettings {
        let mut settings = match self.layer_for_own(&settings) {
            Some(layer) => overlay(&settings, &layer),
            None => settings,
        };
        settings.rules.extend(self.rules.iter().cloned());
        settings
    }

    /// The layer [`apply_to_own`](Self::apply_to_own) (`own`) or
    /// [`apply`](Self::apply) lays over `settings`, without the rules.
    pub(in crate::session_pool) fn layer_over(
        &self,
        settings: &LoadedSettings,
        own: bool,
    ) -> Option<Map<String, Value>> {
        if own {
            self.layer_for_own(settings)
        } else {
            self.layer_for_other(settings)
        }
    }

    fn layer_for_other(&self, settings: &LoadedSettings) -> Option<Map<String, Value>> {
        let mut layer = self.added.clone()?;
        self.drop_project_providers(&mut layer, &[]);
        if let Ok(Value::Object(theirs)) = serde_json::to_value(&settings.values) {
            extend_lists(&mut layer, &theirs, None);
        }
        Some(layer)
    }

    fn layer_for_own(&self, settings: &LoadedSettings) -> Option<Map<String, Value>> {
        let mut layer = self.own.clone()?;
        if let Ok(Value::Object(now)) = serde_json::to_value(&settings.values) {
            follow_disk(
                &mut layer,
                &now,
                self.added.as_ref(),
                self.removed.as_ref(),
                None,
            );
            let on_disk = now.get("providers").and_then(Value::as_array);
            self.drop_project_providers(&mut layer, on_disk.map_or(&[], Vec::as_slice));
        }
        Some(layer)
    }
}

impl HostOverrides {
    /// `layer` without the providers of this daemon's own project, but those
    /// still `on_disk`.
    fn drop_project_providers(&self, layer: &mut Map<String, Value>, on_disk: &[Value]) {
        let Some(Value::Array(providers)) = layer.get_mut("providers") else {
            return;
        };
        let id_of = |p: &Value| p.get("id").and_then(Value::as_str).map(str::to_string);
        providers.retain(|p| {
            id_of(p).is_none_or(|id| {
                !self.project_providers.contains(&id)
                    || on_disk.iter().any(|d| id_of(d).as_ref() == Some(&id))
            })
        });
        if providers.is_empty() {
            layer.remove("providers");
        }
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
fn extend_lists(layer: &mut Map<String, Value>, theirs: &Map<String, Value>, at: Option<&str>) {
    for (key, value) in layer.iter_mut() {
        match (value, theirs.get(key)) {
            (Value::Object(inner), Some(Value::Object(below))) => {
                extend_lists(inner, below, Some(key))
            }
            (Value::Array(added), Some(Value::Array(below)))
                if key != "providers" && !is_whole(at, key) =>
            {
                let mut list = below.clone();
                list.extend(added.drain(..).filter(|item| !below.contains(item)));
                *added = list;
            }
            _ => {}
        }
    }
}

/// Each list of `layer` as [`HostOverrides::apply_to_own`] lays it over
/// `now`; `added` and `removed` are the host's lists of the same shape.
fn follow_disk(
    layer: &mut Map<String, Value>,
    now: &Map<String, Value>,
    added: Option<&Map<String, Value>>,
    removed: Option<&Map<String, Value>>,
    at: Option<&str>,
) {
    for (key, value) in layer.iter_mut() {
        let added_here = added.and_then(|a| a.get(key));
        let removed_here = removed.and_then(|r| r.get(key));
        match (value, now.get(key)) {
            (Value::Object(inner), Some(Value::Object(below))) => follow_disk(
                inner,
                below,
                added_here.and_then(Value::as_object),
                removed_here.and_then(Value::as_object),
                Some(key),
            ),
            (Value::Array(host), below) if !is_whole(at, key) => {
                let below = below.and_then(Value::as_array).cloned().unwrap_or_default();
                let added = added_here
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let removed = removed_here
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                *host = if key == "providers" {
                    providers_on_disk(host, &below, &added)
                } else {
                    let mut list: Vec<Value> = host
                        .iter()
                        .filter(|item| added.contains(item) || below.contains(item))
                        .cloned()
                        .collect();
                    let later: Vec<Value> = below
                        .iter()
                        .filter(|item| !host.contains(item) && !removed.contains(item))
                        .cloned()
                        .collect();
                    list.extend(later);
                    list
                };
            }
            _ => {}
        }
    }
}

/// The host's providers in its order: one it added as it has it, else the
/// disk's entry of that id while there is one.
fn providers_on_disk(host: &[Value], below: &[Value], added: &[Value]) -> Vec<Value> {
    host.iter()
        .filter_map(|entry| {
            if added.contains(entry) {
                return Some(entry.clone());
            }
            let id = entry.get("id")?;
            below.iter().find(|p| p.get("id") == Some(id)).cloned()
        })
        .collect()
}

/// `value` with only its lists, and the objects that lead to one.
fn only_lists(value: Value) -> Option<Value> {
    match value {
        Value::Object(fields) => {
            let kept: Map<String, Value> = fields
                .into_iter()
                .filter_map(|(key, inner)| Some((key, only_lists(inner)?)))
                .collect();
            (!kept.is_empty()).then_some(Value::Object(kept))
        }
        Value::Array(items) => Some(Value::Array(items)),
        _ => None,
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

/// `prompt.append` is one value however it is written: a list of blocks is
/// replaced whole, never merged entry by entry with the one below.
fn is_whole(at: Option<&str>, key: &str) -> bool {
    at == Some("prompt") && key == "append"
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lists {
    AsTheHostHasThem,
    WhatTheHostAdded,
}

fn changed(from: &Value, to: &Value, lists: Lists, at: Option<&str>) -> Option<Value> {
    match (from, to) {
        (Value::Object(from), Value::Object(to)) => {
            let layer: Map<String, Value> = to
                .iter()
                .filter_map(|(key, value)| {
                    let diff = match from.get(key) {
                        Some(old) if is_whole(at, key) => (old != value).then(|| value.clone())?,
                        Some(old) => changed(old, value, lists, Some(key))?,
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
        assert_eq!(applied.main_model, "other-model");
        assert_eq!(applied.values.prompt.language.as_deref(), Some("fr"));

        let same = HostOverrides::between(&loaded(disk.clone()), &loaded(disk));
        assert!(same.own.is_none() && same.added.is_none());
    }

    fn with_append(mut values: Settings, blocks: serde_json::Value) -> Settings {
        values.prompt.append = Some(serde_json::from_value(blocks).unwrap());
        values
    }

    fn append_of(settings: &LoadedSettings) -> serde_json::Value {
        serde_json::to_value(&settings.values.prompt.append).unwrap()
    }

    #[test]
    fn a_listed_append_the_host_set_replaces_the_one_below_whole() {
        let block = |name: &str| serde_json::json!({"name": name, "text": name});
        let disk = with_append(Settings::defaults_for("m"), serde_json::json!([block("a")]));
        let host = with_append(disk.clone(), serde_json::json!([block("h")]));
        let overrides = HostOverrides::between(&loaded(disk.clone()), &loaded(host));

        let disk_now = with_append(disk, serde_json::json!([block("a"), block("later")]));
        let own = overrides.apply_to_own(loaded(disk_now));
        assert_eq!(append_of(&own), serde_json::json!([block("h")]));

        let other = with_append(
            Settings::defaults_for("other"),
            serde_json::json!([block("theirs")]),
        );
        let applied = overrides.apply(loaded(other));
        assert_eq!(append_of(&applied), serde_json::json!([block("h")]));
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
    fn a_provider_of_the_own_project_the_host_changed_stays_on_the_own_pair() {
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
        host.values.providers[0].quota.day = Some(5);
        let overrides = HostOverrides::between(&disk, &host);

        let other = overrides.apply(loaded(Settings::defaults_for("m")));
        assert!(
            other.values.providers.iter().all(|p| p.id != "own-project"),
            "{:?}",
            other.values.providers
        );
        let own = overrides.apply_to_own(disk);
        assert_eq!(own.values.providers[0].quota.day, Some(5));
        let untrusted_now = overrides.apply_to_own(loaded(Settings::defaults_for("m")));
        assert!(
            untrusted_now
                .values
                .providers
                .iter()
                .all(|p| p.id != "own-project"),
            "{:?}",
            untrusted_now.values.providers
        );
    }

    #[test]
    fn what_left_the_own_projects_files_does_not_come_back_with_the_host_layer() {
        let mut disk = Settings::defaults_for("m");
        disk.providers = vec![provider("own-project")];
        disk.files.extra_dirs = vec!["/own-project/extra".into()];
        disk.files.deny_write = vec!["secret".into(), "keep".into()];
        let disk = loaded(disk);
        let mut host = disk.clone();
        host.values.providers.push(provider("host"));
        host.values.files.extra_dirs.push("/host/extra".into());
        host.values.files.deny_write = vec!["keep".into()];
        let overrides = HostOverrides::between(&disk, &host);

        let mut untrusted_now = Settings::defaults_for("m");
        untrusted_now.files.deny_write = vec!["secret".into(), "keep".into(), "added-later".into()];
        let own = overrides.apply_to_own(loaded(untrusted_now));
        let ids: Vec<&str> = own.values.providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["host"]);
        assert_eq!(
            own.values.files.extra_dirs,
            [std::path::PathBuf::from("/host/extra")]
        );
        assert_eq!(own.values.files.deny_write, ["keep", "added-later"]);
    }

    #[test]
    fn the_host_keeps_its_order_of_the_providers_still_on_disk() {
        let mut disk = Settings::defaults_for("m");
        disk.providers = vec![provider("a"), provider("b")];
        let disk = loaded(disk);
        let mut host = disk.clone();
        host.values.providers = vec![provider("host"), provider("b"), provider("a")];
        let overrides = HostOverrides::between(&disk, &host);
        let own = overrides.apply_to_own(disk);
        let ids: Vec<&str> = own.values.providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["host", "b", "a"]);
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
