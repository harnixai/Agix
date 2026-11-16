//! The paths a policy is written against: working directory, writable roots and state
//! root resolved through their symlinks, plus what in a project must stay read-only
//! (git control files, submodules, engine configuration). Shared by both backends.

use super::SandboxPolicy;
use base::paths::{
    ENGINE_CONFIG_DIRS, ENGINE_CONFIG_FILES, ENGINE_CONFIG_ROOTS, GIT_CONTROL_FILES, GIT_HOOKS_DIR,
    GIT_WORKTREE_CONTROL_FILES, PROJECT_ENGINE_CONFIG_DIRS, PROJECT_WORKTREES_DIR,
    REPOSITORY_CONFIG_DIR, STATE_ROOT_SESSION_TMP, STATE_ROOT_WORKING_DIRS,
};
use std::path::{Path, PathBuf};

/// The state root to protect: the one the caller named, else the
/// conventional `$HOME/.agix`.
fn state_root_of(policy: &SandboxPolicy) -> Option<PathBuf> {
    policy
        .state_root
        .clone()
        .or_else(|| base::paths::home_dir().map(|h| h.join(".agix")))
}

/// The project's `.agix`, as given and as the kernel sees it.
pub(super) fn project_agix_dirs(cwd: &Path) -> Vec<PathBuf> {
    let given = cwd.join(".agix");
    let real = base::path::resolve_physically(&given);
    if real == given {
        vec![given]
    } else {
        vec![given, real]
    }
}

/// The paths a policy is written against, each with its symlinks resolved.
///
/// The kernel checks a write against the path it resolves to: on macOS a rule
/// for `/tmp/x` never matches, because the write lands on `/private/tmp/x`.
pub(super) struct Roots {
    pub(super) cwd: PathBuf,
    /// The policy's `worktree_git`, its git directory guarded in place of
    /// `cwd/.git`.
    pub(super) worktree_git: Option<base::paths::WorktreeGit>,
    pub(super) only_additional: bool,
    pub(super) writable: Vec<PathBuf>,
    pub(super) state_root: Option<PathBuf>,
    pub(super) paths: base::paths::SensitivePaths,
}

impl Roots {
    pub(super) fn resolve(cwd: &Path, additional: &[PathBuf], policy: &SandboxPolicy) -> Self {
        let cwd = base::path::resolve_physically(cwd);
        let state_root = state_root_of(policy).map(|p| base::path::resolve_physically(&p));
        Self {
            paths: base::paths::SensitivePaths::new(state_root.as_deref(), Some(&cwd)),
            cwd,
            worktree_git: policy
                .worktree_git
                .as_ref()
                .map(|git| base::paths::WorktreeGit {
                    dir: base::path::resolve_physically(&git.dir),
                    admin: base::path::resolve_physically(&git.admin),
                    branch: git.branch.clone(),
                }),
            only_additional: policy.only_additional_writable,
            writable: additional
                .iter()
                .map(|p| base::path::resolve_physically(p))
                .collect(),
            state_root,
        }
    }

    /// Whether a write to `path` falls inside one of the writable roots.
    pub(super) fn write_roots(&self) -> impl Iterator<Item = &Path> {
        (!self.only_additional)
            .then_some(self.cwd.as_path())
            .into_iter()
            .chain(self.writable.iter().map(PathBuf::as_path))
    }

    pub(super) fn opens(&self, path: &Path) -> bool {
        (!self.only_additional && path.starts_with(&self.cwd))
            || self.writable.iter().any(|root| path.starts_with(root))
    }

    /// Where the state root is not fenced off, the files and the
    /// directories of its configuration a command could otherwise write:
    /// it is an `.agix` like a project's. One the working directory is in
    /// is left out, or the project could not be written at all.
    pub(super) fn unfenced_state_root_config(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let Some(root) = self
            .state_root
            .as_deref()
            .filter(|_| self.fenced_state_root().is_none())
        else {
            return Default::default();
        };
        let guarded = |names: &[&str]| -> Vec<PathBuf> {
            names
                .iter()
                .map(|name| root.join(name))
                .filter(|p| self.opens(p) && !self.cwd.starts_with(p))
                .collect()
        };
        (guarded(ENGINE_CONFIG_FILES), guarded(ENGINE_CONFIG_DIRS))
    }

