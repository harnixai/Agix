//! The modules this build assembles into sessions — the one place the
//! assembly names them, and with the MCP plumbing in `session_pool` the only
//! place their features appear.
//!
//! Per project and scene, a [`ProjectModules`] holds what the modules share
//! across that pair's sessions (the skill catalog, the agent-type catalog,
//! and what installed packages contribute to both), and the same catalogs
//! for the sessions that draw on only some of the packages, one pair per set;
//! per session, [`ProjectModules::session_modules`] hands the builder the
//! modules; `build()` checks their order against each module's `Module::after`.

use std::collections::BTreeSet;
use std::sync::Arc;

use base::agent_type::AgentTypeDefinition;
use runtime::commands::CommandRegistry;
use runtime::module::Module;
use settings::Settings;

/// The modules compiled into this build, in assembly order.
pub fn compiled() -> Vec<&'static str> {
    let mut out = Vec::new();
    if cfg!(feature = "mcp") {
        out.push("mcp");
    }
    if cfg!(feature = "agents") {
        out.push("agents");
    }
    if cfg!(feature = "skills") {
        out.push("skills");
    }
    if cfg!(feature = "scripts") {
        out.push("scripts");
    }
    if cfg!(feature = "plugin-packages") {
        out.push("plugin-packages");
    }
    out
}

/// The settings that configure a module this build does not carry, named
/// so a section that will be ignored says so instead of doing nothing.
pub fn unassembled_settings(settings: &Settings) -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    if !cfg!(feature = "mcp") && settings.mcp.declared().next().is_some() {
        out.push(("mcp.servers", "mcp"));
    }
    let team_enabled = settings
        .sections
        .get("team")
        .and_then(|team| team.get("enabled"))
        .and_then(serde_json::Value::as_bool);
    if !cfg!(feature = "agents") && team_enabled == Some(true) {
        out.push(("team.enabled", "agents"));
    }
    out
}

pub(crate) struct ProjectModules {
    /// Live-reloaded: a skill file added or removed after this was built is
    /// picked up without a rebuild, and every session on this pair sees it at
    /// once because they share this instance.
    #[cfg(feature = "skills")]
    skills: Arc<skills::manager::SkillManager>,
    /// Kept so a rescan of the skill directories rebuilds the catalog with
    /// the packages' skills rather than without them.
    #[cfg(feature = "skills")]
    plugin_skills: std::sync::RwLock<Vec<crate::plugins::PackageSkills>>,
    #[cfg(feature = "agents")]
    agent_types: agents::agent_tool::SharedAgentTypeCatalog,
    /// Kept so the narrowed catalogs are merged from the packages' types too.
    #[cfg(feature = "agents")]
    plugin_agent_types: std::sync::RwLock<Vec<AgentTypeDefinition>>,
    /// The catalogs of sessions that draw on fewer packages than the pool
    /// has, by the set they draw on; each is dropped once no session holds it.
    narrowed: std::sync::Mutex<Vec<Narrowed>>,
}

/// The catalogs of the sessions on a pair that draw on only `packages`:
/// merged from the tiers, the built-ins and those packages alone, so a name
/// a left-out package held goes to whatever is below it.
struct Narrowed {
    packages: BTreeSet<String>,
    #[cfg(feature = "skills")]
    skills: Arc<skills::manager::SkillManager>,
    #[cfg(feature = "agents")]
    agent_types: agents::agent_tool::SharedAgentTypeCatalog,
}

impl Narrowed {
    fn held(&self) -> bool {
        #[cfg_attr(not(any(feature = "skills", feature = "agents")), allow(unused_mut))]
        let mut held = false;
        #[cfg(feature = "skills")]
        {
            held |= Arc::strong_count(&self.skills) > 1;
        }
        #[cfg(feature = "agents")]
        {
            held |= Arc::strong_count(&self.agent_types.handle()) > 2;
        }
        held
    }
}

#[cfg(feature = "skills")]
fn skills_of(
    all: &[crate::plugins::PackageSkills],
    packages: &BTreeSet<String>,
) -> Vec<crate::plugins::PackageSkills> {
    all.iter()
        .filter(|p| packages.contains(&p.plugin))
        .cloned()
        .collect()
}

#[cfg(feature = "agents")]
fn agent_types_of(
    all: &[AgentTypeDefinition],
    packages: &BTreeSet<String>,
) -> Vec<AgentTypeDefinition> {
    all.iter()
        .filter(|d| match &d.source {
            base::agent_type::AgentTypeSource::Plugin { plugin } => packages.contains(plugin),
            _ => true,
        })
        .cloned()
        .collect()
}

