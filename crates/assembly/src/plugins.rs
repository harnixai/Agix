//! The plugin package subsystem, behind one type the rest of the daemon talks
//! to.
//!
//! Reading a package — manifest, fetch, checksum, unpack, cache, disclose,
//! enable — is the `plugin-packages` feature. A package runs nothing of its
//! own: its MCP servers, script bindings, scene directory, agent types and
//! skills are honored by the parts of the engine that already exist, and its
//! commands and functions by the engine's QuickJS. The host that embeds the
//! engine may bring packages of its own and the libraries packages use.
//! Without the feature the same API answers that there are no plugins, so the
//! rest of the daemon needs no conditional compilation of its own.

#[cfg(feature = "plugin-packages")]
mod commands;
#[cfg(feature = "plugin-packages")]
mod functions;
#[cfg(feature = "plugin-packages")]
mod health;
pub(crate) mod host_libraries;
#[cfg(feature = "plugin-packages")]
mod packages;
mod policy;

pub use policy::PluginsConfig;

use crate::config::DaemonPaths;
use crate::session_pool::api::{
    PluginInfo, PluginInstalled, PluginRemoved, PluginScope, PluginToggled,
};
use base::command::HostLibrary;
use std::collections::HashMap;
use std::sync::Arc;

/// What `daemon.doctor` reports, so "does this binary have plugins" is
/// answerable from the running process rather than from release notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginStatus {
    /// Not compiled into this binary at all.
    CompiledOut,
    Enabled,
    /// Compiled in, switched off by configuration.
    DisabledByPolicy,
}

impl PluginStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompiledOut => "compiled-out",
            Self::Enabled => "enabled",
            Self::DisabledByPolicy => "disabled-by-policy",
        }
    }
}

/// Error returned by every management call in a build without the package
/// layer.
pub const DISABLED_MESSAGE: &str = "plugins are not available in this build";

/// One installed package's script bindings, ready for the carrier.
///
/// Carries the package's name and its unpacked directory rather than only the
/// bindings, because both decide what the scripts may do: the directory is
/// what a path is resolved against and checked against, and the name is the
/// authority they run under. Inferring either from the other end would put
/// the answer back where a path layout could change it.
pub struct PackageScripts {
    pub name: String,
    pub root: std::path::PathBuf,
    pub bindings: Vec<base::interface::script::ScriptBinding>,
}

/// What the active packages hand a session as it is built.
pub struct SessionPackages {
    pub scripts: Vec<PackageScripts>,
    pub commands: Vec<base::command::ExtCommand>,
}

/// One active package's skills, each with the body it expands to.
#[derive(Clone)]
pub struct PackageSkills {
    pub plugin: String,
    pub skills: Vec<(base::skill::SkillEntry, String)>,
}

/// What the active packages add to a project's catalogs.
#[derive(Clone, Default)]
pub struct Contributions {
    pub agent_types: Vec<base::agent_type::AgentTypeDefinition>,
    /// In package-name order, which is the order the catalog takes them in.
    pub skills: Vec<PackageSkills>,
}

#[cfg(feature = "plugin-packages")]
fn disclosure_of(d: &::packages::Disclosure) -> crate::session_pool::api::PluginDisclosure {
    use crate::session_pool::api::{PluginDisclosure, VisibleText};
    PluginDisclosure {
        plugin: d.plugin.clone(),
        version: d.version.clone(),
        capabilities: d.capabilities.clone(),
        scene: d.scene.clone(),
        skills: d.skills.clone(),
        commands: d.commands.clone(),
        functions: d.functions.clone(),
        mcp_servers: d.mcp_servers.clone(),
        model_visible: d
            .model_visible
            .iter()
            .map(|v| VisibleText {
                origin: v.origin.clone(),
                text: v.text.clone(),
            })
            .collect(),
        inert: d.is_inert(),
    }
}

#[cfg(feature = "plugin-packages")]
mod imp {
    use super::health::{
        DefaultEngineNameCheck, EnableOnInstallCheck, EnableStateCheck, RefusedPackagesCheck,
    };
    use super::*;
    use base::agent_type::AgentTypeDefinition;

    pub struct PluginSubsystem {
        packages: super::packages::Packages,
    }

    impl PluginSubsystem {
        /// `libraries` is what the host provides to package code; a package
        /// that uses another does not install or load.
        pub fn new(
            paths: Arc<dyn DaemonPaths>,
            policy: PluginsConfig,
            libraries: Vec<HostLibrary>,
        ) -> Self {
            Self {
                packages: super::packages::Packages::new(paths, policy, libraries),
            }
        }

        pub fn status(&self) -> PluginStatus {
            if self.packages.is_enabled() {
                PluginStatus::Enabled
            } else {
                PluginStatus::DisabledByPolicy
            }
        }

        pub fn policy(&self) -> &PluginsConfig {
            self.packages.policy()
        }

        pub fn agent_types(&self) -> Vec<AgentTypeDefinition> {
            self.packages.agent_types()
        }

