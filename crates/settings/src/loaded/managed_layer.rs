//! The deployment's managed layer over loaded settings: read out of a
//! source's layers before the rest, deciding project trust with them, and
//! laid over every value the settings hold — by the loader, and again after
//! every overlay.

use base::interface::config_source::{ConfigLayer, Tier};
use base::permission::{ManagedSettings, PermissionMode};
use serde_json::Value;

use super::{mcp_servers_declared, read_providers, LoadedSettings, ProviderOrigin, PROVIDERS};
use crate::merge::merge_fitting;
use crate::section::Sections;
use crate::trust::ProjectTrustSettings;
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

/// What the global tier says about trust, with what a managed layer that can
/// be honoured says in its place.
pub(super) fn trust_under(
    global: ProjectTrustSettings,
    managed: Option<&ManagedSettings>,
) -> ProjectTrustSettings {
    match managed
        .filter(|m| m.error.is_none())
        .and_then(|m| m.values.get("app"))
    {
        Some(app) => global.decided_by(app),
        None => global,
    }
}

impl LoadedSettings {
    /// Put the deployment's layer on top of these settings, replacing any
    /// managed layer they already carry: every value it sets over theirs, its
    /// providers ahead of theirs. With bypass disabled, a `bypassPermissions`
    /// the lower tiers configured becomes `default`. A layer that cannot be
    /// honoured sets nothing; the permission gate refuses every call under it.
    pub fn with_managed(mut self, managed: Option<ManagedSettings>) -> Self {
        if let Some(m) = managed.as_ref().filter(|m| m.error.is_none()) {
            self.lay_managed_values(m);
            if m.disable_bypass && self.values.permissions.mode == PermissionMode::BypassPermissions
            {
                tracing::warn!(origin = %m.origin, "managed settings disable bypassPermissions; permissions.mode falls back to default");
                self.values.permissions.mode = PermissionMode::Default;
            }
        }
        self.managed = managed;
        self
    }

    fn lay_managed_values(&mut self, managed: &ManagedSettings) {
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
        for (name, server) in mcp_servers_declared(&layer) {
            if server.get("command").is_some() {
                self.mcp_server_dirs
                    .insert(name.clone(), self.paths.global_data_dir.clone());
            }
            for var in base::interface::exec::env::variables_an_mcp_server_reads(&server) {
                base::interface::exec::env::hold_back_credential(&var);
            }
        }
        // Every value was checked against every section when the layer was
        // read; here they only have to deserialize together.
        for (key, error) in merge_fitting(&mut merged, Value::Object(layer), &Sections::none()) {
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
