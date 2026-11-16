//! Skills, as a module: the session's skill catalog and the `Skill` tool.
//!
//! A session assembled with [`SkillsModule`] lists its skills to the model,
//! expands `/<skill>` commands, and can invoke skills through `Skill`.
//! Assemble it after the agents module for `context: fork` skills to fork;
//! without a delegator they run inline.

use std::sync::Arc;

use base::interface::skill_provider::SkillProvider;
use runtime::module::{Module, ModuleError, SessionContext};
use settings::LoadedSettings;

use crate::manager::SkillManager;

pub const MODULE_ID: &str = "skills";

#[derive(Clone)]
pub struct SkillsModule {
    shared: Option<Arc<SkillManager>>,
    providers: Vec<Arc<dyn SkillProvider>>,
    host: Option<Arc<crate::host::HostSkillLayer>>,
}

impl SkillsModule {
    /// Scan each session's own tiers, and watch them.
    pub fn new() -> Self {
        Self {
            shared: None,
            providers: Vec::new(),
            host: None,
        }
    }

    /// Use `catalog` — built and watched once by the host — for every session.
    pub fn shared(catalog: Arc<SkillManager>) -> Self {
        Self {
            shared: Some(catalog),
            providers: Vec::new(),
            host: None,
        }
    }

    /// Merge `layer` — the host's skill directories for one session — over
    /// the catalog, for the session and every delegate it spawns, which are
    /// handed this module whole. Replacing the layer's directories reaches
    /// all of them at once.
    pub fn with_host(mut self, layer: Arc<crate::host::HostSkillLayer>) -> Self {
        self.host = Some(layer);
        self
    }

    /// Add a skill source of the host's own, after the tiers and built-ins: it
    /// fills names nobody claimed unless it declares
    /// [`SkillPrecedence::Override`](base::interface::skill_provider::SkillPrecedence::Override).
    pub fn provider(mut self, provider: Arc<dyn SkillProvider>) -> Self {
        self.providers.push(provider);
        self
    }
}

impl Default for SkillsModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for SkillsModule {
    fn id(&self) -> &'static str {
        MODULE_ID
    }

    fn after(&self) -> &'static [&'static str] {
        &["agents"]
    }

    fn on_session(&self, cx: &mut SessionContext<'_>) -> Result<(), ModuleError> {
        let manager = match &self.shared {
            Some(shared) => shared.clone(),
            None => Arc::new(build_default_skill_manager(cx.settings)),
        };
        for provider in &self.providers {
            let id = provider.id().to_string();
            let count = manager.load_from(provider.clone());
            tracing::info!(source = %id, count, "host skill source registered");
        }
        let admission =
            crate::catalog::BundledAdmission::for_scene(cx.scene.as_ref(), cx.host.attended);
        let mut catalog = crate::catalog::SceneSkills::new(manager, admission);
        if let Some(layer) = &self.host {
            catalog = catalog.with_host(layer.clone());
        }
        let catalog = Arc::new(catalog);
        cx.provide_skill_catalog(catalog.clone());
        crate::tool::register(
            cx.tools,
            catalog,
            cx.delegator(),
            cx.host.permission.grants.clone(),
        );
        Ok(())
    }
}

/// Scan the three skill-directory tiers from disk and register bundled
/// skills — what [`SkillsModule::new`] does per session. A daemon-style host
/// builds this once at the pool level instead and hands it to
/// [`SkillsModule::shared`].
///
/// Scans `~/.agix/skills/` (global default), then
/// `~/.agix/scenes/<scope>/skills/` (scene override — same name wins,
/// `load_dir_subdirs` overwrites by name), then `<project>/.agents/skills/`
/// (project — external fact standard, also scanned by Codex).
/// `load_dir_subdirs` (not the flat-only `load_dir`) so both `<name>.md` and
/// the `<name>/SKILL.md` subdirectory convention (what
/// `discover_for_paths`/`reload_skill` support) are picked up at startup.
///
/// Also starts this manager's own live-reload watcher (`notify` setup
/// failing degrades to no live reload, logged, not a hard error — every
/// other watcher in this codebase handles it the same way).
pub fn build_default_skill_manager(settings: &LoadedSettings) -> SkillManager {
    build_skill_manager(settings, &[], true)
}

/// The default skill manager (without a watcher) with what installed plugin packages ship,
/// between the tiers and the built-ins: a skill on disk wins over a package's
/// of the same name, and a package's over a built-in, the order agent types
/// are merged in.
pub fn build_skill_manager_with_plugins(
    settings: &LoadedSettings,
    plugins: &[Arc<dyn SkillProvider>],
) -> SkillManager {
    build_skill_manager(settings, plugins, false)
}

