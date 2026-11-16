//! The names of the engine's directories and of what is in them, and the
//! one value that varies between instances: what a project's directory is
//! called ([`Layout`], `.agix` unless the embedder names another).
//!
//! Every other module takes these names from here — the derived paths from
//! [`super::ConfigPaths`], [`Layout`] and [`StateRoot`], the security
//! decisions from [`super::SensitivePaths`] — so that renaming the project
//! directory is one value carried through, not a search for literals.
//! A `Layout` is carried by value from the pool's paths into every session's
//! settings; there is no process-wide one, since two pools in one process
//! may name their project directory differently.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// The project directory's name when the embedder names none.
pub const DEFAULT_PROJECT_DIR: &str = ".agix";

/// `$HOME/<this>` is agixd's state root when nothing names one.
pub const DEFAULT_STATE_DIR: &str = ".agix";

/// Configuration roots of other agents, or of this engine under its default
/// name, guarded beside the instance's own: a model that rewrote one would be
/// granting a later session of that agent, or the import, what it was not
/// granted.
pub const FOREIGN_CONFIG_ROOTS: &[&str] = &[DEFAULT_PROJECT_DIR, CLAUDE_DIR];

/// Claude Code's directory, which the engine imports from.
pub const CLAUDE_DIR: &str = ".claude";

/// Claude Code's login, in the user's home.
pub const CLAUDE_CREDENTIALS: &str = ".claude/.credentials.json";

/// The cross-tool directory beside the project directory; its `skills` is
/// [`PROJECT_SKILLS_DIR`]. Its name belongs to that convention, not to the
/// instance, and does not follow [`Layout`].
pub const AGENTS_DIR: &str = ".agents";

/// A project's skills, where Codex and other agent tools look for them too.
pub const PROJECT_SKILLS_DIR: &str = ".agents/skills";

/// The branch a worktree the engine makes is checked out on, before its name.
pub const WORKTREE_BRANCH_PREFIX: &str = "agix/worktree-";

/// The name a nested repository's `.git` is moved aside to, before its date.
pub const QUARANTINE_PREFIX: &str = ".git.agix-quarantine-";

/// The directory in a git directory holding the linked worktrees' entries.
pub const GIT_WORKTREES_DIR: &str = "worktrees";

/// Entries of an engine directory: the state root, a scene directory, a
/// project directory.
pub mod entry {
    pub const SETTINGS: &str = "settings.json";
    pub const SETTINGS_LOCAL: &str = "settings.local.json";
    pub const MCP_LOCAL: &str = "mcp.local.json";
    pub const SCENE_FILE: &str = "scene.json";
    pub const SCENE_PROMPT: &str = "prompt.md";
    pub const SCHEDULED_TASKS: &str = "scheduled_tasks.json";
    pub const IMPORTED_MARKER: &str = ".imported.json";

    pub const SKILLS: &str = "skills";
    pub const AGENTS: &str = "agents";
    pub const HOOKS: &str = "hooks";
    pub const RULES: &str = "rules";
    pub const OUTPUT_STYLES: &str = "output-styles";
    pub const PLUGINS: &str = "plugins";
    pub const CACHE: &str = "cache";
    pub const SCENES: &str = "scenes";
    pub const MCP: &str = "mcp";
    pub const OAUTH: &str = "oauth";
    pub const POLICY: &str = "policy";
    pub const SCRIPTS: &str = "scripts";

    pub const MEMORY: &str = "memory";
    pub const SCRATCHPAD: &str = "scratchpad";
    pub const TMP: &str = "tmp";
    pub const WORKTREES: &str = "worktrees";
    pub const TEAMS: &str = "teams";
    pub const TELEMETRY: &str = "telemetry";
    pub const TOOL_RESULTS: &str = "tool-results";
    pub const SESSIONS: &str = "sessions";
    pub const PROJECTS: &str = "projects";
    pub const RECORDINGS: &str = "recordings";

    pub const DAEMON: &str = "daemon";
    pub const INSTANCES: &str = "instances.d";
    pub const AUTH: &str = "auth";
    pub const GROUPS: &str = "groups";
    pub const CRASHES: &str = "crashes";
}

