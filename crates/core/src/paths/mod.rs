//! Data directory path abstraction.
//!
//! `ConfigPaths` provides a single source of truth for all persistent
//! directories used by the AGENT: a flat cross-scene **global** root
//! (conventionally `~/.agix/`), a **scene**-specific override root nested
//! under it (`<global>/scenes/<scope>/`), and a **local**/project root
//! (`<cwd>/<project dir>/`, `.agix` unless the instance's [`Layout`] names
//! another). The names themselves are in [`layout`].
//!
//! **The roots are given, never discovered.** Nothing in this module reads
//! the environment: the process entry point decides which directory the
//! instance owns and passes it down. `~/.agix` is what that entry point
//! usually picks, not something the library assumes — which is what lets a
//! test, a service account, or a deployment with its state elsewhere work
//! without every module having to agree separately.
//!
//! `scope` identifies which product built on this engine owns the
//! scene-specific override state (e.g. a coding-agent product might use
//! `"code"`). The engine never assumes a default — callers must pass one
//! explicitly. The local/project root is flat and has no scope segment: a
//! project is assumed to be worked on by one product instance at a time, so
//! there's no collision to avoid the way there is under a shared `$HOME`.
//!
//! The user-level tree is split by resource type:
//!
//! - **Global + scene override** (config-shaped, keyed by name): `settings.json`,
//!   `skills/`, `plugins/`, `agents/`, `rules/`, `hooks/`. Present at both
//!   `~/.agix/<resource>/` (base) and `~/.agix/scenes/<scope>/<resource>/`
//!   (override) — a scene-specific entry with the same name wins.
//! - **Global + project only, no scene tier** (history/state-shaped):
//!   `memory/`, `sessions/`, `recordings/`. Present at `~/.agix/<resource>/`
//!   (used when there's no specific project — reserved for a future
//!   no-project/desktop mode, not yet exercised by `daemon`) and
//!   `<cwd>/.agix/<resource>/` (project, used today). Deliberately **not**
//!   scene-scoped: a scene's tool/system-prompt config has no bearing on
//!   which project a session or memory file belongs to, so borrowing across
//!   scenes would be a correctness hazard (e.g. resuming a session recorded
//!   under a different scene's tool set, or a recording replaying against the
//!   wrong tool config).
//!
//! Project-level skills are **not** under `ConfigPaths` at all — they live in
//! `.agents/skills/`, a sibling of `.agix/`, because that's the path Codex
//! and other external agent tools scan. `ConfigPaths` only models Agix's
//! own private state and extensions, which no other tool reads.

pub mod layout;
mod sensitive;

pub use layout::{
    entry, ConfigRoot, Layout, LayoutError, ProductNameError, StateRoot, AGENTS_DIR, CLAUDE_DIR,
    DEFAULT_PRODUCT_NAME, DEFAULT_PRODUCT_URL, DEFAULT_PROJECT_DIR, DEFAULT_STATE_DIR,
    ENGINE_CONFIG_DIRS, ENGINE_CONFIG_FILES, FOREIGN_CONFIG_ROOTS, GIT_WORKTREES_DIR,
    LOCAL_DATA_IGNORED_DIRS, LOCAL_DATA_IGNORED_FILES, PROJECT_ENGINE_CONFIG_DIRS,
    PROJECT_SKILLS_DIR, PROJECT_WORKTREES_DIR, SETTINGS_FILE, SETTINGS_LOCAL_FILE,
    STATE_ROOT_SCENE_FILES, STATE_ROOT_SCRATCHPAD, STATE_ROOT_SESSION_TMP, STATE_ROOT_WORKING_DIRS,
};
pub use sensitive::{
    credential_dir_names, session_scratch_dir, tool_results_dir, ConfigRootAt, Credential,
    KeyFileNames, Located, PromptFiles, Scope, Secret, SensitivePaths, CREDENTIALS,
    GIT_CONTROL_FILES, GIT_HOOKS_DIR, GIT_WORKTREE_CONTROL_FILES, HOME_CREDENTIAL_STORES,
    REPOSITORY_CONFIG_DIR, SECRET_FILE_NAMES,
};