/// The project's own skills whose body runs a command (`` !`cmd` `` or a
/// ` ```! ` block) or whose `allowed-tools` grant something — what waits for
/// the project to be trusted. As `skills/<name>`, for `project.status`.
pub fn project_skills_requiring_trust(settings: &LoadedSettings) -> Vec<String> {
    use crate::manager::SkillSource;
    let Some(project) = settings.paths.project_root() else {
        return Vec::new();
    };
    let manager = SkillManager::new().reading_with(prompt_files(settings));
    let dir = project.join(".agents").join("skills");
    if manager
        .load_dir_subdirs(&dir, SkillSource::Project)
        .is_err()
    {
        return Vec::new();
    }
    let mut out: Vec<String> = manager
        .list()
        .into_iter()
        .filter(|skill| {
            skill.allowed_tools.as_ref().is_some_and(|t| !t.is_empty())
                || manager
                    .get_skill_content(&skill.name)
                    .is_some_and(|body| crate::tool::runs_commands(&body))
        })
        .map(|skill| format!("skills/{}", skill.name))
        .collect();
    out.sort();
    out
}

fn prompt_files(settings: &LoadedSettings) -> base::paths::PromptFiles {
    base::paths::PromptFiles::for_project(&base::paths::ConfigPaths::from_settings(&settings.paths))
}

