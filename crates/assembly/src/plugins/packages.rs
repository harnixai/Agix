//! One pool's package state: the policy it started with, the libraries its
//! host provides, the host's own packages, and the active set every reader
//! shares.

use super::{ActivePackage, DaemonPaths, PackageScripts, PackageSkills, PluginsConfig};
use crate::session_pool::api::{PluginInfo, PluginOrigin};
use ::packages::manifest::Plugin;
use base::command::HostLibrary;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub struct Packages {
    paths: Arc<dyn DaemonPaths>,
    /// The `settings.plugins` policy this pool started with. Held rather
    /// than re-read, so a refresh mid-session cannot quietly widen what
    /// is allowed to load.
    policy: PluginsConfig,
    enabled: bool,
    libraries: Vec<HostLibrary>,
    /// The host's own packages, in name order. Each is loaded in place of an
    /// installed package of its name.
    host: RwLock<Arc<Vec<Plugin>>>,
    /// The permitted, enabled set. Rescanned by install/uninstall/enable/
    /// disable, so every reader — the MCP configs, the script bindings,
    /// the scenes, the agent types — is looking at one answer rather than each
    /// walking the directory at a different moment.
    active: RwLock<Arc<Vec<Plugin>>>,
    /// Why a package's script binding could not be honored, by package.
    /// A fault here costs that binding and nothing else, so it has to be
    /// reported somewhere the person who installed the package will look.
    script_faults: RwLock<HashMap<String, Vec<String>>>,
}