use std::path::{Path, PathBuf};

use crate::interface::config_source::Tier;

use serde::{Deserialize, Serialize};

/// This machine's home directory: `HOME`, else `USERPROFILE`; an empty value counts as unset.
#[expect(
    clippy::disallowed_methods,
    reason = "the one place this machine's home is looked up"
)]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Path configuration for data directories.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[non_exhaustive]
pub struct PathSettings {
    /// User-level, scene-specific override root (e.g. `~/.agix/scenes/<scope>/`).
    pub user_data_dir: PathBuf,
    /// User-level, cross-scene global root — flat, shared by every scene
    /// (e.g. `~/.agix/`). Used by resources that don't have a scene tier
    /// (`memory`/`sessions`/`recordings`/`mcp`) and as the base layer for resources
    /// that do (`settings.json`/`skills`/`plugins`/`agents`/`rules`/`hooks`).
    /// See `base::paths::ConfigPaths` module docs for the full breakdown.
    #[serde(default)]
    pub global_data_dir: PathBuf,
    /// The project tier's root: `<project>/<project dir>/` in a project, the state
    /// root itself (`global_data_dir`) in a session without one.
    pub local_data_dir: PathBuf,
    /// Where the session's tools work, and whether there is a project.
    pub workspace: Workspace,
    /// Which product instance's user-level state `user_data_dir` was built
    /// for (see `base::paths::ConfigPaths`). Carried alongside the resolved
    /// dirs so downstream code (e.g. `FrozenContext::collect_environment`) doesn't have
    /// to re-derive it from the path.
    #[serde(default = "default_scope")]
    pub scope: String,
    /// The project is not trusted: what its tier holds under a name the user
    /// tier also holds does not replace the user's.
    #[serde(skip)]
    #[schemars(skip)]
    pub project_untrusted: bool,
    /// The worktree a sub-agent was given: its tools work there, while the
    /// project stays the workspace's.
    #[serde(skip)]
    #[schemars(skip)]
    pub worktree: Option<LinkedWorktree>,
    /// What the instance names its directories; `local_data_dir` is
    /// [`Layout::project_dir`] of the project.
    #[serde(skip)]
    #[schemars(skip)]
    pub layout: Layout,
}

/// A linked worktree of the project, and where in the repository's git
/// directory a commit in it writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedWorktree {
    pub dir: PathBuf,
    /// As the engine found it when it made the worktree. Never re-read from
    /// the worktree's `.git` link or the `commondir` it leads to: a
    /// sandboxed command may have rewritten both since. `None`: nothing in
    /// the repository's git directory is writable, so a sandboxed command
    /// cannot commit there.
    pub git: Option<WorktreeGit>,
}

/// The parts of the repository's git directory that a commit in one linked
/// worktree writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeGit {
    /// The repository's git directory.
    pub dir: PathBuf,
    /// The worktree's entry under `dir/worktrees/`: its `HEAD`, index and
    /// `HEAD` reflog.
    pub admin: PathBuf,
    /// The branch checked out in it, e.g. `agix/worktree-x`.
    pub branch: String,
}

impl WorktreeGit {
    /// The loose ref of [`Self::branch`]. git rewrites it by renaming
    /// `<ref>.lock` over it.
    pub fn branch_ref(&self) -> PathBuf {
        self.dir.join("refs/heads").join(&self.branch)
    }

    pub fn branch_ref_lock(&self) -> PathBuf {
        let mut lock = self.branch_ref().into_os_string();
        lock.push(".lock");
        PathBuf::from(lock)
    }

    /// What a commit writes: the object store, the worktree's entry, the
    /// branch's ref, its lock and its reflog. Nothing else in `dir` — not
    /// the other refs, `packed-refs`, `info/` or the main worktree's files.
    /// `objects/info/` and the entry's control files are held read-only on
    /// top by the sandbox.
    pub fn writable(&self) -> Vec<PathBuf> {
        vec![
            self.dir.join("objects"),
            self.admin.clone(),
            self.branch_ref(),
            self.branch_ref_lock(),
            self.dir.join("logs/refs/heads").join(&self.branch),
        ]
    }
}