        pub fn contributions(&self) -> Contributions {
            Contributions {
                agent_types: self.packages.agent_types(),
                skills: self.packages.skills(),
            }
        }

        pub fn scenes(&self) -> Vec<Arc<base::interface::scene::SceneSpec>> {
            ::packages::scenes::declared_scenes(&self.packages.active())
        }

        pub fn mcp_servers(&self) -> HashMap<String, serde_json::Value> {
            self.packages.mcp_servers()
        }

        pub fn mcp_server_dirs(&self) -> HashMap<String, std::path::PathBuf> {
            self.packages.mcp_server_dirs()
        }

        pub fn mcp_key_clashes(&self) -> Vec<(String, String, String)> {
            self.packages.mcp_key_clashes()
        }

        pub fn for_session(&self) -> SessionPackages {
            SessionPackages {
                scripts: self.packages.script_bindings(),
                commands: self.packages.commands(),
            }
        }

        pub fn commands(&self) -> Vec<base::command::ExtCommand> {
            self.packages.commands()
        }

        pub fn command_clashes(&self) -> Vec<(String, String, String)> {
            self.packages.command_clashes()
        }

        /// Every function the active packages expose to the host.
        pub fn functions(&self) -> Vec<base::command::PluginFunction> {
            self.packages.functions()
        }

        /// Take up the package at `dir` as one of the host's own. Returns
        /// its name.
        pub fn register_host(&self, dir: &std::path::Path) -> Result<String, String> {
            self.packages.register_host(dir)
        }

        fn refuse_host(&self, name: &str, what: &str) -> Result<(), String> {
            if self.packages.is_host(name) {
                return Err(format!(
                    "`{name}` is the host's own package; it cannot be {what} through the \
                     protocol"
                ));
            }
            Ok(())
        }

        pub fn note_script_faults(&self, faults: Vec<(String, String)>) {
            self.packages.note_script_faults(faults);
        }

        pub fn count(&self) -> usize {
            self.packages.count()
        }

        pub fn list(&self) -> Vec<PluginInfo> {
            self.packages.list()
        }

        pub fn health_check(&self) -> Vec<Arc<dyn base::interface::health::HealthCheck>> {
            if !self.packages.is_enabled() {
                return Vec::new();
            }
            let layout = self.packages.layout();
            let renamed = DefaultEngineNameCheck::under(
                layout.engine(),
                self.packages.commands(),
                self.packages.script_bindings(),
            );
            let mut checks: Vec<Arc<dyn base::interface::health::HealthCheck>> = vec![
                Arc::new(EnableStateCheck(self.packages.tier_dirs())),
                Arc::new(RefusedPackagesCheck {
                    tiers: self.packages.tier_dirs(),
                    layout: layout.clone(),
                    libraries: self.packages.libraries().to_vec(),
                }),
                Arc::new(EnableOnInstallCheck(
                    self.packages.policy().enable_on_install,
                )),
            ];
            checks.extend(renamed.map(|c| Arc::new(c) as _));
            checks
        }

        pub async fn install(
            &self,
            name: &str,
            version: &str,
            download_url: &str,
            checksum: Option<&str>,
            scope: PluginScope,
        ) -> Result<PluginInstalled, String> {
            let scope = scope.as_str();
            let cache = self.packages.cache_for(scope)?;
            let held = !self.packages.policy().enable_on_install
                && !cache.is_cached(name, version)
                && self.packages.hold_disabled(name, scope)?;
            let installed = self
                .install_and_load_back(&cache, name, version, download_url, checksum, scope)
                .await;
            if held && installed.is_err() {
                self.packages.release_hold(name, scope);
            }
            let (message, disclosure) = installed?;

            self.refresh().await;
            Ok(PluginInstalled {
                success: true,
                message,
                enabled: self.packages.resolved_enabled(name),
                disclosure: super::disclosure_of(&disclosure),
            })
        }

        async fn install_and_load_back(
            &self,
            cache: &::packages::cache::PluginCache,
            name: &str,
            version: &str,
            download_url: &str,
            checksum: Option<&str>,
            scope: &str,
        ) -> Result<(String, ::packages::Disclosure), String> {
            let source = ::packages::install::PluginSource {
                download_url: download_url.to_string(),
                checksum: checksum.map(str::to_string),
                version: version.to_string(),
            };
            let installed = ::packages::install::install(
                cache,
                name,
                &source,
                &self.packages.layout(),
                self.packages.libraries(),
            )
            .await
            .map_err(|e| e.to_string())?;
            let loaded = self.packages.load_back(name, version, scope);
            let (message, disclosure) = match (installed, loaded) {
                (::packages::install::Installed::AlreadyPresent, Ok(d)) => (
                    format!("plugin '{name}' v{version} is already installed"),
                    d,
                ),
                (::packages::install::Installed::AlreadyPresent, Err(e)) => {
                    return Err(format!(
                        "plugin '{name}' v{version} is already installed but is refused on \
                         loading it back: {e}; uninstall it before installing it again"
                    ));
                }
                (::packages::install::Installed::Into(_), Ok(d)) => (
                    format!("installed plugin '{name}' v{version} from {download_url}"),
                    d,
                ),
                (::packages::install::Installed::Into(_), Err(e)) => {
                    let _ = ::packages::install::uninstall(cache, name, Some(version));
                    return Err(format!(
                        "installed but refused on loading it back, so it was removed: {e}"
                    ));
                }
            };
            Ok((message, disclosure))
        }

