//! Data directory path abstraction.
//!
//! `ConfigPaths` provides a single source of truth for all persistent
//! directories used by the AGENT: a flat cross-scene **global** root
//! (conventionally `~/.agix/`), a **scene**-specific override root nested
//! under it (`<global>/scenes/<scope>/`), and a **local**/project root
//! (`<cwd>/.agix/`).
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

mod sensitive;

pub use sensitive::{
    credential_dir_names, session_scratch_dir, Credential, KeyFileNames, Located, PromptFiles,
    Scope, Secret, SensitivePaths, CREDENTIALS, ENGINE_CONFIG_DIRS, ENGINE_CONFIG_FILES,
    ENGINE_CONFIG_ROOTS, GIT_CONTROL_FILES, GIT_HOOKS_DIR, HOME_CREDENTIAL_STORES,
    PROJECT_ENGINE_CONFIG_DIRS, PROJECT_WORKTREES_DIR, REPOSITORY_CONFIG_DIR, SECRET_FILE_NAMES,
    STATE_ROOT_SCENE_FILES, STATE_ROOT_SCRATCHPAD, STATE_ROOT_SESSION_TMP, STATE_ROOT_WORKING_DIRS,
};

use std::path::{Path, PathBuf};

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

/// The shared, committed settings file present in every tier.
pub const SETTINGS_FILE: &str = "settings.json";

/// The gitignored per-machine overlay sitting beside `SETTINGS_FILE` in the
/// same tier, overriding it.
pub const SETTINGS_LOCAL_FILE: &str = "settings.local.json";

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
    /// The project tier's root: `<project>/.agix/` in a project, the state
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
}

/// Placeholder directories for settings that were not loaded from any — the
/// ones [`LoadedSettings::new`] is commonly given in tests.
impl Default for PathSettings {
    fn default() -> Self {
        Self {
            user_data_dir: PathBuf::from("~/.agix/scenes/agent"),
            global_data_dir: PathBuf::from("~/.agix"),
            local_data_dir: PathBuf::from("."),
            workspace: Workspace::Project(PathBuf::new()),
            scope: default_scope(),
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
    /// A project: the tools work in it, and its `.agix` is the project tier.
    Project(PathBuf),
    /// No project (`project_root: null`): no project tier, no git or
    /// `AGENTS.md` context, and the tools work in this directory of the
    /// session's own under the state root's scratchpad
    /// ([`crate::paths::session_scratch_dir`]) — never in the state root's
    /// parent, which for `~/.agix` is the user's home.
    Scratch(PathBuf),
}

impl Workspace {
    /// The project whose `.agix` is `local_data_dir`.
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
    ) -> Self {
        Self {
            user_data_dir,
            global_data_dir,
            local_data_dir,
            workspace,
            scope,
        }
    }

    /// The project, `None` in a session without one.
    pub fn project_root(&self) -> Option<&Path> {
        self.workspace.project_root()
    }

    /// The tools' working directory: the root a write needs no approval in,
    /// a read none either, and a sandboxed command may write.
    pub fn working_dir(&self) -> &Path {
        match &self.workspace {
            Workspace::Project(dir) | Workspace::Scratch(dir) => dir,
        }
    }

    /// The project tier's `settings.local.json` — the file the interactive
    /// permission prompt's "always allow, this project" writes to.
    ///
    /// Inside `local_data_dir` (`<project_root>/.agix/`), not beside it: the
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
/// directory sits in: the files the engine writes keys into, and its
/// telemetry recordings.
pub const LOCAL_DATA_GITIGNORE: &str = "settings.local.json\nmcp.local.json\ntelemetry/\n";

/// Give `dir` a `.gitignore` of [`LOCAL_DATA_GITIGNORE`] if it has none. One
/// already there is the user's, and is left as it is.
pub fn ensure_local_data_gitignore(dir: &std::path::Path) {
    use std::io::Write;
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(".gitignore"));
    if let Ok(mut file) = created {
        let _ = file.write_all(LOCAL_DATA_GITIGNORE.as_bytes());
    }
}

/// Unified management of AGENT persistent directory paths.
///
/// Default: `global_data_dir = $HOME/.agix/`
///          `user_data_dir   = $HOME/.agix/scenes/<scope>/`
///          `local_data_dir  = <cwd>/.agix/`
///
/// `global_data_dir` (and therefore `user_data_dir`, derived from it) can be
/// overridden via `AGIX_DATA_DIR`; `local_data_dir` via `AGIX_LOCAL_DATA_DIR`.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// User-level, scene-specific override root. Default `$HOME/.agix/scenes/<scope>/`.
    pub user_data_dir: PathBuf,
    /// User-level, cross-scene global root (flat, shared by every scene).
    /// Default `$HOME/.agix/`.
    pub global_data_dir: PathBuf,
    /// Local/project data root. Default `<cwd>/.agix/`.
    pub local_data_dir: PathBuf,
    /// The project, `None` for a session without one
    /// ([`crate::paths::PathSettings::project_root`]).
    pub project_root: Option<PathBuf>,
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
    pub fn new(global_data_dir: PathBuf, local_data_dir: PathBuf, scope: &str) -> Self {
        let project_root = Workspace::project_of(&local_data_dir)
            .project_root()
            .map(PathBuf::from);
        Self {
            user_data_dir: global_data_dir.join("scenes").join(scope),
            global_data_dir,
            local_data_dir,
            project_root,
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
        }
    }

    pub fn user_skills_dir(&self) -> PathBuf {
        self.user_data_dir.join("skills")
    }

    pub fn global_skills_dir(&self) -> PathBuf {
        self.global_data_dir.join("skills")
    }

    pub fn global_plugins_dir(&self) -> PathBuf {
        self.global_data_dir.join("plugins")
    }

    /// Cross-scene memory root — used when there's no specific project open
    /// (reserved for future no-project/desktop use; today sessions always
    /// have a project, so `local_memory_dir()` is what's actually read).
    pub fn global_memory_dir(&self) -> PathBuf {
        self.global_data_dir.join("memory")
    }

    pub fn global_recordings_dir(&self) -> PathBuf {
        self.global_data_dir.join("recordings")
    }

    pub fn local_memory_dir(&self) -> PathBuf {
        self.local_data_dir.join("memory")
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
        let code = ConfigPaths::new(global.clone(), local.clone(), "code");
        let ops = ConfigPaths::new(global, local, "ops");
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
        };
        let back = ConfigPaths::from_settings(&carried);
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
}