/// The shared, committed settings file present in every tier.
pub const SETTINGS_FILE: &str = entry::SETTINGS;

/// The gitignored per-machine overlay sitting beside `SETTINGS_FILE` in the
/// same tier, overriding it.
pub const SETTINGS_LOCAL_FILE: &str = entry::SETTINGS_LOCAL;

/// The state root's subdirectories that are the agent's own working space
/// rather than configuration: the only parts of it the agent's tools and its
/// sandboxed commands may write.
pub const STATE_ROOT_WORKING_DIRS: &[&str] = &[STATE_ROOT_SCRATCHPAD, entry::MEMORY];

/// The state root's scratchpad, one of [`STATE_ROOT_WORKING_DIRS`].
pub const STATE_ROOT_SCRATCHPAD: &str = entry::SCRATCHPAD;

/// Under the state root, one temporary directory per session,
/// `<state root>/tmp/<session>`: the session's sandboxed commands may write
/// their own and no other.
pub const STATE_ROOT_SESSION_TMP: &str = entry::TMP;

/// The directory in a project directory that holds the git worktrees
/// `EnterWorktree` creates: project work checked out elsewhere, not
/// configuration, so it is outside [`ENGINE_CONFIG_DIRS`].
pub const PROJECT_WORKTREES_DIR: &str = entry::WORKTREES;

/// Files in a configuration root that configure the engine. The write tools
/// refuse them anywhere under such a directory, a sandboxed command directly
/// in it.
pub const ENGINE_CONFIG_FILES: &[&str] =
    &[entry::SETTINGS, entry::SETTINGS_LOCAL, entry::MCP_LOCAL];

/// Directories directly in a configuration root that configure the engine or
/// that it acts on unsandboxed, guarded with everything under them.
pub const ENGINE_CONFIG_DIRS: &[&str] = &[
    entry::MCP,
    entry::HOOKS,
    entry::AGENTS,
    entry::SKILLS,
    entry::PLUGINS,
    entry::RULES,
    entry::SCENES,
    entry::POLICY,
    entry::SCRIPTS,
    entry::OUTPUT_STYLES,
    entry::TOOL_RESULTS,
];

/// Directories in a project, outside the configuration roots, that the
/// engine loads configuration from: the project's skills, whose
/// `allowed-tools` grant permissions. Guarded like a configuration
/// directory in those roots: a write tool asks, a sandboxed command cannot
/// write, and the directories above one cannot be moved aside.
pub const PROJECT_ENGINE_CONFIG_DIRS: &[&str] = &[PROJECT_SKILLS_DIR];

/// The files in the state root and in each `scenes/<scene>/` under it that a
/// sandboxed command must not rewrite: the settings, and — for a
/// declarative scene — the files that define it.
pub const STATE_ROOT_SCENE_FILES: &[&str] =
    &[entry::SETTINGS, entry::SCENE_FILE, entry::SCENE_PROMPT];

/// The files a project directory's `.gitignore` keeps out of the repository:
/// the ones the engine writes keys into.
pub const LOCAL_DATA_IGNORED_FILES: &[&str] = &[entry::SETTINGS_LOCAL, entry::MCP_LOCAL];

/// The directories it keeps out: telemetry recordings and saved tool results.
pub const LOCAL_DATA_IGNORED_DIRS: &[&str] = &[entry::TELEMETRY, entry::TOOL_RESULTS];

/// Project directories other tools own, which an instance's cannot be: git's,
/// the cross-tool skills directory, the other agents' configuration roots,
/// and the editors' and forges' settings, whose files would collide with the
/// engine's or become unwritable under its protection.
pub const OTHER_TOOLS_DIRS: &[&str] = &[
    super::REPOSITORY_CONFIG_DIR,
    AGENTS_DIR,
    CLAUDE_DIR,
    ".codex",
    ".cursor",
    ".github",
    ".vscode",
    ".idea",
];

/// Why a name cannot be a project directory's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{name}` cannot name the project directory: {reason}")]
pub struct LayoutError {
    pub name: String,
    pub reason: &'static str,
}

/// The names an instance gives its directories. One per pool, carried in the
/// pool's paths and every session's settings.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Layout {
    project_dir: Arc<str>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            project_dir: Arc::from(DEFAULT_PROJECT_DIR),
        }
    }
}