/// The catalogs one session is built with.
struct Catalogs {
    #[cfg(feature = "skills")]
    skills: Arc<skills::manager::SkillManager>,
    #[cfg(feature = "agents")]
    agent_types: agents::agent_tool::SharedAgentTypeMap,
}

impl ProjectModules {
    /// Neither catalog starts a watcher of its own: the pool watches every
    /// tier through one `WatchHub` and calls [`Self::reload_skills`] /
    /// [`Self::refresh_agent_types`], so a second mechanism would be both
    /// redundant and — at one `notify` stream per catalog per project — the
    /// thing that runs a process out of them.
    pub(crate) fn build(
        settings: &settings::LoadedSettings,
        plugins: &crate::plugins::Contributions,
    ) -> Self {
        let _ = (settings, plugins);
        Self {
            #[cfg(feature = "skills")]
            skills: Arc::new(skill_catalog(settings, &plugins.skills)),
            #[cfg(feature = "skills")]
            plugin_skills: std::sync::RwLock::new(plugins.skills.clone()),
            #[cfg(feature = "agents")]
            agent_types: agents::agent_tool::SharedAgentTypeCatalog::unwatched(
                &agents::agent_tool::agent_type_dirs(settings),
                &plugins.agent_types,
                &agents::agent_tool::agent_type_files(settings),
            ),
            #[cfg(feature = "agents")]
            plugin_agent_types: std::sync::RwLock::new(plugins.agent_types.clone()),
            narrowed: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The narrowed catalogs some session still holds.
    fn narrowed(&self) -> std::sync::MutexGuard<'_, Vec<Narrowed>> {
        let mut narrowed = self.narrowed.lock().unwrap_or_else(|e| e.into_inner());
        narrowed.retain(Narrowed::held);
        narrowed
    }

    #[cfg_attr(
        not(any(feature = "agents", feature = "skills")),
        allow(unused_variables)
    )]
    fn catalogs_for(
        &self,
        settings: &settings::LoadedSettings,
        plugins: &crate::plugins::SessionPlugins,
    ) -> Catalogs {
        if !plugins.is_narrowed() {
            return Catalogs {
                #[cfg(feature = "skills")]
                skills: self.skills.clone(),
                #[cfg(feature = "agents")]
                agent_types: self.agent_types.handle(),
            };
        }
        let packages = plugins.names();
        let mut narrowed = self.narrowed();
        let at = match narrowed.iter().position(|n| n.packages == packages) {
            Some(at) => at,
            None => {
                narrowed.push(Narrowed {
                    #[cfg(feature = "skills")]
                    skills: Arc::new(skill_catalog(
                        settings,
                        &skills_of(&self.plugin_skills(), &packages),
                    )),
                    #[cfg(feature = "agents")]
                    agent_types: agents::agent_tool::SharedAgentTypeCatalog::unwatched(
                        &agents::agent_tool::agent_type_dirs(settings),
                        &agent_types_of(&self.plugin_agent_types(), &packages),
                        &agents::agent_tool::agent_type_files(settings),
                    ),
                    packages,
                });
                narrowed.len() - 1
            }
        };
        #[cfg_attr(
            not(any(feature = "agents", feature = "skills")),
            allow(unused_variables)
        )]
        let found = &narrowed[at];
        Catalogs {
            #[cfg(feature = "skills")]
            skills: found.skills.clone(),
            #[cfg(feature = "agents")]
            agent_types: found.agent_types.handle(),
        }
    }

    #[cfg(feature = "skills")]
    fn plugin_skills(&self) -> Vec<crate::plugins::PackageSkills> {
        self.plugin_skills
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    #[cfg(feature = "agents")]
    fn plugin_agent_types(&self) -> Vec<AgentTypeDefinition> {
        self.plugin_agent_types
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The command catalog shared by every session on the pair: the built-in
    /// local commands plus a live view of the skill catalog, so a skill file
    /// added or deleted later is reflected — in `commands.list` too — without
    /// a rebuild.
    pub(crate) fn commands(&self) -> Arc<CommandRegistry> {
        #[cfg(feature = "skills")]
        return Arc::new(CommandRegistry::from_skill_catalog(self.skills.clone()));
        #[cfg(not(feature = "skills"))]
        Arc::new(CommandRegistry::default())
    }

    pub(crate) fn skill_count(&self) -> usize {
        #[cfg(feature = "skills")]
        return self.skills.list().len();
        #[cfg(not(feature = "skills"))]
        0
    }

    /// Re-read the skill directories into the catalog the pair's sessions
    /// already share. A running session notices the difference at the start
    /// of its next turn.
    pub(crate) fn reload_skills(&self, settings: &settings::LoadedSettings) {
        #[cfg(feature = "skills")]
        {
            let packages = self.plugin_skills();
            self.skills
                .replace_catalog(skill_catalog(settings, &packages));
            for narrowed in self.narrowed().iter() {
                narrowed.skills.replace_catalog(skill_catalog(
                    settings,
                    &skills_of(&packages, &narrowed.packages),
                ));
            }
        }
        #[cfg(not(feature = "skills"))]
        let _ = settings;
    }

    /// Take up what the active packages contribute now.
    pub(crate) fn refresh_plugins(
        &self,
        settings: &settings::LoadedSettings,
        plugins: &crate::plugins::Contributions,
    ) {
        self.refresh_agent_types(settings, &plugins.agent_types);
        #[cfg(feature = "skills")]
        {
            *self
                .plugin_skills
                .write()
                .unwrap_or_else(|e| e.into_inner()) = plugins.skills.clone();
            self.reload_skills(settings);
        }
    }

    /// Package skills whose name another source holds in this catalog, by
    /// package: which source, and the file it was read from.
    pub(crate) fn skill_conflicts(
        &self,
    ) -> std::collections::HashMap<String, Vec<crate::session_pool::api::SkillConflict>> {
        #[cfg_attr(not(feature = "skills"), allow(unused_mut))]
        let mut out: std::collections::HashMap<
            String,
            Vec<crate::session_pool::api::SkillConflict>,
        > = std::collections::HashMap::new();
        #[cfg(feature = "skills")]
        {
            let packages = self.plugin_skills.read().unwrap_or_else(|e| e.into_inner());
            let package_of = |path: &std::path::Path| {
                packages
                    .iter()
                    .find(|p| p.skills.iter().any(|(entry, _)| entry.path == path))
                    .map(|p| p.plugin.clone())
            };
            for package in packages.iter() {
                for (entry, _) in &package.skills {
                    let Some(holder) = self.skills.get(&entry.name) else {
                        continue;
                    };
                    if holder.path == entry.path {
                        continue;
                    }
                    use crate::session_pool::api::SkillOrigin;
                    out.entry(package.plugin.clone()).or_default().push(
                        crate::session_pool::api::SkillConflict {
                            skill: entry.name.clone(),
                            shadowed_by: match holder.source {
                                skills::manager::SkillSource::User => SkillOrigin::User,
                                skills::manager::SkillSource::Project => SkillOrigin::Project,
                                skills::manager::SkillSource::Plugin => SkillOrigin::Plugin,
                                skills::manager::SkillSource::Host => SkillOrigin::Host,
                            },
                            path: holder.path.display().to_string(),
                            plugin: package_of(&holder.path),
                        },
                    );
                }
            }
        }
        out
    }

    /// Re-merge the agent types from the tiers and what plugins contribute.
    pub(crate) fn refresh_agent_types(
        &self,
        settings: &settings::LoadedSettings,
        plugin_agent_types: &[AgentTypeDefinition],
    ) {
        #[cfg(feature = "agents")]
        {
            let (dirs, files) = (
                agents::agent_tool::agent_type_dirs(settings),
                agents::agent_tool::agent_type_files(settings),
            );
            self.agent_types.refresh(&dirs, plugin_agent_types, &files);
            *self
                .plugin_agent_types
                .write()
                .unwrap_or_else(|e| e.into_inner()) = plugin_agent_types.to_vec();
            for narrowed in self.narrowed().iter() {
                narrowed.agent_types.refresh(
                    &dirs,
                    &agent_types_of(plugin_agent_types, &narrowed.packages),
                    &files,
                );
            }
        }
        #[cfg(not(feature = "agents"))]
        let _ = (settings, plugin_agent_types);
    }

    #[cfg(all(test, feature = "skills"))]
    pub(crate) fn skills(&self) -> &Arc<skills::manager::SkillManager> {
        &self.skills
    }

    #[cfg(all(test, feature = "agents"))]
    pub(crate) fn has_agent_type(&self, name: &str) -> bool {
        self.agent_types
            .handle()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(name)
    }

    /// Everything after MCP, in order, for a session on the pair's `settings`
    /// with the host's skill directories `host`, drawing on `plugins`. Pushed
    /// one by one because each push is behind its own feature.
    #[allow(clippy::vec_init_then_push)]
    pub(crate) fn session_modules(
        &self,
        settings: &settings::LoadedSettings,
        host: &crate::session_pool::SessionSkillDirs,
        plugins: &crate::plugins::SessionPlugins,
    ) -> Vec<Arc<dyn Module>> {
        #[cfg(not(feature = "skills"))]
        let _ = host;
        #[cfg_attr(
            not(any(feature = "agents", feature = "skills")),
            allow(unused_variables)
        )]
        let catalogs = self.catalogs_for(settings, plugins);
        #[cfg_attr(not(any(feature = "agents", feature = "skills")), allow(unused_mut))]
        let mut out: Vec<Arc<dyn Module>> = Vec::new();
        #[cfg(feature = "agents")]
        out.push(Arc::new(
            agents::AgentsModule::new().shared_agent_types(catalogs.agent_types),
        ));
        #[cfg(feature = "skills")]
        out.push(Arc::new(
            skills::SkillsModule::shared(catalogs.skills).with_host(host.layer().clone()),
        ));
        out
    }

    /// What each of `host`'s directories gives a session on this pair.
    #[cfg(feature = "skills")]
    pub(crate) fn skill_dir_reports(
        &self,
        host: &crate::session_pool::SessionSkillDirs,
    ) -> Vec<skills::host::HostDirReport> {
        host.layer().report(&self.skills)
    }
}