    /// The state root to fence off, its working directories left open —
    /// unless the project lives inside it, in which case fencing the root
    /// would fence the project too, or nothing but the named roots is
    /// writable, in which case it is shut already.
    /// The writable roots that are a session's own directory under the
    /// state root's `tmp/`: the fence around the state root would otherwise
    /// shut them along with the rest of it.
    pub(super) fn session_temp_dirs_in<'a>(
        &'a self,
        root: &Path,
    ) -> impl Iterator<Item = &'a PathBuf> {
        let tmp = root.join(STATE_ROOT_SESSION_TMP);
        self.writable
            .iter()
            .filter(move |p| p.starts_with(&tmp) && **p != tmp)
    }

    pub(super) fn fenced_state_root(&self) -> Option<&Path> {
        self.state_root
            .as_deref()
            .filter(|root| !self.only_additional && !self.project_in(root))
    }

    /// Whether the working directory is a project inside `state_root`. A
    /// session without a project works in a directory of its own in the
    /// state root's scratchpad, which the fence leaves open anyway, so that
    /// one is no reason to drop the fence.
    pub(super) fn project_in(&self, state_root: &Path) -> bool {
        self.cwd.starts_with(state_root)
            && !STATE_ROOT_WORKING_DIRS
                .iter()
                .any(|dir| self.cwd.starts_with(state_root.join(dir)))
    }
}

/// Whether a path component `name` is one `deny_write` refuses: equal to one
/// of `denied`, or beginning with it and a dot.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn denied_for_write(name: &str, denied: &[String]) -> bool {
    denied.iter().any(|d| {
        name == d
            || name
                .strip_prefix(d.as_str())
                .is_some_and(|r| r.starts_with('.'))
    })
}

/// The `deny_write` names a path component can be: not empty, no separator.
pub(super) fn write_denial_names(denied: &[String]) -> Vec<String> {
    denied
        .iter()
        .filter(|d| !d.is_empty() && !d.contains('/'))
        .cloned()
        .collect()
}

/// How deep below a denied directory its symbolic links are looked for, and
/// the most entries looked at.
const LINK_SEARCH_DEPTH: usize = 4;
const LINK_SEARCH_ENTRIES: usize = 4096;

/// Where the symbolic links inside the directory `dir` lead, when that is
/// outside it: a denial of `dir` does not cover those, and the kernel
/// resolves a link before it matches a path. Empty for anything that is not
/// a directory.
pub(super) fn link_targets_outside(dir: &Path) -> Vec<PathBuf> {
    let real = base::path::resolve_physically(dir);
    let mut found = Vec::new();
    let mut pending = vec![(dir.to_path_buf(), 0)];
    let mut looked_at = 0;
    while let Some((at, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            continue;
        };
        for entry in entries.flatten() {
            looked_at += 1;
            if looked_at > LINK_SEARCH_ENTRIES {
                return found;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                let target = base::path::resolve_physically(&entry.path());
                if !target.starts_with(&real) && !found.contains(&target) {
                    found.push(target);
                }
            } else if kind.is_dir() && depth < LINK_SEARCH_DEPTH {
                pending.push((entry.path(), depth + 1));
            }
        }
    }
    found
}

/// The real directories directly under `dir`.
pub(super) fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

/// How many directories deep below a git directory's `modules/` the search
/// for submodules goes, counting both the levels of a name with slashes in
/// it and the `modules/` of each nested submodule.
const SUBMODULE_SEARCH_DEPTH: usize = 12;

/// The most submodule git directories one project gets rules for.
const SUBMODULE_LIMIT: usize = 256;