/// One configuration root as [`Layout::config_roots`] lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigRoot<'a> {
    pub name: &'a str,
    /// The instance's own project directory, rather than a foreign root.
    pub own: bool,
}

impl Layout {
    /// One path component of `[a-z0-9._-]`, at most 64 long: the
    /// configuration checks split shell text on any other character, and a
    /// lowercase-only name is the same directory on every filesystem. It
    /// starts with `.`: the engine deletes what it does not recognize under
    /// `<name>/worktrees/` and refuses writes to its configuration
    /// subdirectories, which a project's own `src` or `docs` must never be
    /// subject to, and a hidden directory is where tools keep their state.
    /// Not `.`, `..`, nor a directory that is another tool's
    /// ([`OTHER_TOOLS_DIRS`]).
    pub fn new(project_dir: &str) -> Result<Self, LayoutError> {
        let refuse = |reason| {
            Err(LayoutError {
                name: project_dir.to_string(),
                reason,
            })
        };
        if project_dir.is_empty() {
            return refuse("it is empty");
        }
        if project_dir.len() > 64 {
            return refuse("it is longer than 64 characters");
        }
        if !project_dir
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
        {
            return refuse("only lowercase letters, digits, `.`, `_` and `-` are allowed");
        }
        if project_dir == "." || project_dir == ".." {
            return refuse("it is not a directory of its own");
        }
        if !project_dir.starts_with('.') {
            return refuse(
                "it must start with `.`, as a directory a project does not use for its own files",
            );
        }
        if OTHER_TOOLS_DIRS.contains(&project_dir) {
            return refuse("that directory belongs to another tool");
        }
        Ok(Self {
            project_dir: Arc::from(project_dir),
        })
    }

    pub fn project_dir_name(&self) -> &str {
        &self.project_dir
    }

    /// `<project_root>/<name>`.
    pub fn project_dir(&self, project_root: &Path) -> PathBuf {
        project_root.join(&*self.project_dir)
    }

    /// `<repo_root>/<name>/worktrees`: where the engine's worktrees live.
    pub fn worktrees_dir(&self, repo_root: &Path) -> PathBuf {
        self.project_dir(repo_root).join(PROJECT_WORKTREES_DIR)
    }

    /// `<dir>/<name>/teams`.
    pub fn teams_dir(&self, dir: &Path) -> PathBuf {
        self.project_dir(dir).join(entry::TEAMS)
    }

    /// The repository `.gitignore` line keeping the project directory out.
    pub fn gitignore_entry(&self) -> String {
        format!("{}/", self.project_dir)
    }

    /// The configuration roots the engine guards in a directory: its own
    /// name first, then each of [`FOREIGN_CONFIG_ROOTS`] that is not it.
    pub fn config_roots(&self) -> impl Iterator<Item = ConfigRoot<'_>> {
        let own: &str = &self.project_dir;
        std::iter::once(ConfigRoot {
            name: own,
            own: true,
        })
        .chain(
            FOREIGN_CONFIG_ROOTS
                .iter()
                .filter(move |name| **name != own)
                .map(|name| ConfigRoot { name, own: false }),
        )
    }

    /// The directories an engine of this family writes a project's keys and
    /// worktrees to: the instance's own, and `.agix` when that is not it.
    pub fn engine_dir_names(&self) -> impl Iterator<Item = &str> {
        let own: &str = &self.project_dir;
        std::iter::once(own).chain((own != DEFAULT_PROJECT_DIR).then_some(DEFAULT_PROJECT_DIR))
    }

    /// Whether a path component, compared without case, is the instance's
    /// own project directory.
    pub fn is_own_dir_component(&self, component: &str) -> bool {
        component.to_lowercase() == *self.project_dir
    }

    /// What `path` names inside a git worktree an engine made,
    /// `<repo>/<engine dir>/worktrees/<w>/<rest>`, as the main checkout holds
    /// it: `<repo>/<rest>`, and for a worktree made inside another, each
    /// checkout out to the main one. Nothing is read from disk.
    pub fn main_checkout_copies(&self, path: &Path) -> Vec<PathBuf> {
        let mut copies = Vec::new();
        let mut current: Vec<Component<'_>> = path.components().collect();
        while let Some(at) = current.windows(3).rposition(|w| {
            self.engine_dir_names().any(|dir| w[0].as_os_str() == dir)
                && w[1].as_os_str() == PROJECT_WORKTREES_DIR
                && matches!(w[2], Component::Normal(_))
        }) {
            current.drain(at..at + 3);
            copies.push(current.iter().collect());
        }
        copies
    }

    /// `<repo>/<name>/worktrees/<w>` when `path` is in one of this
    /// instance's worktrees, the outermost when they nest.
    pub fn worktree_of(&self, path: &Path) -> Option<PathBuf> {
        let parts: Vec<Component<'_>> = path.components().collect();
        let at = parts.windows(2).position(|w| {
            w[0].as_os_str() == &*self.project_dir && w[1].as_os_str() == PROJECT_WORKTREES_DIR
        })?;
        Some(parts.get(..at + 3)?.iter().collect())
    }
}

