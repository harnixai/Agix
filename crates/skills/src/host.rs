//! The skill directories a host gives one session (`options.skill_dirs`):
//! [`HostSkillDir`] is one directory read into its own [`SkillManager`], shared
//! by every session that lists it; [`HostSkillLayer`] is one session's ordered
//! list of them, replaced in place, which `SceneSkills` merges over the
//! binding's shared catalog by [`SkillManager::yields_to_host`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use base::interface::skill_provider::{SkillPrecedence, SkillProvider};
use base::paths::{PromptFiles, SensitivePaths};
use base::skill::SkillEntry;

use crate::manager::{SkillInfo, SkillManager, SkillSource};
use crate::sources::SkillDirectory;

/// A name the MCP adapters make; a directory does not get to claim one.
const MCP_PREFIX: &str = "mcp__";

/// One host skill directory, as read last.
pub struct HostSkillDir {
    path: PathBuf,
    sensitive: SensitivePaths,
    /// Swapped whole on a rescan, not refilled: it reads through a root
    /// resolved when it was made, and a directory missing then resolves
    /// differently once it is there.
    manager: RwLock<Arc<SkillManager>>,
    present: AtomicBool,
    /// Whatever keeps the directory watched; dropped with it.
    watch: Mutex<Option<Box<dyn std::any::Any + Send + Sync>>>,
}

