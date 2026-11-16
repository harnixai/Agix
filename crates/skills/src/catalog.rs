//! [`SkillManager`] as the kernel's [`SkillCatalog`]: the turn loop, slash
//! commands and delegates see skills only through this.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base::interface::scene::SceneSpec;
use base::interface::skill_catalog::{SkillCatalog, SkillExpansion};
use base::interface::tool::ToolOrigin;
use base::skill::SkillEntry;
use base::tool::ToolContext;

use crate::host::HostSkillLayer;
use crate::manager::{SkillInfo, SkillManager, SkillSource};

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

pub fn to_entry(skill: &SkillInfo) -> SkillEntry {
    SkillEntry {
        name: skill.name.clone(),
        description: skill.description.clone(),
        when_to_use: skill.when_to_use.clone(),
        source: match skill.source {
            SkillSource::User => base::skill::SkillSource::User,
            SkillSource::Project => base::skill::SkillSource::Project,
            SkillSource::Plugin => base::skill::SkillSource::Plugin,
            SkillSource::Host => base::skill::SkillSource::Host,
        },
        tier: skill.tier,
        path: skill.path.clone(),
        argument_hint: skill.argument_hint.clone(),
        allowed_tools: skill.allowed_tools.clone(),
        disallowed_tools: skill.disallowed_tools.clone(),
        arguments: skill.arguments.clone(),
        model: skill.model.clone(),
        effort: skill.effort.clone(),
        context: skill.context.clone(),
        agent: skill.agent.clone(),
        background: skill.background,
        disable_model_invocation: skill.disable_model_invocation,
        user_invocable: skill.user_invocable,
        paths: skill.paths.clone(),
        version: skill.version.clone(),
        // `SkillInfo` has no counterpart for these two — they are dropped on
        // load, so the catalog cannot give them back. Fields listed
        // exhaustively (no `..Default::default()`) so that adding one to
        // `SkillEntry` fails the build here instead of silently arriving as
        // `None`, which is how `arguments` once went missing and disabled
        // named-argument expansion for every slash command.
        files: None,
        hooks: None,
    }
}

impl SkillCatalog for SkillManager {
    fn list(&self) -> Vec<SkillEntry> {
        SkillManager::list(self).iter().map(to_entry).collect()
    }
    fn get(&self, name: &str) -> Option<SkillEntry> {
        SkillManager::get(self, name).as_ref().map(to_entry)
    }
    fn body(&self, name: &str) -> Option<String> {
        self.get_skill_content(name)
    }
    fn expand<'a>(
        &'a self,
        name: &'a str,
        args: &'a str,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Option<Result<SkillExpansion, String>>> {
        Box::pin(async move {
            let info = SkillManager::get(self, name)?;
            match crate::tool::expand_for_invocation(self, &info, args, ctx).await {
                Ok(expansion) => {
                    self.record_invocation(name);
                    Some(Ok(expansion))
                }
                Err(crate::tool::ExpandError::NotFound) => None,
                Err(crate::tool::ExpandError::Rejected(msg)) => Some(Err(msg)),
            }
        })
    }
    fn reload_changed(&self) -> usize {
        self.check_for_changes()
    }
    fn discover_for_paths(&self, paths: &[PathBuf]) -> Vec<SkillEntry> {
        SkillManager::discover_for_paths(self, paths)
            .iter()
            .map(to_entry)
            .collect()
    }
    fn activate_for_paths(&self, files: &[PathBuf]) -> Vec<SkillEntry> {
        self.activate_conditional_skills_for_paths(files)
            .iter()
            .map(to_entry)
            .collect()
    }
    fn invocation_count(&self, name: &str) -> u32 {
        SkillManager::invocation_count(self, name)
    }
    fn last_invoked_seq(&self, name: &str) -> u64 {
        SkillManager::last_invoked_seq(self, name)
    }
}

/// Which built-in skills a session lists: the ones its scene names (with its
/// `attended_skills` when someone attends the session), less any whose
/// required tools the scene does not permit. Skills from disk and host
/// providers always pass, so a project's own `loop` still shows in a scene
/// that leaves the built-in one out.
#[derive(Clone, Default)]
pub struct BundledAdmission(Option<Arc<HashSet<String>>>);

impl BundledAdmission {
    pub fn for_scene(scene: &SceneSpec, attended: bool) -> Self {
        let attended_skills = scene.attended_skills.iter().filter(|_| attended);
        let names = scene
            .bundled_skills
            .iter()
            .chain(attended_skills)
            .filter(|name| {
                crate::bundled::required_tools(name)
                    .iter()
                    .all(|tool| scene.permits_tool(tool, &ToolOrigin::Builtin))
            })
            .cloned()
            .collect();
        Self(Some(Arc::new(names)))
    }

    pub fn admits(&self, name: &str, path: &Path) -> bool {
        match &self.0 {
            None => true,
            Some(names) => !path.to_string_lossy().starts_with("(bundled:") || names.contains(name),
        }
    }
}

/// A [`SkillManager`] as one session sees it: its scene's built-ins (see
/// [`BundledAdmission`]), with the host's skill directories for the session
/// merged over it (see [`crate::host`]).
pub struct SceneSkills {
    manager: Arc<SkillManager>,
    admission: BundledAdmission,
    host: Option<Arc<HostSkillLayer>>,
}

impl SceneSkills {
    pub fn new(manager: Arc<SkillManager>, admission: BundledAdmission) -> Self {
        Self {
            manager,
            admission,
            host: None,
        }
    }