/// Placeholder directories for settings that were not loaded from any — the
/// ones [`LoadedSettings::new`] is commonly given in tests.
impl Default for PathSettings {
    fn default() -> Self {
        let state = PathBuf::from("~").join(DEFAULT_STATE_DIR);
        Self {
            user_data_dir: StateRoot::new(state.clone()).scene_dir("agent"),
            global_data_dir: state,
            local_data_dir: PathBuf::from("."),
            workspace: Workspace::Project(PathBuf::new()),
            scope: default_scope(),
            project_untrusted: false,
            worktree: None,
            layout: Layout::default(),
        }
    }
}

fn default_scope() -> String {
    "code".to_string()
}

/// What a session works in (docs/ARCHITECTURE.md §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Workspace {
    /// A project: the tools work in it, and its project directory is the
    /// project tier.
    Project(PathBuf),
    /// No project (`project_root: null`): no project tier, no git or
    /// `AGENTS.md` context, and the tools work in this directory of the
    /// session's own under the state root's scratchpad
    /// ([`crate::paths::session_scratch_dir`]) — never in the state root's
    /// parent, which for `~/.agix` is the user's home.
    Scratch(PathBuf),
}

impl Workspace {
    /// The project whose project directory is `local_data_dir`.
    pub fn project_of(local_data_dir: &Path) -> Self {
        Self::Project(
            local_data_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| local_data_dir.to_path_buf()),
        )
    }

    /// The project, `None` without one.
    pub fn project_root(&self) -> Option<&Path> {
        match self {
            Self::Project(root) => Some(root),
            Self::Scratch(_) => None,
        }
    }
}

impl PathSettings {
    pub fn new(
        user_data_dir: PathBuf,
        global_data_dir: PathBuf,
        local_data_dir: PathBuf,
        workspace: Workspace,
        scope: String,
        layout: Layout,
    ) -> Self {
        Self {
            user_data_dir,
            global_data_dir,
            local_data_dir,
            workspace,
            scope,
            project_untrusted: false,
            worktree: None,
            layout,
        }
    }

    /// The derived paths of these roots.
    pub fn config_paths(&self) -> ConfigPaths {
        ConfigPaths::from_settings(self)
    }

    /// The project, `None` in a session without one.
    pub fn project_root(&self) -> Option<&Path> {
        self.workspace.project_root()
    }

    /// The tools' working directory: the root a write needs no approval in,
    /// a read none either, and a sandboxed command may write.
    pub fn working_dir(&self) -> &Path {
        if let Some(worktree) = &self.worktree {
            return &worktree.dir;
        }
        match &self.workspace {
            Workspace::Project(dir) | Workspace::Scratch(dir) => dir,
        }
    }

    /// `git`, resolved, when its git directory is the project's own
    /// `<project>/.git` — the only git directory the sandbox guards for a
    /// worktree — its entry lies directly under that directory's
    /// `worktrees/`, and its branch stays below `refs/heads/`.
    pub fn project_worktree_git(&self, git: &WorktreeGit) -> Option<WorktreeGit> {
        let own = self
            .project_root()?
            .join(REPOSITORY_CONFIG_DIR)
            .canonicalize()
            .ok()?;
        if git.dir.canonicalize().ok()? != own || !own.is_dir() {
            return None;
        }
        let admin = git.admin.canonicalize().ok()?;
        let branch_is_plain = git.branch.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".." && !part.ends_with(".lock")
        });
        (admin.parent() == Some(own.join(GIT_WORKTREES_DIR).as_path()) && branch_is_plain).then(
            || WorktreeGit {
                dir: own,
                admin,
                branch: git.branch.clone(),
            },
        )
    }

    /// What of the repository's git directory a sandboxed command in the
    /// session's worktree may write, while it is still the project's own.
    pub fn worktree_git(&self) -> Option<WorktreeGit> {
        self.project_worktree_git(self.worktree.as_ref()?.git.as_ref()?)
    }

    /// The project tier's `settings.local.json` — the file the interactive
    /// permission prompt's "always allow, this project" writes to.
    ///
    /// Inside `local_data_dir` (the project directory), not beside it: the
    /// Bash sandbox's deny-file-write rules (`tools::bash::sandbox`) protect
    /// exactly this path to stop a sandboxed command from granting itself
    /// permissions, and a copy written anywhere else would sit outside that
    /// protection while still being loaded by `LoadedSettings::load`.
    ///
    /// `None` without a project: "this project" means nothing there, and its
    /// `local_data_dir` is the state root, whose `settings.local.json` is the
    /// global tier — a rule written there would allow the call everywhere.
    pub fn local_settings_file(&self) -> Option<PathBuf> {
        self.project_root()
            .map(|_| self.local_data_dir.join(SETTINGS_LOCAL_FILE))
    }
}