fn build_skill_manager(
    settings: &LoadedSettings,
    plugins: &[Arc<dyn SkillProvider>],
    watch: bool,
) -> SkillManager {
    use crate::manager::SkillSource;
    use crate::sources::BundledSkills;

    let defers = settings.trust() == base::permission::ProjectTrust::Untrusted;
    let mut skill_mgr = SkillManager::new().reading_with(prompt_files(settings));
    if defers {
        skill_mgr = skill_mgr.with_project_deferring();
    }
    let project_skills_dir = settings
        .paths
        .project_root()
        .map(|project| project.join(".agents").join("skills"));
    let global_skills = settings.paths.global_data_dir.join("skills");
    let scene_skills = settings.paths.user_data_dir.join("skills");
    let mut user_tiers = vec![(global_skills.clone(), base::skill::SkillTier::Global)];
    if base::path::resolve_physically(&scene_skills)
        != base::path::resolve_physically(&global_skills)
    {
        user_tiers.push((scene_skills, base::skill::SkillTier::Scene));
    }
    let mut loaded_count: usize = user_tiers
        .iter()
        .map(|(dir, scope)| {
            skill_mgr
                .load_dir_subdirs_in(dir, SkillSource::User, Some(*scope))
                .unwrap_or(0)
        })
        .sum();
    // An untrusted project's skills load after everything else and only fill
    // names nobody holds; a trusted project's load before the plugins and
    // built-ins, which defer to it.
    if !defers {
        loaded_count += project_skills_dir
            .iter()
            .map(|dir| skill_mgr.load_project_tier(dir))
            .sum::<usize>();
    }
    for provider in plugins {
        skill_mgr.load_from(provider.clone());
    }
    // Built-ins come last and defer: a project that writes its own skill of
    // the same name means to replace the one in the binary.
    skill_mgr.load_from(Arc::new(BundledSkills));
    if defers {
        loaded_count += project_skills_dir
            .iter()
            .map(|dir| skill_mgr.load_project_tier(dir))
            .sum::<usize>();
    }
    let total_skills = skill_mgr.list().len();
    tracing::info!(loaded_count, total_skills, "skills loaded (incl. bundled)");

    if watch {
        let watched: Vec<_> = user_tiers
            .into_iter()
            .map(|(dir, _)| dir)
            .chain(project_skills_dir)
            .collect();
        if let Err(e) = skill_mgr.enable_watching(&watched) {
            tracing::warn!(error = %e, "failed to enable skill file watching; live skill reload disabled for this session");
        }
    }
    skill_mgr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_skill_that_runs_commands_or_grants_tools_waits_for_trust() {
        let project = tempfile::tempdir().unwrap();
        let skill = |name: &str, frontmatter: &str, body: &str| {
            let dir = project.path().join(".agents/skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name}\n{frontmatter}---\n{body}\n"),
            )
            .unwrap();
        };
        skill("runs", "", "Branch: !`git branch --show-current`");
        skill(
            "grants",
            "allowed-tools: [\"Bash(python3:*)\"]\n",
            "Use python.",
        );
        skill("reads", "", "Just text.");

        let mut settings = LoadedSettings::new(
            settings::Settings::defaults_for("m"),
            base::paths::PathSettings::default(),
        )
        .with_project_layer(settings::trust::ProjectLayer {
            trust: base::permission::ProjectTrust::Untrusted,
            ..Default::default()
        });
        settings.paths.local_data_dir = project.path().join(".agix");
        settings.paths.workspace =
            base::paths::Workspace::project_of(&settings.paths.local_data_dir);

        assert_eq!(
            project_skills_requiring_trust(&settings),
            ["skills/grants", "skills/runs"]
        );
    }

    fn settings_with_both_tiers(
        trust: base::permission::ProjectTrust,
    ) -> (tempfile::TempDir, LoadedSettings) {
        let root = tempfile::tempdir().unwrap();
        let write = |dir: std::path::PathBuf, name: &str, description: &str| {
            let dir = dir.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
            )
            .unwrap();
        };
        let global = root.path().join("global");
        let project = root.path().join("project");
        write(global.join("skills"), "deploy", "the user's");
        write(project.join(".agents/skills"), "deploy", "the repository's");
        write(
            project.join(".agents/skills"),
            "extra",
            "only the repository",
        );
        let mut settings = LoadedSettings::new(
            settings::Settings::defaults_for("m"),
            base::paths::PathSettings::default(),
        )
        .with_project_layer(settings::trust::ProjectLayer {
            trust,
            ..Default::default()
        });
        settings.paths.global_data_dir = global.clone();
        settings.paths.user_data_dir = global.join("scenes/code");
        settings.paths.local_data_dir = project.join(".agix");
        settings.paths.workspace =
            base::paths::Workspace::project_of(&settings.paths.local_data_dir);
        (root, settings)
    }

    #[test]
    fn an_untrusted_projects_skill_adds_names_but_does_not_replace_the_users() {
        let (_root, settings) = settings_with_both_tiers(base::permission::ProjectTrust::Untrusted);
        let manager = build_skill_manager_with_plugins(&settings, &[]);
        assert_eq!(manager.get("deploy").unwrap().description, "the user's");
        assert!(manager.get("extra").is_some(), "its own names still load");

        let (_root, settings) = settings_with_both_tiers(base::permission::ProjectTrust::Trusted);
        let manager = build_skill_manager_with_plugins(&settings, &[]);
        assert_eq!(
            manager.get("deploy").unwrap().description,
            "the repository's"
        );
    }

    fn packaged(plugin: &str, name: &str, body: &str) -> Arc<dyn SkillProvider> {
        let entry = base::skill::SkillEntry {
            name: name.into(),
            description: format!("{plugin}'s {name}"),
            path: std::path::PathBuf::from(format!("/cache/{plugin}/skills/{name}/SKILL.md")),
            ..Default::default()
        };
        Arc::new(crate::sources::PluginSkills::new(
            plugin,
            vec![(entry, body.to_string())],
        ))
    }

    /// Disk over package over built-in, and of two packages the first:
    /// a name ends up with exactly one source, and expanding it gives that
    /// source's text, not the text of a source it beat.
    #[test]
    fn a_disk_skill_wins_over_a_packaged_one_which_wins_over_a_built_in() {
        use crate::manager::SkillSource;
        let home = tempfile::tempdir().unwrap();
        let mut settings = LoadedSettings::new(
            settings::Settings::defaults_for("m"),
            base::paths::PathSettings::default(),
        );
        settings.paths.global_data_dir = home.path().join("global");
        settings.paths.user_data_dir = home.path().join("scene");
        settings.paths.local_data_dir = home.path().join("project/.agix");
        settings.paths.workspace =
            base::paths::Workspace::project_of(&settings.paths.local_data_dir);
        let disk = settings.paths.global_data_dir.join("skills/tour");
        std::fs::create_dir_all(&disk).unwrap();
        std::fs::write(
            disk.join("SKILL.md"),
            "---\ndescription: the user's tour\n---\nDISK BODY\n",
        )
        .unwrap();

        let catalog = build_skill_manager_with_plugins(
            &settings,
            &[
                packaged("alpha", "tour", "ALPHA TOUR"),
                packaged("alpha", "guide", "ALPHA GUIDE"),
                packaged("beta", "guide", "BETA GUIDE"),
                packaged("beta", "simplify", "BETA SIMPLIFY"),
            ],
        );

        let tour = catalog.get("tour").unwrap();
        assert_eq!(tour.source, SkillSource::User);
        assert_eq!(
            catalog.get_skill_content("tour").unwrap().trim(),
            "DISK BODY"
        );

        let guide = catalog.get("guide").unwrap();
        assert_eq!(guide.source, SkillSource::Plugin);
        assert_eq!(guide.description, "alpha's guide");
        assert_eq!(catalog.get_skill_content("guide").unwrap(), "ALPHA GUIDE");

        assert_eq!(catalog.get("simplify").unwrap().source, SkillSource::Plugin);
        assert_eq!(
            catalog.get_skill_content("simplify").unwrap(),
            "BETA SIMPLIFY"
        );
    }
}
