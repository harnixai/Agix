//! 宿主在内存里改过的设置：宿主拿到守护进程自己那一对的设置之后、交给池之前改掉的值，
//! 作为一层叠在池从盘上读入的每份设置上，和托管层在同一个位置；宿主加的权限规则只回到守护进程自己那一对上。

use base::permission::LoadedRule;
use serde_json::Value;
use settings::{LoadedSettings, Settings};

/// What the host's settings hold that the same pair read from disk does not:
/// the values it set in memory, as a JSON layer, and the rules it added.
#[derive(Debug, Default, Clone)]
pub(in crate::session_pool) struct HostOverrides {
    values: Option<Value>,
    rules: Vec<LoadedRule>,
}

impl HostOverrides {
    /// What `host` sets differently from `disk`. A key the host removed from a
    /// map, or a rule it removed, is not carried: a layer can only add or
    /// replace.
    pub(in crate::session_pool) fn between(disk: &LoadedSettings, host: &LoadedSettings) -> Self {
        let values = match (
            serde_json::to_value(&disk.values),
            serde_json::to_value(&host.values),
        ) {
            (Ok(disk), Ok(host)) => changed(&disk, &host),
            _ => None,
        };
        let on_disk: Vec<_> = disk.rules.iter().map(rule_key).collect();
        let rules = host
            .rules
            .iter()
            .filter(|rule| !on_disk.contains(&rule_key(rule)))
            .cloned()
            .collect();
        Self { values, rules }
    }

    /// `settings` with the values this layer holds merged over its own.
    pub(in crate::session_pool) fn apply(&self, mut settings: LoadedSettings) -> LoadedSettings {
        let Some(layer) = &self.values else {
            return settings;
        };
        let Ok(mut merged) = serde_json::to_value(&settings.values) else {
            return settings;
        };
        let providers = with_the_hosts_providers(&merged, layer);
        settings::merge_json_values(&mut merged, layer.clone());
        if let Some(providers) = providers {
            merged["providers"] = providers;
        }
        match serde_json::from_value::<Settings>(merged) {
            Ok(values) => {
                let disk = std::mem::replace(&mut settings.values, values);
                settings.settle_providers();
                if let Err(e) = settings.place_worktrees() {
                    tracing::warn!(error = %e, "the host's app.worktree_dir is not allowed; left out");
                    settings.values.app.worktree_dir = disk.app.worktree_dir;
                }
            }
            Err(e) => tracing::warn!(error = %e, "the host's settings do not apply here; left out"),
        }
        settings
    }

    /// [`Self::apply`], and the rules the host added after it: what the host
    /// made of this daemon's own pair, on that pair read from disk.
    pub(in crate::session_pool) fn apply_to_own(&self, settings: LoadedSettings) -> LoadedSettings {
        let mut settings = self.apply(settings);
        settings.rules.extend(self.rules.iter().cloned());
        settings
    }
}

/// The host's providers followed by those of `merged` with an id the host's
/// do not have, when the host set any: the list adds up as the tiers' do.
fn with_the_hosts_providers(merged: &Value, layer: &Value) -> Option<Value> {
    let hosts = layer.get("providers")?.as_array()?;
    let ids: Vec<&Value> = hosts.iter().filter_map(|p| p.get("id")).collect();
    let rest = merged
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| p.get("id").is_none_or(|id| !ids.contains(&id)));
    Some(Value::Array(hosts.iter().chain(rest).cloned().collect()))
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

fn changed(from: &Value, to: &Value) -> Option<Value> {
    match (from, to) {
        (Value::Object(from), Value::Object(to)) => {
            let layer: serde_json::Map<String, Value> = to
                .iter()
                .filter_map(|(key, value)| {
                    let diff = match from.get(key) {
                        Some(old) => changed(old, value)?,
                        None => value.clone(),
                    };
                    Some((key.clone(), diff))
                })
                .collect();
            (!layer.is_empty()).then_some(Value::Object(layer))
        }
        _ => (from != to).then(|| to.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::config_source::Tier;
    use base::permission::PermissionRuleEntry;

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
        assert_eq!(
            overrides.values,
            Some(serde_json::json!({"memory": {"enabled": false}, "model": {"max_tokens": 7}}))
        );

        let mut other = Settings::defaults_for("other-model");
        other.prompt.language = Some("fr".into());
        let applied = overrides.apply(loaded(other));
        assert!(!applied.values.memory.enabled);
        assert_eq!(applied.values.model.max_tokens, 7);
        assert_eq!(applied.values.model.model_name, "other-model");
        assert_eq!(applied.values.prompt.language.as_deref(), Some("fr"));

        let same = HostOverrides::between(&loaded(disk.clone()), &loaded(disk));
        assert!(same.values.is_none());
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
