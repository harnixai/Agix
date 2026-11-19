//! 宿主按会话给的 skill 目录（`options.skill_dirs`）在池里的一份：按解析后的路径共享的
//! 目录（每个读一次、存在时监视一次，持有它的会话都离开内存后放掉）、只增不减的受保护
//! 集合、对一组目录的校验，以及一个会话的那一层 `SessionSkillDirs`。没有 skills 模块
//! 的构建不收任何目录。

use crate::session_pool::api::{InvalidOption, SkillDirOption};
use crate::watch_hub::WatchHub;
use base::context::LiveDirs;
use settings::LoadedSettings;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

pub(in crate::session_pool) const MAX_SKILL_DIRS: usize = 64;

/// A directory that passed the checks: as given, and where it resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session_pool) struct AcceptedDir {
    given: PathBuf,
    real: PathBuf,
}

/// One session's host skill directories: the layer its skill catalogs merge,
/// and the ones its tools read without asking. Both are replaced in place, so
/// its engine and delegates see a change without a rebuild.
#[derive(Default)]
pub(crate) struct SessionSkillDirs {
    #[cfg(feature = "skills")]
    layer: std::sync::Arc<skills::host::HostSkillLayer>,
    readable: LiveDirs,
    /// Recorded directories left out when the session was brought back, as
    /// recorded.
    refused: Mutex<Vec<String>>,
}

impl SessionSkillDirs {
    #[cfg(feature = "skills")]
    pub(crate) fn layer(&self) -> &std::sync::Arc<skills::host::HostSkillLayer> {
        &self.layer
    }

    pub(in crate::session_pool) fn readable(&self) -> &LiveDirs {
        &self.readable
    }