    pub fn with_host(mut self, host: Arc<HostSkillLayer>) -> Self {
        self.host = Some(host);
        self
    }

    /// The manager that answers for `name` in this session, and its entry
    /// there: its body, `files:` and grants all come from that one.
    pub fn resolve(&self, name: &str) -> Option<(Arc<SkillManager>, SkillInfo)> {
        crate::host::winner(&self.manager, self.host.as_deref(), name)
            .filter(|(_, info)| self.admission.admits(&info.name, &info.path))
    }

    /// Those of `entries` that are the one the session has under their name.
    fn won(&self, entries: Vec<SkillEntry>) -> Vec<SkillEntry> {
        entries
            .into_iter()
            .filter(|e| self.resolve(&e.name).is_some_and(|(_, w)| w.path == e.path))
            .collect()
    }
}

impl SkillCatalog for SceneSkills {
    fn list(&self) -> Vec<SkillEntry> {
        let mut names: Vec<String> = self.manager.list().into_iter().map(|s| s.name).collect();
        if let Some(host) = &self.host {
            names.extend(host.names());
        }
        names.sort();
        names.dedup();
        names
            .iter()
            .filter_map(|name| self.resolve(name).map(|(_, info)| to_entry(&info)))
            .collect()
    }
    fn get(&self, name: &str) -> Option<SkillEntry> {
        self.resolve(name).map(|(_, info)| to_entry(&info))
    }
    fn body(&self, name: &str) -> Option<String> {
        self.resolve(name)?.0.get_skill_content(name)
    }
    fn expand<'a>(
        &'a self,
        name: &'a str,
        args: &'a str,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Option<Result<SkillExpansion, String>>> {
        Box::pin(async move {
            let (manager, _) = self.resolve(name)?;
            manager.expand(name, args, ctx).await
        })
    }
    fn reload_changed(&self) -> usize {
        self.manager.check_for_changes()
    }
    fn discover_for_paths(&self, paths: &[PathBuf]) -> Vec<SkillEntry> {
        SkillCatalog::discover_for_paths(&*self.manager, paths)
            .into_iter()
            .filter(|e| self.admission.admits(&e.name, &e.path))
            .collect()
    }
    fn activate_for_paths(&self, files: &[PathBuf]) -> Vec<SkillEntry> {
        let mut found = SkillCatalog::activate_for_paths(&*self.manager, files);
        for dir in self.host.iter().flat_map(|h| h.dirs()) {
            let manager = dir.manager();
            found.extend(SkillCatalog::activate_for_paths(&*manager, files));
        }
        self.won(found)
    }
    fn invocation_count(&self, name: &str) -> u32 {
        self.resolve(name)
            .map_or(0, |(manager, _)| manager.invocation_count(name))
    }
    fn last_invoked_seq(&self, name: &str) -> u64 {
        self.resolve(name)
            .map_or(0, |(manager, _)| manager.last_invoked_seq(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::scene::{FixedPrompt, ToolSurface};

    #[allow(non_snake_case)]
    fn Picky(bundled: Vec<&str>, tools: Vec<&str>) -> SceneSpec {
        let mut s = SceneSpec::new("picky", "picky", Arc::new(FixedPrompt(vec![])));
        s.tools = ToolSurface::from_lists(&tools, &[] as &[&str]);
        s.bundled_skills = bundled.into_iter().map(String::from).collect();
        s
    }

    fn manager() -> Arc<SkillManager> {
        let mgr = SkillManager::new();
        mgr.load_from(Arc::new(crate::sources::BundledSkills::new(
            Default::default(),
            PathBuf::from("/state"),
        )));
        Arc::new(mgr)
    }

    fn names(catalog: &SceneSkills) -> Vec<String> {
        let mut names: Vec<_> = SkillCatalog::list(catalog)
            .into_iter()
            .map(|e| e.name)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_scene_lists_the_built_ins_it_names_and_can_run() {
        let scene = Picky(vec!["skillify", "loop", "debug"], vec!["Read", "Write"]);
        let catalog = SceneSkills::new(manager(), BundledAdmission::for_scene(&scene, false));
        assert_eq!(
            names(&catalog),
            ["debug", "skillify"],
            "loop needs CronCreate"
        );
        assert!(SkillCatalog::get(&catalog, "loop").is_none());
        assert!(SkillCatalog::body(&catalog, "loop").is_none());
        assert!(SkillCatalog::body(&catalog, "skillify").is_some());
    }

    #[test]
    fn the_attended_skills_are_listed_only_for_an_attended_session() {
        let mut scene = Picky(vec!["skillify"], vec![]);
        scene.attended_skills = vec!["remember".into()];
        let attended = SceneSkills::new(manager(), BundledAdmission::for_scene(&scene, true));
        assert_eq!(names(&attended), ["remember", "skillify"]);
        let unattended = SceneSkills::new(manager(), BundledAdmission::for_scene(&scene, false));
        assert_eq!(names(&unattended), ["skillify"]);
    }

    #[test]
    fn skills_from_elsewhere_pass_whatever_the_scene_names() {
        let mgr = manager();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("loop.md");
        std::fs::write(&file, "---\nname: loop\ndescription: mine\n---\nbody").unwrap();
        mgr.reload_skill(&file).unwrap();
        let catalog = SceneSkills::new(
            mgr,
            BundledAdmission::for_scene(&Picky(vec![], vec![]), true),
        );
        assert_eq!(names(&catalog), ["loop"]);
    }
}