/// What a data directory's `.gitignore` keeps out of the repository the
/// directory sits in, one line each: [`LOCAL_DATA_IGNORED_FILES`] and
/// [`LOCAL_DATA_IGNORED_DIRS`].
pub fn local_data_gitignore() -> Vec<String> {
    LOCAL_DATA_IGNORED_FILES
        .iter()
        .map(|file| file.to_string())
        .chain(LOCAL_DATA_IGNORED_DIRS.iter().map(|dir| format!("{dir}/")))
        .collect()
}

/// Make sure `dir`'s `.gitignore` has every [`local_data_gitignore`] entry,
/// appending the ones it lacks and touching nothing else in it: in a cloned
/// repository the file is the repository author's, and the keys and
/// recordings the engine writes here must stay out of it all the same.
pub fn ensure_local_data_gitignore(dir: &std::path::Path) {
    use std::io::Write;
    let path = dir.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let lacking: Vec<String> = local_data_gitignore()
        .into_iter()
        .filter(|entry| !existing.lines().any(|line| line.trim() == entry))
        .collect();
    if lacking.is_empty() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let mut added = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        added.push('\n');
    }
    for entry in lacking {
        added.push_str(&entry);
        added.push('\n');
    }
    let _ = file.write_all(added.as_bytes());
}

/// Unified management of AGENT persistent directory paths: the three tiers'
/// roots and every path derived from them. A caller asks for the path here
/// rather than joining a name onto a root.
///
/// Usually `global_data_dir = $HOME/.agix/`,
/// `user_data_dir = <global>/scenes/<scope>/`,
/// `local_data_dir = <project>/<layout's project dir>/`.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// User-level, scene-specific override root.
    pub user_data_dir: PathBuf,
    /// User-level, cross-scene global root (flat, shared by every scene).
    pub global_data_dir: PathBuf,
    /// The project tier's root; the state root in a session without a project.
    pub local_data_dir: PathBuf,
    /// The project, `None` for a session without one
    /// ([`crate::paths::PathSettings::project_root`]).
    pub project_root: Option<PathBuf>,
    /// See [`PathSettings::project_untrusted`].
    pub project_untrusted: bool,
    pub layout: Layout,
}

impl ConfigPaths {
    /// Build from explicit roots.
    ///
    /// There is no constructor that consults the environment, and that is the
    /// point: a process serves one instance, and which directory that
    /// instance owns is the caller's to decide. Reading `$HOME` here would
    /// make every downstream module silently agree on the invoking user's
    /// home, which is wrong for a test, wrong for a service account, and
    /// wrong for any deployment that keeps its state somewhere else.
    ///
    /// `scope` identifies the product instance whose scene-specific override
    /// tree lives under `global_data_dir/scenes/<scope>/`.
    pub fn new(
        global_data_dir: PathBuf,
        local_data_dir: PathBuf,
        scope: &str,
        layout: Layout,
    ) -> Self {
        let project_root = Workspace::project_of(&local_data_dir)
            .project_root()
            .map(PathBuf::from);
        Self {
            user_data_dir: StateRoot::new(global_data_dir.clone()).scene_dir(scope),
            global_data_dir,
            local_data_dir,
            project_root,
            project_untrusted: false,
            layout,
        }
    }