#[cfg(feature = "skills")]
fn skill_catalog(
    settings: &settings::LoadedSettings,
    packages: &[crate::plugins::PackageSkills],
) -> skills::manager::SkillManager {
    let providers: Vec<Arc<dyn base::interface::skill_provider::SkillProvider>> = packages
        .iter()
        .map(|p| {
            Arc::new(skills::sources::PluginSkills::new(
                &p.plugin,
                p.skills.clone(),
            )) as Arc<dyn base::interface::skill_provider::SkillProvider>
        })
        .collect();
    skills::module::build_skill_manager_with_plugins(settings, &providers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_section_for_a_missing_module_is_named() {
        let mut settings = Settings::defaults_for("test");
        settings.mcp =
            serde_json::from_value(serde_json::json!({"servers": {"x": {"command": "x"}}}))
                .unwrap();
        settings
            .sections
            .insert("team".into(), serde_json::json!({"enabled": true}));
        let named = unassembled_settings(&settings);
        assert_eq!(
            named.contains(&("mcp.servers", "mcp")),
            !cfg!(feature = "mcp")
        );
        assert_eq!(
            named.contains(&("team.enabled", "agents")),
            !cfg!(feature = "agents")
        );
    }

    #[cfg(feature = "skills")]
    fn package(plugin: &str, names: &[&str]) -> crate::plugins::PackageSkills {
        crate::plugins::PackageSkills {
            plugin: plugin.into(),
            skills: names
                .iter()
                .map(|name| {
                    let entry = base::skill::SkillEntry {
                        name: (*name).into(),
                        description: format!("{plugin} {name}"),
                        source: base::skill::SkillSource::Plugin,
                        path: format!("/cache/{plugin}/skills/{name}/SKILL.md").into(),
                        ..Default::default()
                    };
                    (entry, format!("{plugin} body"))
                })
                .collect(),
        }
    }

    /// A package skill that loses its name is still listed with its package,
    /// and the conflict says who has the name: the disk, or the package that
    /// sorts first.
    #[cfg(feature = "skills")]
    #[test]
    fn a_shadowed_package_skill_is_reported_with_what_shadows_it() {
        let home = tempfile::tempdir().unwrap();
        let mut settings = settings::LoadedSettings::new(
            Settings::defaults_for("test"),
            base::paths::PathSettings::default(),
        );
        settings.paths.global_data_dir = home.path().join("global");
        settings.paths.user_data_dir = home.path().join("scene");
        settings.paths.local_data_dir = home.path().join("project/.agix");
        settings.paths.workspace =
            base::paths::Workspace::project_of(&settings.paths.local_data_dir);
        let disk = home.path().join("project/.agents/skills/tour");
        std::fs::create_dir_all(&disk).unwrap();
        std::fs::write(
            disk.join("SKILL.md"),
            "---\ndescription: the project's tour\n---\nbody\n",
        )
        .unwrap();

        let modules = ProjectModules::build(
            &settings,
            &crate::plugins::Contributions {
                skills: vec![
                    package("alpha", &["guide", "tour"]),
                    package("beta", &["guide", "solo"]),
                ],
                ..Default::default()
            },
        );
        let conflicts = modules.skill_conflicts();

        let alpha = &conflicts["alpha"];
        assert_eq!(alpha.len(), 1, "{conflicts:?}");
        assert_eq!(alpha[0].skill, "tour");
        assert_eq!(
            alpha[0].shadowed_by,
            crate::session_pool::api::SkillOrigin::Project
        );
        assert!(alpha[0].path.ends_with(".agents/skills/tour/SKILL.md"));
        assert!(alpha[0].plugin.is_none());

        let beta = &conflicts["beta"];
        assert_eq!(beta.len(), 1, "{conflicts:?}");
        assert_eq!(beta[0].skill, "guide");
        assert_eq!(
            beta[0].shadowed_by,
            crate::session_pool::api::SkillOrigin::Plugin
        );
        assert_eq!(beta[0].plugin.as_deref(), Some("alpha"));

        modules.refresh_plugins(
            &settings,
            &crate::plugins::Contributions {
                skills: vec![package("beta", &["guide", "solo"])],
                ..Default::default()
            },
        );
        assert!(modules.skill_conflicts().is_empty());
        assert_eq!(
            modules.skills().get_skill_content("guide").as_deref(),
            Some("beta body"),
            "with alpha gone, beta's skill has the name"
        );
        assert!(modules.skills().get("solo").is_some());
    }

    /// A session drawing on beta alone gets beta's skill under the name alpha
    /// holds pool-wide, the built-in agent type alpha replaced, and nothing of
    /// a package enabled after it was built.
    #[cfg(all(feature = "skills", feature = "agents"))]
    #[test]
    fn a_narrowed_session_gets_what_a_left_out_package_shadowed_and_no_later_package() {
        let home = tempfile::tempdir().unwrap();
        let mut settings = settings::LoadedSettings::new(
            Settings::defaults_for("test"),
            base::paths::PathSettings::default(),
        );
        settings.paths.global_data_dir = home.path().join("global");
        settings.paths.user_data_dir = home.path().join("scene");
        let explorer = AgentTypeDefinition {
            name: "explore".into(),
            source: base::agent_type::AgentTypeSource::Plugin {
                plugin: "alpha".into(),
            },
            system_prompt: "alpha explores".into(),
            ..Default::default()
        };
        let contributions = |skills| crate::plugins::Contributions {
            agent_types: vec![explorer.clone()],
            skills,
        };
        let modules = ProjectModules::build(
            &settings,
            &contributions(vec![
                package("alpha", &["guide"]),
                package("beta", &["guide", "solo"]),
            ]),
        );
        let active = |name: &str| crate::plugins::ActivePackage {
            name: name.into(),
            version: "1.0.0".into(),
            dependencies: Vec::new(),
        };
        let layer = serde_json::json!({"plugins": {"allow": ["beta"]}});
        let (only_beta, _) = crate::plugins::SessionPlugins::resolve(
            layer.as_object(),
            true,
            &[active("alpha"), active("beta")],
        );

        let narrowed = modules.catalogs_for(&settings, &only_beta);
        assert_eq!(
            narrowed.skills.get_skill_content("guide").as_deref(),
            Some("beta body")
        );
        let explore = |map: &agents::agent_tool::SharedAgentTypeMap| {
            map.read().unwrap()["explore"].source.clone()
        };
        assert_eq!(
            explore(&narrowed.agent_types),
            base::agent_type::AgentTypeSource::Builtin
        );
        let shared = modules.catalogs_for(&settings, &Default::default());
        assert_eq!(
            explore(&shared.agent_types),
            base::agent_type::AgentTypeSource::Plugin {
                plugin: "alpha".into()
            }
        );
        assert!(Arc::ptr_eq(
            &modules.catalogs_for(&settings, &only_beta).skills,
            &narrowed.skills
        ));

        modules.refresh_plugins(
            &settings,
            &contributions(vec![
                package("alpha", &["guide"]),
                package("beta", &["guide", "solo"]),
                package("gamma", &["late"]),
            ]),
        );
        assert!(modules.skills().get("late").is_some());
        assert!(narrowed.skills.get("late").is_none());
        assert!(narrowed.skills.get("solo").is_some());
        assert_eq!(
            explore(&narrowed.agent_types),
            base::agent_type::AgentTypeSource::Builtin
        );

        drop(narrowed);
        assert!(modules.narrowed().is_empty(), "no session holds it");
    }
}