    pub(in crate::session_pool) fn refused(&self) -> Vec<String> {
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Every host skill directory a session in memory holds, by where it
/// resolves, and every one any session has listed.
#[derive(Default)]
pub(in crate::session_pool) struct HostDirTable {
    #[cfg(feature = "skills")]
    dirs: Mutex<std::collections::HashMap<PathBuf, std::sync::Weak<skills::host::HostSkillDir>>>,
    /// Only grows: a session out of memory keeps a directory protected.
    protected: LiveDirs,
}

impl HostDirTable {
    pub(in crate::session_pool) fn protected(&self) -> &LiveDirs {
        &self.protected
    }

    /// `dirs` as a session on `settings` may list them, else every entry that
    /// may not be, by field.
    pub(in crate::session_pool) fn check(
        dirs: &[SkillDirOption],
        settings: &LoadedSettings,
    ) -> Result<Vec<AcceptedDir>, Vec<InvalidOption>> {
        let (accepted, refused) = Self::sort(dirs, settings);
        if refused.is_empty() {
            Ok(accepted)
        } else {
            Err(refused)
        }
    }

    /// What of `dirs` passes, and why each of the rest does not.
    pub(in crate::session_pool) fn sort(
        dirs: &[SkillDirOption],
        settings: &LoadedSettings,
    ) -> (Vec<AcceptedDir>, Vec<InvalidOption>) {
        let mut accepted: Vec<(usize, AcceptedDir)> = Vec::new();
        let mut refused = Vec::new();
        if dirs.is_empty() {
            return (Vec::new(), refused);
        }
        if !cfg!(feature = "skills") {
            refused.push(InvalidOption::new(
                "skill_dirs",
                "this build does not include the skills module",
            ));
            return (Vec::new(), refused);
        }
        if dirs.len() > MAX_SKILL_DIRS {
            refused.push(InvalidOption::new(
                "skill_dirs",
                format!("at most {MAX_SKILL_DIRS} skill directories"),
            ));
            return (Vec::new(), refused);
        }
        let forbidden = Forbidden::of(settings);
        for (index, dir) in dirs.iter().enumerate() {
            let field = format!("skill_dirs[{index}].path");
            let checked = forbidden.check(Path::new(&dir.path)).and_then(|dir| {
                match accepted
                    .iter()
                    .find(|(_, earlier)| earlier.real == dir.real)
                {
                    Some((earlier, _)) => {
                        Err(format!("is the same directory as skill_dirs[{earlier}]"))
                    }
                    None => Ok(dir),
                }
            });
            match checked {
                Ok(dir) => accepted.push((index, dir)),
                Err(error) => refused.push(InvalidOption::new(field, error)),
            }
        }
        (accepted.into_iter().map(|(_, d)| d).collect(), refused)
    }

    /// Give `session` the directories `accepted`, reading the ones no session
    /// in memory holds yet and watching those that exist; `refused` are the
    /// recorded ones left out. The directories are protected from then on.
    pub(in crate::session_pool) fn apply(
        &self,
        session: &SessionSkillDirs,
        accepted: &[AcceptedDir],
        refused: Vec<String>,
        settings: &LoadedSettings,
        hub: Option<&WatchHub>,
    ) {
        let spellings = || {
            accepted
                .iter()
                .flat_map(|d| [d.given.clone(), d.real.clone()])
        };
        self.protected.extend(spellings());
        #[cfg(feature = "skills")]
        {
            let sensitive = base::paths::SensitivePaths::new(
                Some(&settings.paths.global_data_dir),
                None,
                settings.paths.layout.clone(),
            );
            let dirs = accepted
                .iter()
                .map(|dir| self.open(dir, &sensitive, hub))
                .collect();
            session.layer.replace(dirs);
        }
        #[cfg(not(feature = "skills"))]
        let _ = (settings, hub);
        session.readable.set(spellings().collect());
        *session
            .refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = refused;
    }

    #[cfg(feature = "skills")]
    fn open(
        &self,
        dir: &AcceptedDir,
        sensitive: &base::paths::SensitivePaths,
        hub: Option<&WatchHub>,
    ) -> std::sync::Arc<skills::host::HostSkillDir> {
        if let Some(held) = self.held(&dir.real) {
            return held;
        }
        let opened = std::sync::Arc::new(skills::host::HostSkillDir::open(
            dir.given.clone(),
            sensitive.clone(),
        ));
        let mut dirs = self.dirs.lock().unwrap_or_else(PoisonError::into_inner);
        dirs.retain(|_, held| held.strong_count() > 0);
        if let Some(raced) = dirs.get(&dir.real).and_then(std::sync::Weak::upgrade) {
            return raced;
        }
        dirs.insert(dir.real.clone(), std::sync::Arc::downgrade(&opened));
        drop(dirs);
        if opened.present() {
            watch(&opened, &dir.real, hub);
        }
        opened
    }

    #[cfg(feature = "skills")]
    fn held(&self, real: &Path) -> Option<std::sync::Arc<skills::host::HostSkillDir>> {
        self.dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(real)
            .and_then(std::sync::Weak::upgrade)
    }

    /// Read the directory that resolves to `real` again, for every session
    /// holding it.
    pub(in crate::session_pool) fn rescan(&self, real: &Path) {
        #[cfg(feature = "skills")]
        if let Some(dir) = self.held(real) {
            dir.rescan();
        }
        #[cfg(not(feature = "skills"))]
        let _ = real;
    }

    /// Read and watch those of `session`'s directories that were missing and
    /// are there now. One that resolves somewhere other than where it did
    /// when it was listed — it appeared as a link — is found, protected and
    /// readable there too.
    pub(in crate::session_pool) fn take_up_appeared(
        &self,
        session: &SessionSkillDirs,
        hub: Option<&WatchHub>,
    ) {
        #[cfg(feature = "skills")]
        for dir in session.layer.dirs() {
            let appeared = dir.appeared();
            if !dir.present() {
                continue;
            }
            let real = base::path::resolve_physically(dir.path());
            self.hold_at(&real, &dir);
            if appeared && !dir.is_watched() {
                watch(&dir, &real, hub);
            }
            self.protected.extend([real.clone()]);
            session.readable.extend([real]);
        }
        #[cfg(not(feature = "skills"))]
        let _ = (session, hub);
    }

    /// Have `dir` found under `real` unless another directory held is.
    #[cfg(feature = "skills")]
    fn hold_at(&self, real: &Path, dir: &std::sync::Arc<skills::host::HostSkillDir>) {
        let mut dirs = self.dirs.lock().unwrap_or_else(PoisonError::into_inner);
        if dirs.get(real).and_then(std::sync::Weak::upgrade).is_none() {
            dirs.insert(real.to_path_buf(), std::sync::Arc::downgrade(dir));
        }
    }
}

/// `session.get`'s report of `session`'s directories, `recorded` in order,
/// over what `modules` shares on its pair.
pub(in crate::session_pool) fn report_skill_dirs(
    recorded: &[SkillDirOption],
    session: &SessionSkillDirs,
    modules: &crate::modules::ProjectModules,
) -> Vec<crate::session_pool::api::SkillDirReport> {
    use crate::session_pool::api::{SkillDirReport, SkillDirState};
    let refused = session.refused();
    #[cfg(feature = "skills")]
    let mut reports = modules.skill_dir_reports(session).into_iter();
    #[cfg(not(feature = "skills"))]
    let _ = modules;
    recorded
        .iter()
        .map(|dir| {
            if refused.contains(&dir.path) {
                return SkillDirReport {
                    path: dir.path.clone(),
                    state: Some(SkillDirState::Refused),
                    skills: Some(Vec::new()),
                    shadowed: Some(Vec::new()),
                };
            }
            #[cfg(feature = "skills")]
            if let Some(report) = reports.next() {
                return SkillDirReport {
                    path: dir.path.clone(),
                    state: Some(if report.present {
                        SkillDirState::Ok
                    } else {
                        SkillDirState::Missing
                    }),
                    skills: Some(report.skills),
                    shadowed: Some(
                        report
                            .shadowed
                            .into_iter()
                            .map(|s| crate::session_pool::api::ShadowedSkill {
                                skill: s.skill,
                                by: match s.by {
                                    skills::manager::SkillSource::User => {
                                        crate::session_pool::api::SkillOrigin::User
                                    }
                                    skills::manager::SkillSource::Project => {
                                        crate::session_pool::api::SkillOrigin::Project
                                    }
                                    skills::manager::SkillSource::Plugin => {
                                        crate::session_pool::api::SkillOrigin::Plugin
                                    }
                                    skills::manager::SkillSource::Host => {
                                        crate::session_pool::api::SkillOrigin::Host
                                    }
                                },
                                path: s.path.display().to_string(),
                            })
                            .collect(),
                    ),
                };
            }
            SkillDirReport {
                path: dir.path.clone(),
                state: None,
                skills: None,
                shadowed: None,
            }
        })
        .collect()
}

#[cfg(feature = "skills")]
fn watch(dir: &skills::host::HostSkillDir, real: &Path, hub: Option<&WatchHub>) {
    use crate::watch_hub::{Tier, WatchKey, WatchTarget};
    let Some(hub) = hub else {
        return;
    };
    let token = hub.subscribe(
        WatchKey {
            tier: Tier::Host(real.to_path_buf()),
            target: WatchTarget::Skills,
        },
        real,
    );
    dir.keep_watched(Box::new(token));
}

/// Where a session's host skill directory may not be, and why: what would
/// make one file two sources, what the session or its sandboxed commands may
/// write, and credentials.
struct Forbidden {
    roots: Vec<(PathBuf, &'static str)>,
    sensitive: base::paths::SensitivePaths,
    credential_stores: Vec<PathBuf>,
}

impl Forbidden {
    fn of(settings: &LoadedSettings) -> Self {
        let paths = &settings.paths;
        let config = paths.config_paths();
        let sensitive = base::paths::SensitivePaths::for_paths(&config);
        let cwd = paths.working_dir().to_path_buf();
        let mut roots = vec![
            (config.global_skills_dir(), "the global skill tier"),
            (config.state_root().scenes_dir(), "the scene tiers"),
            (config.user_skills_dir(), "the scene's skill tier"),
            (cwd.clone(), "the directory the session works in"),
        ];
        if let Some(project) = paths.project_root() {
            roots.push((project.to_path_buf(), "the session's project"));
        }
        roots.extend(
            Self::sandbox_writable(settings, &sensitive, &cwd)
                .into_iter()
                .map(|dir| (dir, "a directory the session's commands write")),
        );
        let sandbox = &settings.values.sandbox;
        let credential_stores = base::interface::exec::default_deny_read()
            .into_iter()
            .chain(sandbox.deny_read.iter().cloned())
            .collect();
        Self {
            roots,
            sensitive,
            credential_stores,
        }
    }

    /// Every root a sandboxed command of a session on `settings` may write
    /// but its working directory: as the session's sandbox policy is made,
    /// with all of `<state>/tmp` for the session's own `TMPDIR` under it.
    fn sandbox_writable(
        settings: &LoadedSettings,
        sensitive: &base::paths::SensitivePaths,
        cwd: &Path,
    ) -> Vec<PathBuf> {
        let values = &settings.values;
        let env = base::context::SessionEnv {
            additional_writable_dirs: values
                .files
                .extra_dirs
                .iter()
                .map(|dir| cwd.join(dir))
                .chain(sensitive.state_working_dirs())
                .collect(),
            cache_directories: values.sandbox.cache_dirs.paths(cwd),
            ..base::context::SessionEnv::new(cwd.to_path_buf())
        };
        let session_tmp = sensitive
            .state_root()
            .map(|root| root.join(base::paths::STATE_ROOT_SESSION_TMP))
            .unwrap_or_else(std::env::temp_dir);
        tools::bash::writable_dirs(&env, session_tmp)
            .into_iter()
            .chain(platform::sandbox::implicit_writable_roots(false))
            .collect()
    }

    fn check(&self, path: &Path) -> Result<AcceptedDir, String> {
        if !path.is_absolute() {
            return Err("must be an absolute path".into());
        }
        let given = base::path::normalize_lexically(path);
        if given.exists() && !given.is_dir() {
            return Err("exists and is not a directory".into());
        }
        let real = base::path::resolve_physically(&given);
        if self.sensitive.is_credential(&given) || self.sensitive.is_credential(&real) {
            return Err("is a credential location".into());
        }
        for (root, what) in &self.roots {
            let root_real = base::path::resolve_physically(root);
            let inside = |p: &Path| p.starts_with(root) || p.starts_with(&root_real);
            if inside(&given) || inside(&real) {
                return Err(format!("is inside {what} ({})", root.display()));
            }
            if root.starts_with(&given) || root_real.starts_with(&real) {
                return Err(format!("holds {what} ({})", root.display()));
            }
        }
        if let Some(store) = self.credential_stores.iter().find(|store| {
            let store_real = base::path::resolve_physically(store);
            store.starts_with(&given) || store_real.starts_with(&real)
        }) {
            return Err(format!("holds a credential location ({})", store.display()));
        }
        Ok(AcceptedDir { given, real })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_in(root: &Path) -> LoadedSettings {
        let mut settings = LoadedSettings::new(
            settings::Settings::defaults_for("m"),
            base::paths::PathSettings::default(),
        );
        settings.paths.global_data_dir = root.join("state");
        settings.paths.user_data_dir = root.join("state/scenes/code");
        settings.paths.local_data_dir = root.join("project/.agix");
        settings.paths.workspace =
            base::paths::Workspace::project_of(&settings.paths.local_data_dir);
        settings
    }

    /// A directory outside the OS temporary directory, where a sandboxed
    /// command could write and a host skill directory may therefore not be.
    #[cfg(feature = "skills")]
    fn scratch() -> tempfile::TempDir {
        let exe = std::env::current_exe().unwrap();
        let target = exe.ancestors().nth(3).unwrap().join("tmp");
        std::fs::create_dir_all(&target).unwrap();
        tempfile::tempdir_in(target).unwrap()
    }

    fn dirs(paths: &[&str]) -> Vec<SkillDirOption> {
        paths
            .iter()
            .map(|p| SkillDirOption {
                path: p.to_string(),
            })
            .collect()
    }

    #[cfg(feature = "skills")]
    #[test]
    fn every_entry_that_may_not_be_listed_is_named() {
        let root = scratch();
        let root = root.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("project")).unwrap();
        std::fs::create_dir_all(root.join("ext/a")).unwrap();
        std::fs::write(root.join("file"), "x").unwrap();
        let settings = settings_in(&root);
        let at = |p: &str| root.join(p).display().to_string();
        let listed = dirs(&[
            &at("ext/a"),
            "relative/skills",
            &at("file"),
            &at("ext/a/"),
            &at("state/skills"),
            &at("project/skills"),
            &at("missing"),
            &at("state/scratchpad/x"),
            "/",
        ]);
        let refused = HostDirTable::check(&listed, &settings).unwrap_err();
        let fields: Vec<&str> = refused.iter().map(|r| r.field.as_str()).collect();
        assert_eq!(
            fields,
            [
                "skill_dirs[1].path",
                "skill_dirs[2].path",
                "skill_dirs[3].path",
                "skill_dirs[4].path",
                "skill_dirs[5].path",
                "skill_dirs[7].path",
                "skill_dirs[8].path",
            ],
            "{refused:?}"
        );
        assert!(refused[2].error.contains("skill_dirs[0]"), "{refused:?}");

        let (accepted, _) = HostDirTable::sort(&listed, &settings);
        assert_eq!(accepted.len(), 2, "{accepted:?}");
    }

    #[cfg(feature = "skills")]
    fn skill(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(
            dir.join(name).join("SKILL.md"),
            format!("---\ndescription: {name}\n---\n{name} body\n"),
        )
        .unwrap();
    }

    /// `ext/hs` listed while missing, then made a link to `elsewhere`, and
    /// taken up as before a turn.
    #[cfg(all(feature = "skills", unix))]
    fn appeared_as_a_link(root: &Path) -> (HostDirTable, SessionSkillDirs, PathBuf, PathBuf) {
        std::fs::create_dir_all(root.join("project")).unwrap();
        std::fs::create_dir_all(root.join("ext")).unwrap();
        let settings = settings_in(root);
        let listed = root.join("ext/hs");
        let target = root.join("elsewhere");
        let accepted =
            HostDirTable::check(&dirs(&[&listed.display().to_string()]), &settings).unwrap();
        let table = HostDirTable::default();
        let session = SessionSkillDirs::default();
        table.apply(&session, &accepted, Vec::new(), &settings, None);
        skill(&target, "first");
        std::os::unix::fs::symlink(&target, &listed).unwrap();
        table.take_up_appeared(&session, None);
        (table, session, listed, target)
    }

    #[cfg(all(feature = "skills", unix))]
    #[test]
    fn a_dir_that_appears_as_a_link_is_read_again_when_it_changes() {
        let scratch = scratch();
        let root = scratch.path().canonicalize().unwrap();
        let (table, session, listed, target) = appeared_as_a_link(&root);
        let shared = skills::manager::SkillManager::new();
        assert_eq!(session.layer().report(&shared)[0].skills, ["first"]);

        skill(&target, "second");
        table.rescan(&base::path::resolve_physically(&listed));
        assert_eq!(
            session.layer().report(&shared)[0].skills,
            ["first", "second"]
        );
    }

    #[cfg(all(feature = "skills", unix))]
    #[test]
    fn a_dir_that_appears_as_a_link_is_protected_and_readable_where_it_resolves() {
        let scratch = scratch();
        let root = scratch.path().canonicalize().unwrap();
        let (table, session, _, target) = appeared_as_a_link(&root);
        let inside = target.join("tour/SKILL.md");
        assert!(
            table.protected().holding(&inside).is_some(),
            "{:?}",
            table.protected().get()
        );
        assert!(
            session.readable().holding(&inside).is_some(),
            "{:?}",
            session.readable().get()
        );
    }

    #[cfg(feature = "skills")]
    #[test]
    fn a_dir_holding_a_credential_location_is_refused() {
        let scratch = scratch();
        let root = scratch.path().canonicalize().unwrap();
        let mut settings = settings_in(&root);
        settings.values.sandbox.deny_read = vec![root.join("ext/vault/secret")];
        let built_in = base::interface::exec::default_deny_read_in(&root.join("home"));
        let holding_one = built_in
            .iter()
            .find(|store| store.ends_with(".config/gh"))
            .and_then(|store| store.parent())
            .unwrap()
            .to_path_buf();
        settings.values.sandbox.deny_read.extend(built_in);
        let listed = [root.join("ext/vault"), holding_one];
        let listed: Vec<String> = listed.iter().map(|p| p.display().to_string()).collect();
        let listed: Vec<&str> = listed.iter().map(String::as_str).collect();
        let refused = HostDirTable::check(&dirs(&listed), &settings).unwrap_err();
        assert_eq!(refused.len(), listed.len(), "{refused:?}");
        assert!(
            refused.iter().all(|r| r.error.contains("credential")),
            "{refused:?}"
        );
    }

    #[cfg(feature = "skills")]
    #[test]
    fn a_dir_a_sandboxed_command_may_write_is_refused() {
        let scratch = scratch();
        let root = scratch.path().canonicalize().unwrap();
        let mut settings = settings_in(&root);
        settings.values.files.extra_dirs = vec![root.join("extra")];
        let in_temp = tempfile::tempdir().unwrap();
        let mut listed = vec![
            root.join("extra/skills").display().to_string(),
            root.join("state/tmp/a-session/skills")
                .display()
                .to_string(),
            "/dev/skills".to_string(),
        ];
        if cfg!(target_os = "macos") {
            listed.push(in_temp.path().display().to_string());
        }
        settings.values.sandbox.cache_dirs.0.insert(
            "toolchain".into(),
            vec![root.join("cache").display().to_string()],
        );
        listed.push(root.join("cache/skills").display().to_string());
        let listed: Vec<&str> = listed.iter().map(String::as_str).collect();
        let refused = HostDirTable::check(&dirs(&listed), &settings).unwrap_err();
        assert_eq!(refused.len(), listed.len(), "{refused:?}");
    }

    #[test]
    fn more_than_the_limit_is_refused_whole() {
        let root = tempfile::tempdir().unwrap();
        let settings = settings_in(root.path());
        let many: Vec<String> = (0..=MAX_SKILL_DIRS)
            .map(|i| format!("/elsewhere/{i}"))
            .collect();
        let listed = dirs(&many.iter().map(String::as_str).collect::<Vec<_>>());
        let refused = HostDirTable::check(&listed, &settings).unwrap_err();
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].field, "skill_dirs");
    }
}
