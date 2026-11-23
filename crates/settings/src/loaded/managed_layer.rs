//! The deployment's managed layer over loaded settings: read out of a
//! source's layers before the rest, deciding project trust with them, and
//! laid over every value the settings hold — by the loader, and again after
//! every overlay.

use base::interface::config_source::{ConfigLayer, Tier};
use base::permission::{ManagedSettings, PermissionMode};
use serde_json::{Map, Value};

use super::{mcp_servers_declared, read_providers, LoadedSettings, ProviderOrigin, PROVIDERS};
use crate::merge::merge_fitting;
use crate::overlay::value_at;
use crate::section::Sections;
use crate::Settings;

/// The `Tier::Managed` layers of `layers` read as one managed layer, and the
/// rest.
pub(super) fn split_off(
    layers: Vec<ConfigLayer>,
    sections: &Sections,
) -> (Option<ManagedSettings>, Vec<ConfigLayer>) {
    let (managed, rest): (Vec<_>, Vec<_>) = layers
        .into_iter()
        .partition(|layer| layer.tier == Tier::Managed);
    let managed = managed
        .into_iter()
        .map(|layer| crate::managed::from_value(layer.origin, layer.value, sections))
        .reduce(crate::managed::followed_by);
    (managed, rest)
}

pub(super) use crate::managed::trust_under;

impl LoadedSettings {
    /// Put the deployment's layer on top of these settings, replacing any
    /// managed layer they already carry: every value it sets over theirs,
    /// except one the trust table orders as looser than theirs (a tighter
    /// budget or a stricter sandbox below it stands), its providers ahead of
    /// theirs. With bypass disabled, a `bypassPermissions`
    /// the lower tiers configured becomes `default`. A layer that cannot be
    /// honoured sets nothing; the permission gate refuses every call under it.
    pub fn with_managed(mut self, managed: Option<ManagedSettings>, sections: &Sections) -> Self {
        if let Some(m) = managed.as_ref().filter(|m| m.error.is_none()) {
            self.lay_managed_values(m, sections);
            if m.disable_bypass && self.values.permissions.mode == PermissionMode::BypassPermissions
            {
                tracing::warn!(origin = %m.origin, "managed settings disable bypassPermissions; permissions.mode falls back to default");
                self.values.permissions.mode = PermissionMode::Default;
            }
        }
        self.managed = managed;
        self
    }

    /// The `prompt.append` of a managed layer that can be honoured.
    pub fn managed_prompt_append(&self) -> Option<&str> {
        self.managed
            .as_ref()
            .filter(|m| m.error.is_none())?
            .values
            .get("prompt")?
            .get("append")?
            .as_str()
    }

    /// `mode` held to the managed layer: no wider than the `permissions.mode`
    /// it sets, and `default` for a `bypassPermissions` it disables.
    pub fn hold_mode_to_managed(&self, mode: PermissionMode) -> PermissionMode {
        let Some(managed) = self.managed.as_ref().filter(|m| m.error.is_none()) else {
            return mode;
        };
        let ceiling = managed
            .values
            .get(crate::permissions::KEY)
            .and_then(|block| block.get("mode"))
            .and_then(|mode| serde_json::from_value::<PermissionMode>(mode.clone()).ok());
        let mode = ceiling.map_or(mode, |ceiling| mode.min_permissive(ceiling));
        if managed.disable_bypass && mode == PermissionMode::BypassPermissions {
            return PermissionMode::Default;
        }
        mode
    }

    /// What the managed layer sets at `keys` — dotted, of the core settings —
    /// laid over these settings again, as [`with_managed`](Self::with_managed)
    /// lays it, after something other than a settings layer set them.
    pub(crate) fn hold_to_managed(&mut self, keys: &[&str]) {
        let Some(managed) = self.managed.as_ref().filter(|m| m.error.is_none()) else {
            return;
        };
        let all = Value::Object(managed.values.clone());
        let mut values = Map::new();
        for key in keys {
            let value = value_at(&all, key);
            if !value.is_null() {
                set_dotted(&mut values, key, value);
            }
        }
        let mut part = managed.clone();
        part.values = values;
        self.lay_managed_values(&part, &Sections::none());
    }

    fn lay_managed_values(&mut self, managed: &ManagedSettings, sections: &Sections) {
        if managed.values.is_empty() {
            return;
        }
        let mut layer = managed.values.clone();
        let mut merged = serde_json::to_value(&self.values).unwrap_or_default();
        if let Some(providers) = layer.remove(PROVIDERS) {
            for (_, provider) in read_providers(providers.clone()).0 {
                if let Some(env) = &provider.api_key_env {
                    base::interface::exec::env::hold_back_credential(env);
                }
                self.provider_origins.insert(
                    provider.id,
                    ProviderOrigin {
                        tier: Tier::Managed,
                        origin: managed.origin.clone(),
                        machine_local: false,
                    },
                );
            }
            merged[PROVIDERS] = providers_first(&providers, &merged);
        }
        crate::tier_gate::tighten_layer(&mut layer, &merged, sections);
        for (name, server) in mcp_servers_declared(&layer) {
            if server.get("command").is_some() {
                self.mcp_server_dirs
                    .insert(name.clone(), self.paths.global_data_dir.clone());
            }
            for var in base::interface::exec::env::variables_an_mcp_server_reads(&server) {
                base::interface::exec::env::hold_back_credential(&var);
            }
        }
        for (key, error) in merge_fitting(&mut merged, Value::Object(layer), sections) {
            tracing::warn!(origin = %managed.origin, key, error, "managed settings value does not fit; ignoring it");
        }
        match serde_json::from_value::<Settings>(merged) {
            Ok(values) => {
                let before = std::mem::replace(&mut self.values, values);
                self.settle_providers();
                if let Err(e) = self.place_worktrees() {
                    tracing::warn!(origin = %managed.origin, error = %e, "managed app.worktree_dir is not allowed, the worktrees stay where they were");
                    self.values.app.worktree_dir = before.app.worktree_dir;
                }
            }
            Err(e) => {
                tracing::warn!(origin = %managed.origin, error = %e, "managed settings do not read over these settings; they are left as they were");
            }
        }
    }
}

fn set_dotted(map: &mut Map<String, Value>, dotted: &str, value: Value) {
    match dotted.split_once('.') {
        None => {
            map.insert(dotted.to_string(), value);
        }
        Some((head, rest)) => {
            if let Value::Object(inner) =
                map.entry(head).or_insert_with(|| Value::Object(Map::new()))
            {
                set_dotted(inner, rest, value);
            }
        }
    }
}

/// `layer`'s providers followed by those of `lower` with an id they do not
/// have: the higher layer's entry for an id is the one that stands.
pub(crate) fn providers_first(layer: &Value, lower: &Value) -> Value {
    let first = layer.as_array().cloned().unwrap_or_default();
    let ids: Vec<&Value> = first.iter().filter_map(|p| p.get("id")).collect();
    let rest = lower
        .get(PROVIDERS)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| p.get("id").is_none_or(|id| !ids.contains(&id)))
        .cloned();
    Value::Array(first.iter().cloned().chain(rest).collect())
}