impl Packages {
    pub fn new(
        paths: Arc<dyn DaemonPaths>,
        policy: PluginsConfig,
        libraries: Vec<HostLibrary>,
    ) -> Self {
        let enabled = policy.enabled;
        let active = Arc::new(if enabled {
            active_plugins(paths.as_ref(), &policy, &[], &libraries)
        } else {
            Vec::new()
        });
        Self {
            paths,
            policy,
            enabled,
            libraries,
            host: RwLock::new(Arc::new(Vec::new())),
            active: RwLock::new(active),
            script_faults: RwLock::new(HashMap::new()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn policy(&self) -> &PluginsConfig {
        &self.policy
    }

    pub fn layout(&self) -> base::paths::Layout {
        self.paths.layout()
    }

    pub fn libraries(&self) -> &[HostLibrary] {
        &self.libraries
    }

    pub fn active(&self) -> Arc<Vec<Plugin>> {
        self.active
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn host(&self) -> Arc<Vec<Plugin>> {
        self.host.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn rescan(&self) {
        if !self.enabled {
            return;
        }
        let refreshed = Arc::new(active_plugins(
            self.paths.as_ref(),
            &self.policy,
            &self.host(),
            &self.libraries,
        ));
        *self.active.write().unwrap_or_else(|e| e.into_inner()) = refreshed;
    }

    /// Take up the package at `dir` as one of the host's own, in place of an
    /// earlier one of its name. Returns its name.
    pub fn register_host(&self, dir: &Path) -> Result<String, String> {
        let bytecode = self
            .tier_dirs()
            .0
            .join(base::paths::entry::CACHE)
            .join(".bytecode");
        let (plugin, _) =
            ::packages::host::load_host_package(dir, &self.layout(), &self.libraries, bytecode)
                .map_err(|e| format!("host package {}: {e}", dir.display()))?;
        let name = plugin.name().to_string();
        {
            let mut host = self.host.write().unwrap_or_else(|e| e.into_inner());
            let mut packages: Vec<Plugin> =
                host.iter().filter(|p| p.name() != name).cloned().collect();
            packages.push(plugin);
            packages.sort_by(|a, b| a.name().cmp(b.name()));
            *host = Arc::new(packages);
        }
        self.rescan();
        Ok(name)
    }

    pub fn is_host(&self, name: &str) -> bool {
        self.host().iter().any(|p| p.name() == name)
    }

    pub fn count(&self) -> usize {
        self.active().len()
    }

    /// Every active package's declared MCP servers by key. A key two
    /// packages both produce goes to the one met first; the other's is in
    /// [`Self::mcp_key_clashes`].
    pub fn mcp_servers(&self) -> HashMap<String, serde_json::Value> {
        let mut out = HashMap::new();
        for (_, key, config, _) in self.keyed_mcp_servers() {
            out.entry(key).or_insert(config);
        }
        out
    }

    /// The package root each of [`Self::mcp_servers`] comes from.
    pub fn mcp_server_dirs(&self) -> HashMap<String, PathBuf> {
        let mut out = HashMap::new();
        for (_, key, _, root) in self.keyed_mcp_servers() {
            out.entry(key).or_insert(root);
        }
        out
    }

    /// `(key, package kept, package refused)` for each MCP server key a
    /// later package produced after an earlier one had it.
    pub fn mcp_key_clashes(&self) -> Vec<(String, String, String)> {
        let mut owners: HashMap<String, String> = HashMap::new();
        let mut clashes = Vec::new();
        for (plugin, key, _, _) in self.keyed_mcp_servers() {
            match owners.get(&key) {
                Some(kept) => clashes.push((key, kept.clone(), plugin)),
                None => {
                    owners.insert(key, plugin);
                }
            }
        }
        clashes
    }

    fn keyed_mcp_servers(&self) -> Vec<(String, String, serde_json::Value, PathBuf)> {
        self.active()
            .iter()
            .flat_map(|p| {
                ::packages::mcp::servers_for(p)
                    .into_iter()
                    .map(|(key, config)| (p.name().to_string(), key, config, p.root.clone()))
            })
            .collect()
    }

    /// See [`super::commands::kept`].
    pub fn commands(&self) -> Vec<base::command::ExtCommand> {
        super::commands::kept(self.active().iter())
    }

    /// [`Self::commands`] of the packages `permits` takes, judged before a
    /// name two packages declare goes to the first.
    pub fn commands_of(&self, permits: impl Fn(&str) -> bool) -> Vec<base::command::ExtCommand> {
        super::commands::kept(self.active().iter().filter(|p| permits(p.name())))
    }

    /// The active set as a session's set is drawn from it: each package with
    /// those of its dependencies the set holds at a version it takes.
    pub fn active_packages(&self) -> Vec<ActivePackage> {
        let active = self.active();
        active
            .iter()
            .map(|p| ActivePackage {
                name: p.name().to_string(),
                version: p.manifest.plugin.version.clone(),
                dependencies: p
                    .manifest
                    .dependencies
                    .iter()
                    .filter(|(name, required)| {
                        let found = active
                            .iter()
                            .filter(|q| q.name() == name.as_str())
                            .map(|q| q.manifest.plugin.version.clone())
                            .collect::<Vec<_>>();
                        let met = ::packages::manifest::check_dependency(name, required, &found);
                        if let Err(e) = &met {
                            tracing::debug!(
                                plugin = %p.name(),
                                error = %e,
                                "a dependency is not in the active set; a session naming the \
                                 package does not get it"
                            );
                        }
                        met.is_ok()
                    })
                    .map(|(name, _)| name.clone())
                    .collect(),
            })
            .collect()
    }

    pub fn command_clashes(&self) -> Vec<(String, String, String)> {
        super::commands::clashes(&self.active())
    }

    /// See [`super::functions::active`].
    pub fn functions(&self) -> Vec<base::command::PluginFunction> {
        super::functions::active(&self.active())
    }

    /// Every active package's `[[script]]` declarations, as bindings the
    /// carrier can take.
    pub fn script_bindings(&self) -> Vec<PackageScripts> {
        self.active()
            .iter()
            .filter(|p| !p.manifest.script.is_empty())
            .map(|p| PackageScripts {
                name: p.name().to_string(),
                root: p.root.clone(),
                bindings: p
                    .manifest
                    .script
                    .iter()
                    .map(|s| base::interface::script::ScriptBinding {
                        path: PathBuf::from(s.file()),
                        point: s.point.clone(),
                        entry: s.function().to_string(),
                        timeout_ms: s.timeout_ms,
                        calls_per_turn: s.calls_per_turn,
                    })
                    .collect(),
            })
            .collect()
    }

    pub fn agent_types(&self) -> Vec<base::agent_type::AgentTypeDefinition> {
        self.active()
            .iter()
            .flat_map(::packages::agents::agent_types)
            .collect()
    }

    /// Discovery already read these skills; a package whose files changed
    /// since and no longer pass loses its skills here, not the others'.
    pub fn skills(&self) -> Vec<PackageSkills> {
        self.active()
            .iter()
            .filter(|p| p.manifest.skills.is_some())
            .filter_map(|p| match ::packages::skills::declared_skills(p) {
                Ok(skills) => Some(PackageSkills {
                    plugin: p.name().to_string(),
                    skills: skills.into_iter().map(|s| (s.entry, s.body)).collect(),
                }),
                Err(e) => {
                    tracing::warn!(
                        plugin = %p.name(),
                        error = %e,
                        "package skills no longer load; skipping them"
                    );
                    None
                }
            })
            .collect()
    }

    pub fn note_script_faults(&self, faults: Vec<(String, String)>) {
        let mut by_plugin: HashMap<String, Vec<String>> = HashMap::new();
        for (plugin, reason) in faults {
            by_plugin.entry(plugin).or_default().push(reason);
        }
        *self
            .script_faults
            .write()
            .unwrap_or_else(|e| e.into_inner()) = by_plugin;
    }

    /// Every plugin on disk with its enable state — including the
    /// disabled ones, which a management UI has to show in order to turn
    /// them back on — and the host's own, ahead of an installed one of the
    /// same name.
    pub fn list(&self) -> Vec<PluginInfo> {
        let (global, scene) = self.tier_dirs();
        let global_state = ::packages::state::EnableState::new(global.clone());
        let scene_state = ::packages::state::EnableState::new(scene.clone());
        let host = self.host();
        let mut listed: Vec<PluginInfo> = host
            .iter()
            .map(|p| {
                let mut info = self.info(p, PluginOrigin::Host);
                info.enabled = info.disabled_reason.is_none();
                info
            })
            .collect();
        for p in ::packages::discover_plugins(&global, &scene, &self.paths.layout()) {
            let name = p.name();
            let mut info = self.info(&p, PluginOrigin::Installed);
            let missing = ::packages::host::missing_libraries(&p, &self.libraries);
            if host.iter().any(|h| h.name() == name) {
                info.shadowed_by = Some(PluginOrigin::Host);
                info.disabled_reason = Some(format!(
                    "the host provides its own package `{name}`, which is loaded instead"
                ));
            } else if info.disabled_reason.is_none() && !missing.is_empty() {
                info.disabled_reason =
                    Some(::packages::manifest::PluginError::MissingLibrary(missing).to_string());
            }
            info.enabled = info.disabled_reason.is_none()
                && ::packages::state::resolve_enabled(name, &global_state, &scene_state);
            listed.push(info);
        }
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        listed
    }

    fn info(&self, p: &Plugin, source: PluginOrigin) -> PluginInfo {
        let name = p.name();
        let skills: Vec<String> = ::packages::skills::declared_skills(p)
            .map(|skills| skills.into_iter().map(|s| s.entry.name).collect())
            .unwrap_or_default();
        let script_faults = self.script_faults.read().unwrap_or_else(|e| e.into_inner());
        PluginInfo {
            name: name.to_string(),
            version: p.manifest.plugin.version.clone(),
            description: p.manifest.plugin.description.clone(),
            source,
            enabled: false,
            disabled_reason: (!self.policy.permits(name))
                .then(|| "not permitted by settings.plugins.allow".to_string()),
            shadowed_by: None,
            // The unpacked directory. Without it a host that wants
            // to read something out of the package — a UI bundle,
            // an icon — has to rebuild this path itself, which
            // makes the daemon's disk layout part of its API.
            root: p.root.display().to_string(),
            script_faults: script_faults.get(name).cloned().unwrap_or_default(),
            skills,
            skill_conflicts: Vec::new(),
            commands: super::commands::listed(p),
            command_conflicts: Vec::new(),
            functions: super::functions::listed(p),
        }
    }

    pub fn tier_dirs(&self) -> (PathBuf, PathBuf) {
        tier_dirs(self.paths.as_ref())
    }

    pub fn tier_root(&self, scope: &str) -> Result<PathBuf, String> {
        let (global, scene) = self.tier_dirs();
        match scope {
            "global" => Ok(global),
            "scene" => Ok(scene),
            other => Err(format!(
                "invalid scope '{other}' — expected 'global' or 'scene'"
            )),
        }
    }

    /// Load back what was just installed, the way discovery will, and
    /// produce the disclosure an installer shows.
    ///
    /// Unpacking and being found are two different pieces of code and
    /// only the second one reads the manifest, so a package can unpack
    /// cleanly and still be one nothing will ever load. Left alone it is
    /// installed in no sense a caller cares about: absent from `list`,
    /// contributing nothing, with the reason in a log line nobody is
    /// reading. Asking here is what turns that into an answer at the
    /// moment somebody is standing in front of it. A disclosure over its
    /// limits is the same kind of answer: the package is refused, not
    /// installed with a note attached.
    pub fn load_back(
        &self,
        name: &str,
        version: &str,
        scope: &str,
    ) -> Result<::packages::Disclosure, String> {
        let cache = ::packages::cache::PluginCache::new(
            self.tier_root(scope)?.join(base::paths::entry::CACHE),
        );
        let manifest = cache
            .cached_manifest(name, version)
            .ok_or_else(|| format!("no plugin.toml in the installed package `{name}`"))?;
        let loaded = Plugin::load(
            &cache.version_dir(name, version),
            &manifest,
            &self.paths.layout(),
        )
        .map_err(|e| e.to_string())?;
        if loaded.manifest.plugin.name != name {
            return Err(format!(
                "the package calls itself `{}`, not `{name}`",
                loaded.manifest.plugin.name
            ));
        }
        ::packages::host::check_libraries(&loaded, &self.libraries).map_err(|e| e.to_string())?;
        ::packages::Disclosure::from_plugin(&loaded, &self.libraries).map_err(|e| e.to_string())
    }

    pub fn cache_for(&self, scope: &str) -> Result<::packages::cache::PluginCache, String> {
        Ok(::packages::cache::PluginCache::new(
            self.tier_root(scope)?.join(base::paths::entry::CACHE),
        ))
    }

    pub fn set_enabled(&self, name: &str, enabled: bool, scope: &str) -> Result<(), String> {
        ::packages::state::EnableState::new(self.tier_root(scope)?)
            .set_enabled(name, enabled)
            .map_err(|e| e.to_string())
    }

    /// Keep a package that is about to be installed off until the host
    /// enables it, by writing its entry before it exists: writing it
    /// afterwards would leave a window in which a rescan counts it as
    /// enabled. Returns whether an entry was written, which
    /// [`release_hold`](Self::release_hold) takes back if the install
    /// fails. A name the tier already has an entry for keeps it.
    pub fn hold_disabled(&self, name: &str, scope: &str) -> Result<bool, String> {
        let state = ::packages::state::EnableState::new(self.tier_root(scope)?);
        if state.get(name).is_some() {
            return Ok(false);
        }
        state.set_enabled(name, false).map_err(|e| e.to_string())?;
        Ok(true)
    }

    pub fn release_hold(&self, name: &str, scope: &str) {
        if let Ok(root) = self.tier_root(scope) {
            let _ = ::packages::state::EnableState::new(root).clear(name);
        }
    }

    pub fn resolved_enabled(&self, name: &str) -> bool {
        let (global, scene) = self.tier_dirs();
        ::packages::state::resolve_enabled(
            name,
            &::packages::state::EnableState::new(global),
            &::packages::state::EnableState::new(scene),
        )
    }
}

/// The two plugin-tier roots (plain `plugins/`, not `plugins/cache/` —
/// see `packages::cache`): global, shared across scenes, and this daemon's
/// own scene.
fn tier_dirs(paths: &dyn DaemonPaths) -> (PathBuf, PathBuf) {
    (
        base::paths::StateRoot::new(paths.global_root()).plugins_dir(),
        paths.config_root().join(base::paths::entry::PLUGINS),
    )
}

fn active_plugins(
    paths: &dyn DaemonPaths,
    policy: &PluginsConfig,
    host: &[Plugin],
    libraries: &[HostLibrary],
) -> Vec<Plugin> {
    let (global, scene) = tier_dirs(paths);
    let global_state = ::packages::state::EnableState::new(global.clone());
    let scene_state = ::packages::state::EnableState::new(scene.clone());
    let installed = ::packages::discover_plugins(&global, &scene, &paths.layout())
        .into_iter()
        .filter(|p| !host.iter().any(|h| h.name() == p.name()))
        .filter(|p| {
            let missing = ::packages::host::missing_libraries(p, libraries);
            if !missing.is_empty() {
                tracing::warn!(
                    plugin = %p.name(),
                    missing = %missing.join(", "),
                    "uses host libraries this host does not provide; skipping"
                );
            }
            missing.is_empty()
        })
        .filter(|p| ::packages::state::resolve_enabled(p.name(), &global_state, &scene_state));
    let mut active: Vec<Plugin> = host
        .iter()
        .cloned()
        .chain(installed)
        .filter(|p| {
            let permitted = policy.permits(p.name());
            if !permitted {
                tracing::info!(plugin = %p.name(), "not permitted by settings.plugins; skipping");
            }
            permitted
        })
        .collect();
    active.sort_by(|a, b| a.name().cmp(b.name()));
    for (_, server) in active.iter().flat_map(::packages::mcp::servers_for) {
        base::interface::exec::env::hold_back_what_an_mcp_server_reads(&server);
    }
    active
}

#[cfg(test)]
mod tests {
    use super::*;

    const XFLOW: &str = "uses = [\"codix\"]\n[plugin]\nname = \"xflow\"\nversion = \"VERSION\"\n\
        api_version = \"0.1\"\n[[command]]\nname = \"flow\"\nentry = \"x.js:main\"\n\
        description = \"d\"\n[[function]]\nname = \"check\"\nentry = \"x.js:check\"\n\
        description = \"d\"\n";

    fn write_package(dir: &Path, version: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), XFLOW.replace("VERSION", version)).unwrap();
        std::fs::write(
            dir.join("x.js"),
            "function main() {}\nfunction check() {}\n",
        )
        .unwrap();
        std::fs::write(dir.join(".agix-installed"), "").unwrap();
    }

    fn codix() -> Vec<HostLibrary> {
        vec![HostLibrary {
            name: "codix".into(),
            description: None,
        }]
    }

    fn packages(root: &Path, libraries: Vec<HostLibrary>) -> Packages {
        let paths = crate::config::StaticDaemonPaths::new(root.join("scene"))
            .with_global(root.join("global"));
        Packages::new(Arc::new(paths), PluginsConfig::default(), libraries)
    }

    #[test]
    fn a_host_package_takes_the_place_of_an_installed_one_of_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_package(&root.join("global/plugins/cache/xflow/1.0.0"), "1.0.0");
        let p = packages(&root, codix());
        assert_eq!(p.active()[0].manifest.plugin.version, "1.0.0");

        write_package(&root.join("host/xflow"), "2.0.0");
        assert_eq!(p.register_host(&root.join("host/xflow")).unwrap(), "xflow");
        assert!(p.is_host("xflow"));
        let active = p.active();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].manifest.plugin.version, "2.0.0");

        let functions = p.functions();
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].call_name(), "xflow.check");
        assert_eq!(functions[0].plugin_version, "2.0.0");
        assert_eq!(functions[0].uses, ["codix"]);
        assert_eq!(
            functions[0].bytecode_dir.as_deref(),
            Some(root.join("global/plugins/cache/.bytecode").as_path())
        );
        assert_eq!(p.commands()[0].uses, ["codix"]);

        let listed = p.list();
        let origins: Vec<_> = listed
            .iter()
            .map(|i| (i.source, i.shadowed_by, i.enabled))
            .collect();
        assert_eq!(
            origins,
            [
                (PluginOrigin::Host, None, true),
                (PluginOrigin::Installed, Some(PluginOrigin::Host), false),
            ]
        );
    }

    #[test]
    fn a_package_whose_library_the_host_lacks_is_listed_and_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_package(&root.join("global/plugins/cache/xflow/1.0.0"), "1.0.0");
        let p = packages(&root, Vec::new());
        assert!(p.active().is_empty());
        assert!(p.functions().is_empty());
        let listed = p.list();
        assert!(!listed[0].enabled);
        assert_eq!(
            listed[0].disabled_reason.as_deref(),
            Some("needs host library `codix`, which this host does not provide")
        );
        write_package(&root.join("host/xflow"), "2.0.0");
        let e = p.register_host(&root.join("host/xflow")).unwrap_err();
        assert!(e.contains("needs host library `codix`"), "{e}");
        assert!(!p.is_host("xflow"));
    }
}