    /// The paths of a session of `scene_dir` in `project`, or in none: the
    /// project tier is `layout`'s directory in the project, and without a
    /// project it is the state root, as `LoadedSettings` makes it.
    pub fn of(
        global_data_dir: PathBuf,
        scene_dir: PathBuf,
        project: Option<&Path>,
        layout: Layout,
    ) -> Self {
        Self {
            user_data_dir: scene_dir,
            local_data_dir: project
                .map(|root| layout.project_dir(root))
                .unwrap_or_else(|| global_data_dir.clone()),
            global_data_dir,
            project_root: project.map(Path::to_path_buf),
            project_untrusted: false,
            layout,
        }
    }

    /// The same roots [`crate::settings::Settings`] already
    /// carries — for the call sites that hold `Settings` and want these
    /// accessors without re-deriving anything.
    pub fn from_settings(paths: &crate::paths::PathSettings) -> Self {
        Self {
            user_data_dir: paths.user_data_dir.clone(),
            global_data_dir: paths.global_data_dir.clone(),
            local_data_dir: paths.local_data_dir.clone(),
            project_root: paths.project_root().map(PathBuf::from),
            project_untrusted: paths.project_untrusted,
            layout: paths.layout.clone(),
        }
    }

    pub fn state_root(&self) -> StateRoot {
        StateRoot::new(self.global_data_dir.clone())
    }

    pub fn scene_dir(&self) -> &Path {
        &self.user_data_dir
    }

    /// The project's own directory, `None` without a project.
    pub fn project_dir(&self) -> Option<&Path> {
        self.project_root
            .as_ref()
            .map(|_| self.local_data_dir.as_path())
    }

    /// The directory of a settings tier: the state root, the scene
    /// directory, the project directory. `None` for the project tier
    /// without a project, and for the managed tier, which is a file.
    pub fn tier_dir(&self, tier: Tier) -> Option<PathBuf> {
        match tier {
            Tier::Global => Some(self.global_data_dir.clone()),
            Tier::Scene => Some(self.user_data_dir.clone()),
            Tier::Project => self.project_dir().map(Path::to_path_buf),
            Tier::Managed => None,
        }
    }

    /// `name`, one of [`entry`], in a tier's directory.
    pub fn tier_entry(&self, tier: Tier, name: &str) -> Option<PathBuf> {
        self.tier_dir(tier).map(|dir| dir.join(name))
    }

    pub fn settings_file(&self, tier: Tier) -> Option<PathBuf> {
        self.tier_entry(tier, entry::SETTINGS)
    }

    pub fn settings_local_file(&self, tier: Tier) -> Option<PathBuf> {
        self.tier_entry(tier, entry::SETTINGS_LOCAL)
    }

    pub fn global_skills_dir(&self) -> PathBuf {
        self.global_data_dir.join(entry::SKILLS)
    }

    pub fn user_skills_dir(&self) -> PathBuf {
        self.user_data_dir.join(entry::SKILLS)
    }

    /// `<project>/.agents/skills`: beside the project directory, not in it.
    pub fn project_skills_dir(&self) -> Option<PathBuf> {
        self.project_root
            .as_ref()
            .map(|root| root.join(PROJECT_SKILLS_DIR))
    }

    pub fn global_agents_dir(&self) -> PathBuf {
        self.global_data_dir.join(entry::AGENTS)
    }

    pub fn user_agents_dir(&self) -> PathBuf {
        self.user_data_dir.join(entry::AGENTS)
    }

    pub fn project_agents_dir(&self) -> Option<PathBuf> {
        self.tier_entry(Tier::Project, entry::AGENTS)
    }

    pub fn global_hooks_dir(&self) -> PathBuf {
        self.global_data_dir.join(entry::HOOKS)
    }

    pub fn user_hooks_dir(&self) -> PathBuf {
        self.user_data_dir.join(entry::HOOKS)
    }

    pub fn project_hooks_dir(&self) -> Option<PathBuf> {
        self.tier_entry(Tier::Project, entry::HOOKS)
    }

    pub fn global_rules_dir(&self) -> PathBuf {
        self.global_data_dir.join(entry::RULES)
    }

    pub fn user_rules_dir(&self) -> PathBuf {
        self.user_data_dir.join(entry::RULES)
    }

    pub fn project_rules_dir(&self) -> Option<PathBuf> {
        self.tier_entry(Tier::Project, entry::RULES)
    }