/// An instance's state root: global settings, scenes, transcripts, plugins,
/// the daemon's files. The names under it do not follow [`Layout`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StateRoot(PathBuf);

impl StateRoot {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self(root.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn into_path(self) -> PathBuf {
        self.0
    }

    pub fn settings_file(&self) -> PathBuf {
        self.0.join(entry::SETTINGS)
    }

    pub fn settings_local_file(&self) -> PathBuf {
        self.0.join(entry::SETTINGS_LOCAL)
    }

    pub fn scenes_dir(&self) -> PathBuf {
        self.0.join(entry::SCENES)
    }

    pub fn scene_dir(&self, scene: &str) -> PathBuf {
        self.scenes_dir().join(scene)
    }

    /// `<scene dir>/scheduled_tasks.json`.
    pub fn scheduled_tasks_file(&self, scene: &str) -> PathBuf {
        self.scene_dir(scene).join(entry::SCHEDULED_TASKS)
    }

    pub fn plugins_dir(&self) -> PathBuf {
        self.0.join(entry::PLUGINS)
    }

    pub fn plugin_cache_dir(&self) -> PathBuf {
        self.plugins_dir().join(entry::CACHE)
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.0.join(entry::TMP)
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.0.join(entry::PROJECTS)
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.0.join(entry::SESSIONS)
    }

    pub fn recordings_dir(&self) -> PathBuf {
        self.0.join(entry::RECORDINGS)
    }

    pub fn daemon_dir(&self) -> PathBuf {
        self.0.join(entry::DAEMON)
    }

    pub fn instances_dir(&self) -> PathBuf {
        self.daemon_dir().join(entry::INSTANCES)
    }

    pub fn auth_dir(&self) -> PathBuf {
        self.daemon_dir().join(entry::AUTH)
    }

    pub fn groups_dir(&self) -> PathBuf {
        self.daemon_dir().join(entry::GROUPS)
    }

    /// `daemon/groups/<instance>.json`.
    pub fn groups_file(&self, instance: &str) -> PathBuf {
        self.groups_dir().join(format!("{instance}.json"))
    }

    pub fn crashes_dir(&self) -> PathBuf {
        self.daemon_dir().join(entry::CRASHES)
    }
}

impl AsRef<Path> for StateRoot {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// The two layouts every test of a security decision runs under: the
/// default, where `.agix` is both the instance's own and the foreign root,
/// and a renamed one, where they differ.
#[cfg(test)]
pub(crate) fn test_layouts() -> [Layout; 2] {
    [Layout::default(), Layout::new(".codix").unwrap()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_one_hidden_lowercase_component_of_no_other_tool() {
        for good in [".agix", ".codix", ".agent-state", ".x_1.d"] {
            assert_eq!(Layout::new(good).unwrap().project_dir_name(), good);
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "../x",
            ".Agix",
            ".git",
            ".agents",
            ".claude",
            "a b",
            "a\\b",
            "x$y",
            "src",
            "docs",
            "node_modules",
            "agent-state",
            ".github",
            ".vscode",
            ".idea",
            ".codex",
            ".cursor",
        ] {
            assert!(Layout::new(bad).is_err(), "{bad:?}");
        }
        assert!(Layout::new(&"a".repeat(65)).is_err());
        assert_eq!(Layout::default().project_dir_name(), DEFAULT_PROJECT_DIR);
    }

    #[test]
    fn its_own_root_comes_first_and_the_foreign_ones_follow_without_it() {
        let roots = |l: &Layout| -> Vec<(String, bool)> {
            l.config_roots()
                .map(|r| (r.name.to_string(), r.own))
                .collect()
        };
        assert_eq!(
            roots(&Layout::default()),
            [(".agix".into(), true), (".claude".into(), false)]
        );
        assert_eq!(
            roots(&Layout::new(".codix").unwrap()),
            [
                (".codix".into(), true),
                (".agix".into(), false),
                (".claude".into(), false)
            ]
        );
        assert_eq!(
            Layout::new(".codix")
                .unwrap()
                .engine_dir_names()
                .collect::<Vec<_>>(),
            [".codix", ".agix"]
        );
        assert_eq!(
            Layout::default().engine_dir_names().collect::<Vec<_>>(),
            [".agix"]
        );
    }

    #[test]
    fn the_derived_paths_follow_the_name() {
        for layout in test_layouts() {
            let name = layout.project_dir_name().to_string();
            let root = Path::new("/repo");
            assert_eq!(layout.project_dir(root), root.join(&name));
            assert_eq!(
                layout.worktrees_dir(root),
                root.join(&name).join("worktrees")
            );
            assert_eq!(layout.teams_dir(root), root.join(&name).join("teams"));
            assert_eq!(layout.gitignore_entry(), format!("{name}/"));
            assert!(layout.is_own_dir_component(&name.to_uppercase()));
        }
    }

    #[test]
    fn a_path_in_a_worktree_has_its_main_checkout_copies() {
        for layout in test_layouts() {
            let name = layout.project_dir_name();
            let nested = format!("/repo/{name}/worktrees/a/{name}/worktrees/b/src/x.rs");
            assert_eq!(
                layout.main_checkout_copies(Path::new(&nested)),
                [
                    PathBuf::from(format!("/repo/{name}/worktrees/a/src/x.rs")),
                    PathBuf::from("/repo/src/x.rs")
                ]
            );
            assert_eq!(
                layout.worktree_of(Path::new(&nested)),
                Some(PathBuf::from(format!("/repo/{name}/worktrees/a")))
            );
            assert!(layout
                .main_checkout_copies(Path::new(&format!("/repo/{name}/worktrees")))
                .is_empty());
            assert_eq!(layout.worktree_of(Path::new("/repo/src/x.rs")), None);
            assert_eq!(
                layout.main_checkout_copies(Path::new(&format!("/repo/{name}/worktrees/wt"))),
                [PathBuf::from("/repo")]
            );
            assert!(layout
                .main_checkout_copies(Path::new(&format!("/repo/{name}/sessions/s/x")))
                .is_empty());
            assert!(layout
                .main_checkout_copies(Path::new("/repo/src/a"))
                .is_empty());
        }
        let codix = Layout::new(".codix").unwrap();
        assert_eq!(
            codix.main_checkout_copies(Path::new("/repo/.agix/worktrees/a/x")),
            [PathBuf::from("/repo/x")],
            "a worktree an agixd made is still an alias of the main checkout"
        );
        assert_eq!(
            codix.worktree_of(Path::new("/repo/.agix/worktrees/a/x")),
            None
        );
        assert!(Layout::default()
            .main_checkout_copies(Path::new("/repo/.codix/worktrees/a/x"))
            .is_empty());
    }

    #[test]
    fn the_state_roots_entries_are_under_it() {
        let root = StateRoot::new("/state");
        assert_eq!(root.scene_dir("code"), Path::new("/state/scenes/code"));
        assert_eq!(
            root.scheduled_tasks_file("code"),
            Path::new("/state/scenes/code/scheduled_tasks.json")
        );
        assert_eq!(
            root.groups_file("desktop"),
            Path::new("/state/daemon/groups/desktop.json")
        );
        assert_eq!(root.instances_dir(), Path::new("/state/daemon/instances.d"));
        assert_eq!(root.plugin_cache_dir(), Path::new("/state/plugins/cache"));
    }
}