        pub async fn uninstall(
            &self,
            name: &str,
            version: Option<&str>,
            scope: PluginScope,
        ) -> Result<PluginRemoved, String> {
            let cache = self.packages.cache_for(scope.as_str())?;
            let shadowed = || match version {
                Some(version) => cache.is_cached(name, version),
                None => !cache.list_versions(name).is_empty(),
            };
            if self.packages.is_host(name) && !shadowed() {
                self.refuse_host(name, "uninstalled")?;
            }
            let removed =
                ::packages::install::uninstall(&cache, name, version).map_err(|e| e.to_string())?;
            let message = match version {
                Some(version) => format!("removed plugin '{name}' v{version}"),
                None => format!("removed plugin '{name}' ({removed} versions)"),
            };
            self.refresh().await;
            Ok(PluginRemoved {
                success: true,
                message,
            })
        }

        pub async fn set_enabled(
            &self,
            name: &str,
            enabled: bool,
            scope: PluginScope,
        ) -> Result<PluginToggled, String> {
            self.refuse_host(name, if enabled { "enabled" } else { "disabled" })?;
            self.packages.set_enabled(name, enabled, scope.as_str())?;
            self.refresh().await;
            Ok(PluginToggled {
                name: name.to_string(),
                enabled,
                scope,
            })
        }

        pub async fn refresh(&self) {
            self.packages.rescan();
        }
    }
}

#[cfg(not(feature = "plugin-packages"))]
mod imp {
    use super::*;
    use base::agent_type::AgentTypeDefinition;

    pub struct PluginSubsystem;

    impl PluginSubsystem {
        pub fn new(
            _paths: Arc<dyn DaemonPaths>,
            _policy: PluginsConfig,
            _libraries: Vec<HostLibrary>,
        ) -> Self {
            Self
        }
        pub fn status(&self) -> PluginStatus {
            PluginStatus::CompiledOut
        }
        pub fn agent_types(&self) -> Vec<AgentTypeDefinition> {
            Vec::new()
        }
        pub fn contributions(&self) -> Contributions {
            Contributions::default()
        }
        pub fn scenes(&self) -> Vec<Arc<base::interface::scene::SceneSpec>> {
            Vec::new()
        }
        pub fn mcp_servers(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
        pub fn mcp_server_dirs(&self) -> HashMap<String, std::path::PathBuf> {
            HashMap::new()
        }
        pub fn mcp_key_clashes(&self) -> Vec<(String, String, String)> {
            Vec::new()
        }
        pub fn for_session(&self) -> SessionPackages {
            SessionPackages {
                scripts: Vec::new(),
                commands: Vec::new(),
            }
        }
        pub fn commands(&self) -> Vec<base::command::ExtCommand> {
            Vec::new()
        }
        pub fn command_clashes(&self) -> Vec<(String, String, String)> {
            Vec::new()
        }
        pub fn functions(&self) -> Vec<base::command::PluginFunction> {
            Vec::new()
        }
        pub fn register_host(&self, _dir: &std::path::Path) -> Result<String, String> {
            Err(DISABLED_MESSAGE.to_string())
        }
        pub fn note_script_faults(&self, _faults: Vec<(String, String)>) {}
        pub fn count(&self) -> usize {
            0
        }
        pub fn list(&self) -> Vec<PluginInfo> {
            Vec::new()
        }
        pub fn health_check(&self) -> Vec<Arc<dyn base::interface::health::HealthCheck>> {
            Vec::new()
        }
        pub async fn install(
            &self,
            _name: &str,
            _version: &str,
            _download_url: &str,
            _checksum: Option<&str>,
            _scope: PluginScope,
        ) -> Result<PluginInstalled, String> {
            Err(DISABLED_MESSAGE.to_string())
        }
        pub async fn uninstall(
            &self,
            _name: &str,
            _version: Option<&str>,
            _scope: PluginScope,
        ) -> Result<PluginRemoved, String> {
            Err(DISABLED_MESSAGE.to_string())
        }
        pub async fn set_enabled(
            &self,
            _name: &str,
            _enabled: bool,
            _scope: PluginScope,
        ) -> Result<PluginToggled, String> {
            Err(DISABLED_MESSAGE.to_string())
        }
        pub async fn refresh(&self) {}
    }
}

pub use imp::PluginSubsystem;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_strings_are_the_ones_doctor_reports() {
        assert_eq!(PluginStatus::CompiledOut.as_str(), "compiled-out");
        assert_eq!(PluginStatus::Enabled.as_str(), "enabled");
        assert_eq!(
            PluginStatus::DisabledByPolicy.as_str(),
            "disabled-by-policy"
        );
    }
}