    pub fn user_output_styles_dir(&self) -> PathBuf {
        self.user_data_dir.join(entry::OUTPUT_STYLES)
    }

    pub fn project_output_styles_dir(&self) -> Option<PathBuf> {
        self.tier_entry(Tier::Project, entry::OUTPUT_STYLES)
    }

    pub fn global_plugins_dir(&self) -> PathBuf {
        self.global_data_dir.join(entry::PLUGINS)
    }

    pub fn user_plugins_dir(&self) -> PathBuf {
        self.user_data_dir.join(entry::PLUGINS)
    }

    /// Cross-scene memory root — used when there's no specific project open
    /// (reserved for future no-project/desktop use; today sessions always
    /// have a project, so `local_memory_dir()` is what's actually read).
    pub fn global_memory_dir(&self) -> PathBuf {
        self.global_data_dir.join(entry::MEMORY)
    }

    pub fn global_recordings_dir(&self) -> PathBuf {
        self.global_data_dir.join(entry::RECORDINGS)
    }

    pub fn local_memory_dir(&self) -> PathBuf {
        self.local_data_dir.join(entry::MEMORY)
    }

    pub fn scratchpad_dir(&self) -> PathBuf {
        self.local_data_dir.join(entry::SCRATCHPAD)
    }

    pub fn telemetry_dir(&self) -> PathBuf {
        self.local_data_dir.join(entry::TELEMETRY)
    }

    /// [`tool_results_dir`] under the project tier's root.
    pub fn tool_results_dir(&self, session_id: &str) -> PathBuf {
        tool_results_dir(&self.local_data_dir, session_id)
    }

    /// The scene's `scheduled_tasks.json`.
    pub fn scheduled_tasks_file(&self) -> PathBuf {
        self.user_data_dir.join(entry::SCHEDULED_TASKS)
    }

    pub fn mcp_oauth_dir(&self) -> PathBuf {
        self.user_data_dir.join(entry::MCP).join(entry::OAUTH)
    }

    /// Where the import records that its question was answered, `None`
    /// without a project.
    pub fn imported_marker(&self) -> Option<PathBuf> {
        self.tier_entry(Tier::Project, entry::IMPORTED_MARKER)
    }

    /// The teams of a session working in `cwd` ([`Layout::teams_dir`]).
    pub fn teams_dir(&self, cwd: &Path) -> PathBuf {
        self.layout.teams_dir(cwd)
    }