/// The submodules of the repository whose git directory is `git_dir`, nested
/// ones included.
#[derive(Default)]
pub(super) struct Submodules {
    /// Every directory from `git_dir/modules` down to each submodule's git
    /// directory, outermost first: what would let a command move a guarded
    /// file's directory aside and put another in its place.
    pub(super) dirs: Vec<PathBuf>,
    pub(super) git_dirs: Vec<PathBuf>,
}

/// A submodule's git directory sits at `modules/<name>`, `<name>` possibly
/// several directories deep, and has a `modules/` of its own. It is known by
/// its `HEAD` or its `config`: the `config` is what a command cannot remove,
/// so removing `HEAD` does not take a submodule off the list. Symbolic links
/// are not followed: the rules are written against the paths found, and a
/// link would lead them somewhere the repository chose.
pub(super) fn submodules_of(git_dir: &Path) -> Submodules {
    let mut found = Submodules::default();
    let mut pending = vec![(git_dir.join("modules"), 0)];
    while let Some((dir, depth)) = pending.pop() {
        if depth >= SUBMODULE_SEARCH_DEPTH || found.git_dirs.len() >= SUBMODULE_LIMIT {
            continue;
        }
        if !dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            continue;
        }
        found.dirs.push(dir.clone());
        for child in subdirs(&dir) {
            let is_git_dir = ["HEAD", "config"]
                .iter()
                .any(|f| child.join(f).symlink_metadata().is_ok());
            if is_git_dir {
                found.dirs.push(child.clone());
                pending.push((child.join("modules"), depth + 1));
                found.git_dirs.push(child);
            } else {
                pending.push((child, depth + 1));
            }
        }
    }
    found
}

/// What the project's own `.git/config` points at inside the working tree: the
/// hooks directory `core.hooksPath` names and the files `[include]` and
/// `[includeIf]` pull in, nested includes among them. Paths a command could
/// create are listed whether or not they exist yet.
#[derive(Default)]
pub(super) struct GitConfigured {
    pub(super) hooks: Option<PathBuf>,
    pub(super) included: Vec<PathBuf>,
}

pub(super) fn git_configured(cwd: &Path, git_dir: &Path) -> GitConfigured {
    let mut found = GitConfigured::default();
    let home = base::paths::home_dir();
    let mut seen = Vec::new();
    read_git_config(
        &git_dir.join("config"),
        cwd,
        home.as_deref(),
        &mut seen,
        &mut found,
    );
    found
}

fn read_git_config(
    config: &Path,
    cwd: &Path,
    home: Option<&Path>,
    seen: &mut Vec<PathBuf>,
    found: &mut GitConfigured,
) {
    let config = base::path::resolve_physically(config);
    if seen.contains(&config) || seen.len() >= 32 {
        return;
    }
    seen.push(config.clone());
    let Ok(text) = std::fs::read_to_string(&config) else {
        return;
    };
    let named = |value: &str, relative_to: &Path| match (value.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => relative_to.join(value),
    };
    let mut section = String::new();
    for line in text.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix('[') {
            section = header
                .split(|c: char| c == ']' || c.is_whitespace() || c == '"')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim().to_ascii_lowercase(), config_value(value));
        match (section.as_str(), key.as_str()) {
            ("core", "hookspath") if !value.is_empty() => {
                let hooks = base::path::resolve_physically(&named(&value, cwd));
                // A hooks path that holds the project itself would shut it.
                found.hooks = Some(hooks).filter(|h| !cwd.starts_with(h));
            }
            ("include" | "includeif", "path") if !value.is_empty() => {
                let included = base::path::resolve_physically(&named(
                    &value,
                    config.parent().unwrap_or(Path::new("/")),
                ));
                found.included.push(included.clone());
                read_git_config(&included, cwd, home, seen, found);
            }
            _ => {}
        }
    }
}

