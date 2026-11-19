//! The functions the active packages expose to the host (`[[function]]`),
//! as a host call finds them and as `plugin.list` reports them. Each is
//! called by `<plugin>.<name>`, so two packages never clash.

use crate::session_pool::api::PluginFunctionEntry;
use base::command::PluginFunction;
use packages::manifest::Plugin;

/// Every function of `active`, in package-name order. Discovery already
/// checked them; a package whose entry or schema no longer passes loses its
/// functions here, not the others'.
pub(super) fn active(active: &[Plugin]) -> Vec<PluginFunction> {
    active
        .iter()
        .flat_map(|p| match packages::functions::declared_functions(p) {
            Ok(functions) => functions,
            Err(e) => {
                tracing::warn!(
                    plugin = %p.name(),
                    error = %e,
                    "package functions no longer load; skipping them"
                );
                Vec::new()
            }
        })
        .collect()
}

pub(super) fn listed(plugin: &Plugin) -> Vec<PluginFunctionEntry> {
    packages::functions::declared_functions(plugin)
        .map(|functions| {
            functions
                .into_iter()
                .map(|f| PluginFunctionEntry {
                    name: f.name,
                    description: f.description,
                    writes: f.writes,
                    has_input_schema: f.input_schema.is_some(),
                })
                .collect()
        })
        .unwrap_or_default()
}