    /// The engine's worktrees of the project ([`Layout::worktrees_dir`]).
    pub fn worktrees_dir(&self) -> Option<PathBuf> {
        self.project_root
            .as_ref()
            .map(|root| self.layout.worktrees_dir(root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> ConfigPaths {
        ConfigPaths::new(
            PathBuf::from("/state/.agix"),
            PathBuf::from("/tmp/test-project/.agix"),
            "code",
            Layout::default(),
        )
    }

    #[test]
    fn roots_are_exactly_what_the_caller_passed() {
        let p = paths();
        assert_eq!(p.global_data_dir, PathBuf::from("/state/.agix"));
        assert_eq!(p.local_data_dir, PathBuf::from("/tmp/test-project/.agix"));
    }

    #[test]
    fn scene_dir_nests_under_global_dir() {
        let p = paths();
        assert_eq!(
            p.user_data_dir,
            p.global_data_dir.join("scenes").join("code")
        );
    }

    #[test]
    fn different_scopes_get_different_user_roots_but_same_global_root() {
        let global = PathBuf::from("/state/.agix");
        let local = PathBuf::from("/tmp/p/.agix");
        let code = ConfigPaths::new(global.clone(), local.clone(), "code", Layout::default());
        let ops = ConfigPaths::new(global, local, "ops", Layout::default());
        assert_ne!(code.user_data_dir, ops.user_data_dir);
        assert_eq!(code.global_data_dir, ops.global_data_dir);
        // Local root is scope-independent (project-level is flat).
        assert_eq!(code.local_data_dir, ops.local_data_dir);
    }

    /// The roots must survive the trip through `Settings` unchanged —
    /// `PathSettings` is how they actually reach most call sites.
    #[test]
    fn settings_roots_round_trip() {
        let p = paths();
        let carried = PathSettings {
            user_data_dir: p.user_data_dir.clone(),
            global_data_dir: p.global_data_dir.clone(),
            workspace: Workspace::project_of(&p.local_data_dir),
            local_data_dir: p.local_data_dir.clone(),
            scope: "code".into(),
            project_untrusted: false,
            worktree: None,
            layout: Layout::new(".codix").unwrap(),
        };
        let back = ConfigPaths::from_settings(&carried);
        assert_eq!(back.layout, carried.layout);
        assert_eq!(back.user_data_dir, p.user_data_dir);
        assert_eq!(back.global_data_dir, p.global_data_dir);
        assert_eq!(back.local_data_dir, p.local_data_dir);
    }

    #[test]
    fn convenience_methods_derive_from_roots() {
        let p = paths();
        assert_eq!(p.user_skills_dir(), p.user_data_dir.join("skills"));
        assert_eq!(p.global_skills_dir(), p.global_data_dir.join("skills"));
        assert_eq!(p.global_memory_dir(), p.global_data_dir.join("memory"));
        assert_eq!(
            p.global_recordings_dir(),
            p.global_data_dir.join("recordings")
        );
        assert_eq!(p.local_memory_dir(), p.local_data_dir.join("memory"));
    }

    #[test]
    fn the_project_tiers_paths_are_in_the_layouts_directory() {
        for layout in layout::test_layouts() {
            let name = layout.project_dir_name().to_string();
            let project = Path::new("/repo");
            let p = ConfigPaths::of(
                PathBuf::from("/state"),
                PathBuf::from("/state/scenes/code"),
                Some(project),
                layout,
            );
            let own = project.join(&name);
            assert_eq!(p.project_dir(), Some(own.as_path()));
            assert_eq!(
                p.settings_file(Tier::Project),
                Some(own.join("settings.json"))
            );
            assert_eq!(p.project_agents_dir(), Some(own.join("agents")));
            assert_eq!(p.project_hooks_dir(), Some(own.join("hooks")));
            assert_eq!(p.project_rules_dir(), Some(own.join("rules")));
            assert_eq!(
                p.project_output_styles_dir(),
                Some(own.join("output-styles"))
            );
            assert_eq!(p.imported_marker(), Some(own.join(".imported.json")));
            assert_eq!(p.worktrees_dir(), Some(own.join("worktrees")));
            assert_eq!(p.telemetry_dir(), own.join("telemetry"));
            assert_eq!(p.scratchpad_dir(), own.join("scratchpad"));
            assert_eq!(p.project_skills_dir(), Some(project.join(".agents/skills")));
            assert_eq!(
                p.settings_local_file(Tier::Scene),
                Some(PathBuf::from("/state/scenes/code/settings.local.json"))
            );
            assert_eq!(p.tier_dir(Tier::Managed), None);
        }
        let none = ConfigPaths::of(
            PathBuf::from("/state"),
            PathBuf::from("/state/scenes/code"),
            None,
            Layout::default(),
        );
        assert_eq!(none.project_dir(), None);
        assert_eq!(none.settings_file(Tier::Project), None);
        assert_eq!(none.local_data_dir, PathBuf::from("/state"));
    }
}

#[cfg(test)]
mod local_data_gitignore_tests {
    use super::*;

    #[test]
    fn a_gitignore_the_repository_ships_gets_the_engines_entries_appended() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".gitignore");
        std::fs::write(&file, "").unwrap();
        ensure_local_data_gitignore(dir.path());
        let lines: Vec<String> = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        for entry in local_data_gitignore() {
            assert!(lines.contains(&entry), "{entry} missing: {lines:?}");
        }

        std::fs::write(&file, "build/\nsettings.local.json").unwrap();
        ensure_local_data_gitignore(dir.path());
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.starts_with("build/\nsettings.local.json\n"),
            "{text:?}"
        );
        assert_eq!(text.matches("settings.local.json").count(), 1, "{text:?}");
        assert!(text.contains("mcp.local.json\n") && text.contains("telemetry/\n"));

        let before = text.clone();
        ensure_local_data_gitignore(dir.path());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }
}