/// A git config value: unquoted trailing comment dropped, quotes removed.
fn config_value(raw: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    for c in raw.trim().chars() {
        match c {
            '"' => quoted = !quoted,
            '#' | ';' if !quoted => break,
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

/// What in the project a sandboxed command must not write, because the
/// engine, or the user's own git, acts on it later outside the sandbox.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) struct ProjectGuard {
    /// `.git` and the git directory it is (in a worktree, the common one it
    /// leads to), the directories down to each submodule's git directory and
    /// the configuration roots themselves: renaming one away, or replacing
    /// it with a symlink, would put every rule below on a path nobody reads.
    pub(super) entries: Vec<PathBuf>,
    pub(super) files: Vec<PathBuf>,
    pub(super) trees: Vec<PathBuf>,
    /// The `worktrees/` of each guarded git directory and the project's
    /// `.agix/worktrees/`: what a worktree made after the profile was
    /// written lands in, where no fixed path names its control files yet.
    pub(super) worktree_parents: WorktreeParents,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) struct WorktreeParents {
    pub(super) admin: Vec<PathBuf>,
    pub(super) links: PathBuf,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn project_guard(roots: &Roots) -> ProjectGuard {
    let cwd = roots.cwd.as_path();
    let git = cwd.join(REPOSITORY_CONFIG_DIR);
    let git_dir = roots
        .worktree_git
        .as_ref()
        .map(|g| g.dir.clone())
        .unwrap_or_else(|| base::path::resolve_physically(&git));
    let agix_dir = base::path::resolve_physically(&cwd.join(".agix"));

    let mut entries = vec![git.clone()];
    if git_dir != git {
        entries.push(git_dir.clone());
    }
    let submodules = submodules_of(&git_dir);
    entries.extend(submodules.dirs);
    let mut files = Vec::new();
    let mut trees = Vec::new();
    let mut admin = Vec::new();
    for repo in std::iter::once(&git_dir).chain(&submodules.git_dirs) {
        files.extend(GIT_CONTROL_FILES.iter().map(|f| repo.join(f)));
        trees.push(repo.join(GIT_HOOKS_DIR));
        for worktree in subdirs(&repo.join(GIT_WORKTREES_DIR)) {
            files.extend(GIT_WORKTREE_CONTROL_FILES.iter().map(|f| worktree.join(f)));
        }
        admin.push(repo.join(GIT_WORKTREES_DIR));
    }
    if roots.worktree_git.is_some() {
        trees.push(git_dir.join(GIT_OBJECTS_INFO_DIR));
    }
    let configured = git_configured(cwd, &git_dir);
    trees.extend(configured.hooks);
    files.extend(configured.included);
    for name in ENGINE_CONFIG_ROOTS {
        let root = cwd.join(name);
        let real = base::path::resolve_physically(&root);
        files.extend(ENGINE_CONFIG_FILES.iter().map(|f| real.join(f)));
        trees.extend(ENGINE_CONFIG_DIRS.iter().map(|d| real.join(d)));
        if real != root {
            entries.push(real);
        }
        entries.push(root);
    }
    for dir in PROJECT_ENGINE_CONFIG_DIRS {
        let mut above = cwd.to_path_buf();
        for part in Path::new(dir).components() {
            above.push(part);
            let real = base::path::resolve_physically(&above);
            if real != above {
                entries.push(real);
            }
            entries.push(above.clone());
        }
        trees.push(base::path::resolve_physically(&above));
    }

    let links = agix_dir.join(PROJECT_WORKTREES_DIR);
    for worktree in subdirs(&links) {
        files.push(worktree.join(REPOSITORY_CONFIG_DIR));
    }
    ProjectGuard {
        entries,
        files,
        trees,
        worktree_parents: WorktreeParents { admin, links },
    }
}

/// Where a git directory keeps its linked worktrees' entries.
pub(super) const GIT_WORKTREES_DIR: &str = "worktrees";

/// Where the object store names other stores to read from (`alternates`)
/// and keeps the indexes git trusts over the objects themselves: shut to a
/// worktree's commands, which may write the objects.
pub(super) const GIT_OBJECTS_INFO_DIR: &str = "objects/info";
