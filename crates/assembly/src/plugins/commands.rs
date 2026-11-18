//! The commands the active packages declare (`[[command]]`), as sessions
//! get them and as `plugin.list` reports them.

use crate::session_pool::api::PluginCommand;
use base::command::ExtCommand;
use packages::manifest::Plugin;
use std::collections::HashMap;

/// Every command of `active`, in package-name order. A name two packages
/// declare goes to the first; the other's is in [`clashes`].
pub(super) fn kept(active: &[Plugin]) -> Vec<ExtCommand> {
    let mut seen = std::collections::HashSet::new();
    declared(active)
        .into_iter()
        .filter(|c| seen.insert(c.name.clone()))
        .collect()
}

/// `(command, package kept, package refused)` for each command name a later
/// package declared after an earlier one had it.
pub(super) fn clashes(active: &[Plugin]) -> Vec<(String, String, String)> {
    let mut owners: HashMap<String, String> = HashMap::new();
    let mut clashes = Vec::new();
    for c in declared(active) {
        match owners.get(&c.name) {
            Some(kept) => clashes.push((c.name, kept.clone(), c.plugin)),
            None => {
                owners.insert(c.name, c.plugin);
            }
        }
    }
    clashes
}

/// What `plugin.list` shows of one package's commands.
pub(super) fn listed(plugin: &Plugin) -> Vec<PluginCommand> {
    packages::commands::declared_commands(plugin)
        .map(|commands| {
            commands
                .into_iter()
                .map(|c| PluginCommand {
                    name: c.name,
                    description: c.description,
                    read_only: c.read_only,
                    writes: c.writes,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Discovery already checked these entries; a package whose entry no longer
/// passes loses its commands here, not the others'.
fn declared(active: &[Plugin]) -> Vec<ExtCommand> {
    active
        .iter()
        .flat_map(|p| match packages::commands::declared_commands(p) {
            Ok(commands) => commands,
            Err(e) => {
                tracing::warn!(
                    plugin = %p.name(),
                    error = %e,
                    "package commands no longer load; skipping them"
                );
                Vec::new()
            }
        })
        .collect()
}