impl HostSkillDir {
    /// `path` read as `sensitive` reads a directory for the model, with
    /// `path` as the root: a credential, or a skill directory, `SKILL.md` or
    /// `files:` entry that resolves outside `path`, is not read.
    pub fn open(path: PathBuf, sensitive: SensitivePaths) -> Self {
        let present = path.is_dir();
        let manager = Arc::new(scan(&path, &sensitive));
        Self {
            path,
            sensitive,
            manager: RwLock::new(manager),
            present: AtomicBool::new(present),
            watch: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn present(&self) -> bool {
        self.present.load(Ordering::Acquire)
    }

    /// Read the directory again, for every session holding it.
    pub fn rescan(&self) {
        let fresh = Arc::new(scan(&self.path, &self.sensitive));
        *self.manager.write().unwrap_or_else(PoisonError::into_inner) = fresh;
        self.present.store(self.path.is_dir(), Ordering::Release);
    }

    /// Whether the directory was missing and is there now; read if so.
    pub fn appeared(&self) -> bool {
        if self.present() || !self.path.is_dir() {
            return false;
        }
        self.rescan();
        true
    }

    /// Keep `watch` for as long as this directory is held.
    pub fn keep_watched(&self, watch: Box<dyn std::any::Any + Send + Sync>) {
        *self.watch.lock().unwrap_or_else(PoisonError::into_inner) = Some(watch);
    }

    pub fn is_watched(&self) -> bool {
        self.watch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    pub(crate) fn manager(&self) -> Arc<SkillManager> {
        self.manager
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn names(&self) -> Vec<String> {
        self.manager().list().into_iter().map(|s| s.name).collect()
    }
}

/// The root is resolved now: a directory that did not exist when it was
/// first read may resolve through a link once it does.
fn scan(path: &Path, sensitive: &SensitivePaths) -> SkillManager {
    let files: PromptFiles = sensitive.prompt_files_within(path);
    let manager = SkillManager::new().reading_with(files.clone());
    manager.load_from(Arc::new(WithoutMcpNames(
        SkillDirectory::tier(path, SkillSource::Host).reading_with(files),
    )));
    manager
}

struct WithoutMcpNames(SkillDirectory);

impl SkillProvider for WithoutMcpNames {
    fn id(&self) -> &str {
        self.0.id()
    }

    fn precedence(&self) -> SkillPrecedence {
        self.0.precedence()
    }

    fn skills(&self) -> Vec<SkillEntry> {
        let (kept, refused): (Vec<_>, Vec<_>) = self
            .0
            .skills()
            .into_iter()
            .partition(|s| !s.name.starts_with(MCP_PREFIX));
        for skill in refused {
            tracing::warn!(
                skill = %skill.name,
                path = %skill.path.display(),
                "not reading a host skill named like an MCP tool"
            );
        }
        kept
    }
}

/// One session's host skill directories, in the order listed: the first one
/// that has a name wins it among them.
#[derive(Default)]
pub struct HostSkillLayer {
    dirs: RwLock<Vec<Arc<HostSkillDir>>>,
}

impl HostSkillLayer {
    pub fn new(dirs: Vec<Arc<HostSkillDir>>) -> Self {
        Self {
            dirs: RwLock::new(dirs),
        }
    }

    pub fn dirs(&self) -> Vec<Arc<HostSkillDir>> {
        self.dirs
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Everything holding this layer sees `dirs` from now on.
    pub fn replace(&self, dirs: Vec<Arc<HostSkillDir>>) {
        *self.dirs.write().unwrap_or_else(PoisonError::into_inner) = dirs;
    }

    pub fn is_empty(&self) -> bool {
        self.dirs
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }

    fn first_with(&self, name: &str) -> Option<(Arc<SkillManager>, SkillInfo)> {
        self.dirs().iter().find_map(|dir| {
            let manager = dir.manager();
            let info = manager.get(name)?;
            Some((manager, info))
        })
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.dirs().iter().flat_map(|d| d.names()).collect()
    }

    /// Each directory: what it wins in a session over `shared`, and what of
    /// it something else holds there.
    pub fn report(&self, shared: &SkillManager) -> Vec<HostDirReport> {
        let dirs = self.dirs();
        dirs.iter()
            .enumerate()
            .map(|(index, dir)| {
                let mut skills = Vec::new();
                let mut shadowed = Vec::new();
                for info in dir.manager().list() {
                    match shared.get(&info.name).filter(|s| !shared.yields_to_host(s)) {
                        Some(held) => shadowed.push(Shadowed {
                            skill: info.name,
                            by: held.source,
                            path: held.path,
                        }),
                        None => match dirs[..index]
                            .iter()
                            .find_map(|earlier| earlier.manager().get(&info.name))
                        {
                            Some(held) => shadowed.push(Shadowed {
                                skill: info.name,
                                by: SkillSource::Host,
                                path: held.path,
                            }),
                            None => skills.push(info.name),
                        },
                    }
                }
                HostDirReport {
                    path: dir.path().to_path_buf(),
                    present: dir.present(),
                    skills,
                    shadowed,
                }
            })
            .collect()
    }
}

/// What one host skill directory gives a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDirReport {
    pub path: PathBuf,
    pub present: bool,
    /// The names this directory wins, sorted.
    pub skills: Vec<String>,
    pub shadowed: Vec<Shadowed>,
}

/// A skill of a host directory that another source holds in the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shadowed {
    pub skill: String,
    /// `User` or `Project` for the binding's disk tiers, `Host` for a
    /// directory listed before this one.
    pub by: SkillSource,
    pub path: PathBuf,
}

/// The manager that answers for `name` in a session over `shared` and
/// `host`, and its entry there.
pub(crate) fn winner(
    shared: &Arc<SkillManager>,
    host: Option<&HostSkillLayer>,
    name: &str,
) -> Option<(Arc<SkillManager>, SkillInfo)> {
    let held = shared.get(name);
    if let Some(info) = held.as_ref().filter(|i| !shared.yields_to_host(i)) {
        return Some((shared.clone(), info.clone()));
    }
    host.and_then(|layer| layer.first_with(name))
        .or_else(|| held.map(|info| (shared.clone(), info)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(dir: &Path, name: &str, description: &str) {
        let dir = dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: {description}\n---\n{description} body\n"),
        )
        .unwrap();
    }

    fn open(dir: &Path) -> Arc<HostSkillDir> {
        Arc::new(HostSkillDir::open(
            dir.to_path_buf(),
            SensitivePaths::default(),
        ))
    }

    fn shared_with_user_skill(root: &Path, name: &str) -> Arc<SkillManager> {
        let user = root.join("user");
        skill(&user, name, "the user's");
        let manager = SkillManager::new();
        manager.load_dir_subdirs(&user, SkillSource::User).unwrap();
        manager.load_from(Arc::new(crate::sources::BundledSkills::new(
            Default::default(),
            "/state".into(),
        )));
        Arc::new(manager)
    }

    #[test]
    fn the_disk_beats_a_host_dir_which_beats_a_built_in_and_a_later_dir() {
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (root.path().join("a"), root.path().join("b"));
        skill(&a, "tour", "a's tour");
        skill(&a, "simplify", "a's simplify");
        skill(&b, "tour", "b's tour");
        skill(&b, "guide", "b's guide");
        let shared = shared_with_user_skill(root.path(), "tour");
        let layer = HostSkillLayer::new(vec![open(&a), open(&b)]);

        let (_, tour) = winner(&shared, Some(&layer), "tour").unwrap();
        assert_eq!(tour.source, SkillSource::User);
        let (_, simplify) = winner(&shared, Some(&layer), "simplify").unwrap();
        assert_eq!(simplify.description, "a's simplify");
        assert_eq!(simplify.source, SkillSource::Host);

        let report = layer.report(&shared);
        assert_eq!(report[0].skills, ["simplify"]);
        assert_eq!(report[0].shadowed[0].skill, "tour");
        assert_eq!(report[0].shadowed[0].by, SkillSource::User);
        assert_eq!(report[1].skills, ["guide"]);
        assert_eq!(report[1].shadowed[0].by, SkillSource::User);

        layer.replace(vec![open(&b), open(&a)]);
        let (_, guide) = winner(&shared, Some(&layer), "guide").unwrap();
        assert_eq!(guide.description, "b's guide");
        let report = layer.report(&shared);
        assert_eq!(
            report[1].shadowed.iter().find(|s| s.skill == "simplify"),
            None
        );
    }

    #[test]
    fn a_dir_that_appears_behind_a_link_is_expanded_too() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let link = root.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        let link = real.clone();
        let host = open(&link);
        skill(&real, "tour", "late");
        assert!(host.appeared());
        let manager = host.manager();
        assert_eq!(
            manager.get_skill_content("tour").as_deref().map(str::trim),
            Some("late body")
        );
    }

    #[test]
    fn a_missing_dir_is_read_once_it_appears() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("later");
        let host = open(&dir);
        assert!(!host.present());
        assert!(!host.appeared());
        skill(&dir, "tour", "late");
        assert!(host.appeared());
        assert!(host.present() && host.manager().get("tour").is_some());
        assert!(!host.appeared());
    }

    #[test]
    fn mcp_names_and_links_out_of_the_dir_are_not_read() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("host");
        let outside = root.path().join("outside");
        skill(&dir, "mcp__x__y", "claims an MCP name");
        skill(&dir, "kept", "kept");
        skill(&outside, "escape", "outside");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("escape"), dir.join("escape")).unwrap();
        let host = open(&dir);
        let names = host.names();
        assert_eq!(names, ["kept"]);
    }
}
