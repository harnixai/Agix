//! The names of the engine's directories and of what is in them, and the
//! values that vary between instances: what the engine is called in scripts
//! and file names, what a project's directory is called, what the product is
//! called, and what the repository directory the engine's worktrees go in is
//! called ([`Layout`]: `agix`, `.agix`, `Agix` and `.wkts` unless the
//! embedder or the user's settings name others).
//!
//! Every other module takes these names from here — the derived paths from
//! [`super::ConfigPaths`], [`Layout`] and [`StateRoot`], the security
//! decisions from [`super::SensitivePaths`], the engine's name wherever a
//! script finds it, the product's name wherever a person or the model reads
//! it — so that renaming any of them is one value
//! carried through, not a search for literals. A `Layout` is carried by value
//! from the pool's paths into every session's settings; there is no
//! process-wide one, since two pools in one process may name these
//! differently.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// The engine's name when the embedder names none.
pub const DEFAULT_ENGINE: &str = "agix";

/// The project directory's name under [`DEFAULT_ENGINE`]: `.<engine>`.
pub const DEFAULT_PROJECT_DIR: &str = ".agix";

/// `$HOME/<this>` is agixd's state root when nothing names one, and the
/// state directory's name under [`DEFAULT_ENGINE`].
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

/// The product's name when the embedder names none.
pub const DEFAULT_PRODUCT_NAME: &str = "Agix";

/// Where the product of that name is published.
pub const DEFAULT_PRODUCT_URL: &str = "https://github.com/openatta/agix";

/// The directory in a git directory holding the linked worktrees' entries.
pub const GIT_WORKTREES_DIR: &str = "worktrees";

/// The directory at a repository's root that the engine's worktrees go in
/// when the settings name none.
pub const DEFAULT_WORKTREES_DIR: &str = ".wkts";

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
    pub const COMMANDS: &str = "commands";
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
    pub const TEAMS: &str = "teams";
    pub const TELEMETRY: &str = "telemetry";
    pub const TOOL_RESULTS: &str = "tool-results";
    pub const STATE: &str = "state";
    pub const HISTORY: &str = "history";
    pub const SESSIONS: &str = "sessions";
    pub const PROJECTS: &str = "projects";
    pub const RECORDINGS: &str = "recordings";
    pub const QUOTA: &str = "quota";

    pub const DAEMON: &str = "daemon";
    pub const INSTANCES: &str = "instances.d";
    pub const AUTH: &str = "auth";
    pub const GROUPS: &str = "groups";
    pub const CRASHES: &str = "crashes";

    pub const MANAGED_SETTINGS: &str = "managed-settings.json";
    pub const MCP_POLICY: &str = "mcp-policy.json";
}

/// The shared, committed settings file present in every tier.
pub const SETTINGS_FILE: &str = entry::SETTINGS;

/// The gitignored per-machine overlay sitting beside `SETTINGS_FILE` in the
/// same tier, overriding it.
pub const SETTINGS_LOCAL_FILE: &str = entry::SETTINGS_LOCAL;

/// The subdirectories of the state root, and of a project directory's
/// `state/`, that are the agent's own working space rather than
/// configuration: the only parts of them the agent's tools and its sandboxed
/// commands may write.
pub const STATE_ROOT_WORKING_DIRS: &[&str] = &[STATE_ROOT_SCRATCHPAD, entry::MEMORY];

/// The state root's scratchpad, one of [`STATE_ROOT_WORKING_DIRS`].
pub const STATE_ROOT_SCRATCHPAD: &str = entry::SCRATCHPAD;

/// Under the state root, one temporary directory per session,
/// `<state root>/tmp/<session>`: the session's sandboxed commands may write
/// their own and no other.
pub const STATE_ROOT_SESSION_TMP: &str = entry::TMP;

/// Files in a configuration root that configure the engine. The write tools
/// refuse them anywhere under such a directory, a sandboxed command directly
/// in it.
pub const ENGINE_CONFIG_FILES: &[&str] =
    &[entry::SETTINGS, entry::SETTINGS_LOCAL, entry::MCP_LOCAL];

/// Directories directly in a configuration root that configure the engine or
/// that it acts on unsandboxed, guarded with everything under them. With
/// `commands`, which the engine does not read but Claude Code reads its slash
/// commands from.
pub const ENGINE_CONFIG_DIRS: &[&str] = &[
    entry::MCP,
    entry::HOOKS,
    entry::COMMANDS,
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

/// The files of a project directory outside its `state/` that its
/// `.gitignore` keeps out of the repository along with `state/`: one user's
/// own configuration and keys, which stay beside the shared settings where
/// people look for them. Only what a project's collaborators share is
/// committed.
pub const LOCAL_CONFIG_FILES: &[&str] = &[entry::SETTINGS_LOCAL, entry::MCP_LOCAL];

/// What the engine keeps in a project directory's `state/` and an older
/// engine kept directly in the project directory: opening the project moves
/// each found there into `state/`.
pub const PROJECT_STATE_ENTRIES: &[&str] = &[
    entry::MEMORY,
    entry::SCRATCHPAD,
    entry::TEAMS,
    entry::TELEMETRY,
    entry::TOOL_RESULTS,
    entry::IMPORTED_MARKER,
];

/// The directories in a project's `state/` the engine acts on unsandboxed —
/// it exports, forks and resumes from them — guarded with everything under
/// them even inside a writable root the sandbox policy names there.
pub const PROJECT_STATE_ENGINE_DIRS: &[&str] = &[entry::TOOL_RESULTS, entry::HISTORY];

/// The names the engine has, or keeps for itself, in a project's `state/`.
/// No product's id may be one: `state/<product id>/` is the embedder's
/// ([`Layout::host_state_dir`]).
pub const STATE_RESERVED_NAMES: &[&str] = &[
    entry::MEMORY,
    entry::SCRATCHPAD,
    entry::TEAMS,
    entry::TELEMETRY,
    entry::TOOL_RESULTS,
    entry::HISTORY,
    "imported",
    entry::SESSIONS,
    entry::CACHE,
    entry::TMP,
];

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

/// Why a name cannot be the worktrees directory's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{name}` cannot name the worktrees directory: {reason}")]
pub struct WorktreesDirError {
    pub name: String,
    pub reason: &'static str,
}

/// Why a name cannot be the engine's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{name}` cannot name the engine: {reason}")]
pub struct EngineNameError {
    pub name: String,
    pub reason: &'static str,
}

/// Why a name cannot be the product's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{name}` cannot name the product: {reason}")]
pub struct ProductNameError {
    pub name: String,
    pub reason: &'static str,
}

/// The names an instance gives its directories and itself. One per pool,
/// carried in the pool's paths and every session's settings.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Layout {
    engine: Arc<str>,
    project_dir: Arc<str>,
    product: Arc<str>,
    worktrees: Arc<str>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            engine: Arc::from(DEFAULT_ENGINE),
            project_dir: Arc::from(DEFAULT_PROJECT_DIR),
            product: Arc::from(DEFAULT_PRODUCT_NAME),
            worktrees: Arc::from(DEFAULT_WORKTREES_DIR),
        }
    }
}

/// Why `name` is not one path component of at most 64 characters, each
/// `allowed` (else `charset` says why), that starts with `.`.
fn not_hidden_component(
    name: &str,
    allowed: fn(char) -> bool,
    charset: &'static str,
) -> Option<&'static str> {
    if name.is_empty() {
        Some("it is empty")
    } else if name.len() > 64 {
        Some("it is longer than 64 characters")
    } else if !name.chars().all(allowed) {
        Some(charset)
    } else if name == "." || name == ".." {
        Some("it is not a directory of its own")
    } else if !name.starts_with('.') {
        Some("it must start with `.`, as a directory a project does not use for its own files")
    } else {
        None
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
    /// The default layout with its project directory `project_dir`
    /// ([`Self::with_project_dir`]).
    pub fn new(project_dir: &str) -> Result<Self, LayoutError> {
        Self::default().with_project_dir(project_dir)
    }

    /// This layout with its project directory `<project>/<name>`: one path
    /// component of `[a-z0-9._-]`, at most 64 long — the configuration
    /// checks split shell text on any other character, and a lowercase-only
    /// name is the same directory on every filesystem. It starts with `.`:
    /// the engine refuses writes to its configuration subdirectories, which
    /// a project's own `src` or `docs` must never be subject to, and a
    /// hidden directory is where tools keep their state. Not `.`, `..`, a
    /// directory that is another tool's ([`OTHER_TOOLS_DIRS`]), nor where
    /// the worktrees go.
    pub fn with_project_dir(self, name: &str) -> Result<Self, LayoutError> {
        match self.refuse_project_dir(name) {
            Some(reason) => Err(LayoutError {
                name: name.to_string(),
                reason,
            }),
            None => Ok(Self {
                project_dir: Arc::from(name),
                ..self
            }),
        }
    }

    fn refuse_project_dir(&self, name: &str) -> Option<&'static str> {
        let lowercase = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c);
        not_hidden_component(
            name,
            lowercase,
            "only lowercase letters, digits, `.`, `_` and `-` are allowed",
        )
        .or_else(|| {
            OTHER_TOOLS_DIRS
                .contains(&name)
                .then_some("that directory belongs to another tool")
        })
        .or_else(|| {
            (name == DEFAULT_WORKTREES_DIR || super::fold_case(&self.worktrees) == name)
                .then_some("that is where the engine's worktrees go")
        })
    }

    /// This layout under the engine name `name`, with its project directory
    /// `.<name>`: name another with [`Self::with_project_dir`] after this.
    /// `[a-z][a-z0-9]*`, at most 16 long, and a name a script can be given
    /// as a new global ([`crate::interface::script::check_global_name`]):
    /// the base library is installed under it. `.<name>` must be a project
    /// directory [`Self::with_project_dir`] accepts.
    pub fn with_engine(self, name: &str) -> Result<Self, EngineNameError> {
        let refuse = |reason| {
            Err(EngineNameError {
                name: name.to_string(),
                reason,
            })
        };
        if name.len() > 16 {
            return refuse("it is longer than 16 characters");
        }
        if let Err(reason) = crate::interface::script::check_global_name(name) {
            return refuse(reason);
        }
        let project_dir = format!(".{name}");
        if self.refuse_project_dir(&project_dir).is_some() {
            return refuse("its project directory would be another tool's, or the worktrees'");
        }
        Ok(Self {
            engine: Arc::from(name),
            project_dir: Arc::from(project_dir),
            ..self
        })
    }

    /// This layout with its worktrees in `<repository>/<name>/`: one path
    /// component of ASCII letters, digits, `.`, `_` and `-`, at most 64 long,
    /// starting with `.` — the repository's `.gitignore` ignores it, every
    /// entry in it counts as a copy of the main checkout, and the sandbox
    /// keeps commands from adding, renaming or removing those entries, none
    /// of which a project's own `src` must be subject to. Not `.`, `..`, a
    /// configuration root ([`Self::config_roots`]), a directory another tool
    /// owns ([`OTHER_TOOLS_DIRS`]) nor anything starting with `.git`, which
    /// git and the forges keep their files under, compared without case: a
    /// worktree in one would sit inside configuration the sandbox shuts.
    pub fn with_worktrees_dir(self, name: &str) -> Result<Self, WorktreesDirError> {
        let ascii = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
        let folded = super::fold_case(name);
        let reason = not_hidden_component(
            name,
            ascii,
            "only ASCII letters, digits, `.`, `_` and `-` are allowed",
        )
        .or_else(|| {
            self.config_roots()
                .any(|root| root.name == folded)
                .then_some("that directory is a configuration root")
        })
        .or_else(|| {
            (OTHER_TOOLS_DIRS.contains(&folded.as_str())
                || folded.starts_with(super::REPOSITORY_CONFIG_DIR))
            .then_some("that directory belongs to another tool")
        });
        match reason {
            Some(reason) => Err(WorktreesDirError {
                name: name.to_string(),
                reason,
            }),
            None => Ok(Self {
                worktrees: Arc::from(name),
                ..self
            }),
        }
    }

    /// This layout under the product name `name`: an ASCII letter, then up
    /// to 31 ASCII letters, digits, `-` or `_`. Its lowercase form is a path
    /// component (`/etc/<name>`, `state/<name>`), a git ref component
    /// (`<name>/worktree-`) and an HTTP token (`<name>/<version>`), all of
    /// which these characters are safe in, and none of
    /// [`STATE_RESERVED_NAMES`].
    pub fn with_product_name(self, name: &str) -> Result<Self, ProductNameError> {
        let refuse = |reason| {
            Err(ProductNameError {
                name: name.to_string(),
                reason,
            })
        };
        let mut chars = name.chars();
        match chars.next() {
            None => return refuse("it is empty"),
            Some(c) if !c.is_ascii_alphabetic() => {
                return refuse("it must start with an ASCII letter")
            }
            Some(_) => {}
        }
        if name.len() > 32 {
            return refuse("it is longer than 32 characters");
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return refuse("only ASCII letters, digits, `-` and `_` are allowed");
        }
        if STATE_RESERVED_NAMES.contains(&name.to_ascii_lowercase().as_str()) {
            return refuse(
                "the engine keeps its own state under that name in a project's `state/`",
            );
        }
        Ok(Self {
            product: Arc::from(name),
            ..self
        })
    }

    /// The engine's name in scripts and file names: the base library's
    /// global, the start of its errors' name.
    pub fn engine(&self) -> &str {
        &self.engine
    }

    /// The directories under `$HOME` instances keep their state in unless
    /// told otherwise: `.<engine>` for this engine's, then agixd's
    /// [`DEFAULT_STATE_DIR`] when that is not it.
    pub fn state_dir_names(&self) -> impl Iterator<Item = String> {
        let own = format!(".{}", self.engine);
        let agixd = (own != DEFAULT_STATE_DIR).then(|| DEFAULT_STATE_DIR.to_string());
        std::iter::once(own).chain(agixd)
    }

    pub fn project_dir_name(&self) -> &str {
        &self.project_dir
    }

    /// The name of the repository directory the engine's worktrees go in.
    pub fn worktrees_dir_name(&self) -> &str {
        &self.worktrees
    }

    /// The product's name as people read it: in prompts, messages, a
    /// consent page, a notification.
    pub fn product_name(&self) -> &str {
        &self.product
    }

    /// The product's name where a machine reads it: lowercase.
    pub fn product_id(&self) -> String {
        self.product.to_ascii_lowercase()
    }

    /// `<product id>/<version>`, the `User-Agent` of the engine's requests.
    pub fn user_agent(&self) -> String {
        format!("{}/{}", self.product_id(), env!("CARGO_PKG_VERSION"))
    }

    /// The branch a worktree the engine makes is checked out on, before its
    /// name: `<product id>/worktree-`.
    pub fn worktree_branch_prefix(&self) -> String {
        format!("{}/worktree-", self.product_id())
    }

    /// The name a nested repository's `.git` is moved aside to, before its
    /// date: `.git.<product id>-quarantine-`.
    pub fn quarantine_prefix(&self) -> String {
        format!(".git.{}-quarantine-", self.product_id())
    }

    /// The product's machine-wide directory, where a deployment puts what
    /// stands above every user and project: macOS `/Library/Application
    /// Support/<name>`, Windows `%ProgramData%\<name>`, elsewhere
    /// `/etc/<id>`. Only this product's: a renamed product has no reason to
    /// read another's, and its name cannot spell a path out of the parent.
    pub fn machine_dir(&self) -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            PathBuf::from("/Library/Application Support").join(self.product_name())
        }
        #[cfg(windows)]
        {
            #[expect(
                clippy::disallowed_methods,
                reason = "where Windows keeps machine-wide program data is a fact about this machine"
            )]
            let root = std::env::var_os("ProgramData")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
            root.join(self.product_name())
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            PathBuf::from("/etc").join(self.product_id())
        }
    }

    /// `<machine dir>/managed-settings.json`: the managed settings layer.
    pub fn managed_settings_file(&self) -> PathBuf {
        self.machine_dir().join(entry::MANAGED_SETTINGS)
    }

    /// `<machine dir>/mcp-policy.json`: which MCP servers may connect.
    pub fn mcp_policy_file(&self) -> PathBuf {
        self.machine_dir().join(entry::MCP_POLICY)
    }

    /// The product as a commit or pull request the model writes credits it:
    /// a link where it is published, else its name.
    pub fn attribution(&self) -> String {
        if &*self.product == DEFAULT_PRODUCT_NAME {
            format!("[{DEFAULT_PRODUCT_NAME}]({DEFAULT_PRODUCT_URL})")
        } else {
            self.product.to_string()
        }
    }

    /// `<project_root>/<name>`.
    pub fn project_dir(&self, project_root: &Path) -> PathBuf {
        project_root.join(&*self.project_dir)
    }

    /// `<repo_root>/<worktrees dir>`: where the engine makes its worktrees.
    pub fn worktrees_dir(&self, repo_root: &Path) -> PathBuf {
        repo_root.join(&*self.worktrees)
    }

    /// The directories at a repository's root that hold the engine's
    /// worktrees: this layout's, then [`DEFAULT_WORKTREES_DIR`] when that is
    /// not it — where the worktrees made before the settings named another
    /// directory, or by an instance whose settings name none, are.
    pub fn worktrees_dir_names(&self) -> impl Iterator<Item = &str> {
        let own: &str = &self.worktrees;
        std::iter::once(own).chain((own != DEFAULT_WORKTREES_DIR).then_some(DEFAULT_WORKTREES_DIR))
    }

    /// `<project_root>/<name>/state`: what the engine keeps in the project
    /// for this machine alone, all of it out of the repository.
    pub fn project_state_dir(&self, project_root: &Path) -> PathBuf {
        self.project_dir(project_root).join(entry::STATE)
    }

    /// `<project_root>/<name>/state/<product id>`: the embedder's own state
    /// in the project, which the engine never writes and guards as it
    /// guards its own.
    pub fn host_state_dir(&self, project_root: &Path) -> PathBuf {
        self.project_state_dir(project_root).join(self.product_id())
    }

    /// `<dir>/<name>/state/teams`.
    pub fn teams_dir(&self, dir: &Path) -> PathBuf {
        self.project_state_dir(dir).join(entry::TEAMS)
    }

    /// The repository `.gitignore` line keeping the worktrees directory out.
    pub fn gitignore_entry(&self) -> String {
        format!("{}/", self.worktrees)
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

    /// The directories an engine of this family writes a project's keys to:
    /// the instance's own, and `.agix` when that is not it.
    pub fn engine_dir_names(&self) -> impl Iterator<Item = &str> {
        let own: &str = &self.project_dir;
        std::iter::once(own).chain((own != DEFAULT_PROJECT_DIR).then_some(DEFAULT_PROJECT_DIR))
    }

    /// Whether a path component, compared without case, is the instance's
    /// own project directory.
    pub fn is_own_dir_component(&self, component: &str) -> bool {
        super::fold_case(component) == *self.project_dir
    }

    /// What `path` names inside a git worktree an engine made,
    /// `<repo>/<worktrees dir>/<w>/<rest>` for any of
    /// [`Self::worktrees_dir_names`], as the main checkout holds it:
    /// `<repo>/<rest>`, and for a worktree made inside another, each checkout
    /// out to the main one. Nothing is read from disk: without case.
    pub fn main_checkout_copies(&self, path: &Path) -> Vec<PathBuf> {
        let mut copies = Vec::new();
        let mut current: Vec<Component<'_>> = path.components().collect();
        while let Some(at) = current.windows(2).rposition(|w| self.opens_a_worktree(w)) {
            current.drain(at..at + 2);
            copies.push(current.iter().collect());
        }
        copies
    }

    /// `<repo>/<worktrees dir>/<w>` when `path` is in one of the engine's
    /// worktrees, the outermost when they nest.
    pub fn worktree_of(&self, path: &Path) -> Option<PathBuf> {
        let parts: Vec<Component<'_>> = path.components().collect();
        let at = parts.windows(2).position(|w| self.opens_a_worktree(w))?;
        Some(parts.get(..at + 2)?.iter().collect())
    }

    fn opens_a_worktree(&self, pair: &[Component<'_>]) -> bool {
        let [Component::Normal(dir), Component::Normal(_)] = pair else {
            return false;
        };
        let dir = super::fold_case(&dir.to_string_lossy());
        self.worktrees_dir_names()
            .any(|name| super::fold_case(name) == dir)
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
/// and a renamed one, where they differ and the worktrees are elsewhere.
#[cfg(test)]
pub(crate) fn test_layouts() -> [Layout; 2] {
    [
        Layout::default(),
        Layout::new(".codix")
            .unwrap()
            .with_worktrees_dir(".trees")
            .unwrap(),
    ]
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
        assert!(Layout::new(DEFAULT_WORKTREES_DIR).is_err());
        assert_eq!(Layout::default().project_dir_name(), DEFAULT_PROJECT_DIR);
    }

    #[test]
    fn a_worktrees_dir_is_one_hidden_component_of_no_configuration_root() {
        for good in [
            ".wkts",
            ".trees",
            ".Trees",
            ".work_trees-2",
            &format!(".{}", "a".repeat(63)),
        ] {
            let layout = Layout::default().with_worktrees_dir(good).unwrap();
            assert_eq!(layout.worktrees_dir_name(), good);
            assert_eq!(layout.project_dir_name(), DEFAULT_PROJECT_DIR);
        }
        let codix = Layout::new(".codix").unwrap();
        for bad in [
            "",
            ".",
            "..",
            "wkts",
            "a/b",
            ".a/b",
            "../x",
            ".a b",
            ".a\\b",
            ".x$y",
            ".tr\u{e9}es",
            ".git",
            ".GIT",
            ".gitignore",
            ".gitmodules",
            ".GitAttributes",
            ".git-blame-ignore-revs",
            ".gitlab",
            ".agix",
            ".AGIX",
            ".claude",
            ".Claude",
            ".agents",
            ".github",
            ".vscode",
            ".idea",
            ".codex",
            ".cursor",
            &format!(".{}", "a".repeat(64)),
        ] {
            assert!(
                Layout::default().with_worktrees_dir(bad).is_err(),
                "{bad:?}"
            );
            assert!(codix.clone().with_worktrees_dir(bad).is_err(), "{bad:?}");
        }
        assert!(codix.clone().with_worktrees_dir(".codix").is_err());
        assert!(codix.clone().with_worktrees_dir(".Codix").is_err());
        assert!(Layout::default().with_worktrees_dir(".codix").is_ok());
        assert_eq!(
            Layout::default().worktrees_dir_name(),
            DEFAULT_WORKTREES_DIR
        );
        let named = Layout::new(".codix")
            .unwrap()
            .with_product_name("Codix")
            .unwrap()
            .with_worktrees_dir(".trees")
            .unwrap();
        assert_eq!(
            (named.project_dir_name(), named.product_name()),
            (".codix", "Codix")
        );
        assert_eq!(
            named
                .with_product_name("Other")
                .unwrap()
                .worktrees_dir_name(),
            ".trees"
        );
    }

    #[test]
    fn a_product_name_is_a_letter_then_letters_digits_dashes_underscores() {
        for good in ["Agix", "Codix", "x", "My-Agent_2", &"A".repeat(32)] {
            let layout = Layout::default().with_product_name(good).unwrap();
            assert_eq!(layout.product_name(), good);
            assert_eq!(layout.project_dir_name(), DEFAULT_PROJECT_DIR);
        }
        for bad in [
            "",
            "1x",
            "-x",
            "a b",
            "a/b",
            "a.b",
            "a..b",
            "a@b",
            "Ägix",
            &"A".repeat(33),
        ] {
            assert!(Layout::default().with_product_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(Layout::default().product_name(), DEFAULT_PRODUCT_NAME);
    }

    #[test]
    fn a_product_id_is_none_of_the_names_the_engine_keeps_in_state() {
        for reserved in STATE_RESERVED_NAMES
            .iter()
            .filter(|n| !n.starts_with('.') && !n.contains('-'))
        {
            let capitalised = format!("{}{}", reserved[..1].to_uppercase(), &reserved[1..]);
            for name in [reserved.to_string(), capitalised, reserved.to_uppercase()] {
                assert!(
                    Layout::default().with_product_name(&name).is_err(),
                    "{name}"
                );
            }
        }
        for name in ["Tool-Results", "History", "Imported"] {
            assert!(Layout::default().with_product_name(name).is_err(), "{name}");
        }
        for entry in PROJECT_STATE_ENTRIES
            .iter()
            .chain(PROJECT_STATE_ENGINE_DIRS)
        {
            let id = entry.trim_start_matches('.').trim_end_matches(".json");
            assert!(STATE_RESERVED_NAMES.contains(&id), "{entry}");
        }
        assert!(!STATE_RESERVED_NAMES.contains(&DEFAULT_PRODUCT_NAME.to_ascii_lowercase().as_str()));
        let codix = Layout::default().with_product_name("Codix").unwrap();
        assert_eq!(
            codix.host_state_dir(Path::new("/repo")),
            Path::new("/repo/.agix/state/codix")
        );
    }

    #[test]
    fn an_engine_name_is_a_lowercase_script_global_and_names_the_project_directory() {
        let agix = Layout::default();
        assert_eq!(agix.engine(), DEFAULT_ENGINE);
        assert_eq!(format!(".{DEFAULT_ENGINE}"), DEFAULT_PROJECT_DIR);
        assert_eq!(agix.state_dir_names().collect::<Vec<_>>(), [".agix"]);
        for good in ["foo", "x", "codix2", &"a".repeat(16)] {
            let layout = Layout::default().with_engine(good).unwrap();
            assert_eq!(layout.engine(), good);
            assert_eq!(layout.project_dir_name(), format!(".{good}"));
            assert_eq!(layout.product_name(), DEFAULT_PRODUCT_NAME);
        }
        let foo = Layout::default().with_engine("foo").unwrap();
        assert_eq!(foo.state_dir_names().collect::<Vec<_>>(), [".foo", ".agix"]);
        assert_eq!(
            foo.config_roots().map(|r| r.name).collect::<Vec<_>>(),
            [".foo", ".agix", ".claude"]
        );
        let named = foo.with_project_dir(".codix").unwrap();
        assert_eq!(
            (named.engine(), named.project_dir_name()),
            ("foo", ".codix")
        );
        for bad in [
            "",
            "Foo",
            "1x",
            "a-b",
            "a_b",
            "a.b",
            "f\u{f6}o",
            "class",
            "new",
            "tools",
            "console",
            "escape",
            "constructor",
            "git",
            "claude",
            "codex",
            "wkts",
            &"a".repeat(17),
        ] {
            assert!(Layout::default().with_engine(bad).is_err(), "{bad:?}");
        }
        let trees = Layout::default().with_worktrees_dir(".foo").unwrap();
        assert!(trees.clone().with_engine("foo").is_err());
        assert!(trees.with_project_dir(".foo").is_err());
    }

    #[test]
    fn what_a_product_name_names_follows_it() {
        let codix = Layout::new(".codix")
            .unwrap()
            .with_product_name("Codix")
            .unwrap();
        assert_eq!(codix.project_dir_name(), ".codix");
        assert_eq!(codix.product_id(), "codix");
        assert_eq!(
            codix.user_agent(),
            format!("codix/{}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(codix.worktree_branch_prefix(), "codix/worktree-");
        assert_eq!(codix.quarantine_prefix(), ".git.codix-quarantine-");
        assert_eq!(codix.attribution(), "Codix");
        let agix = Layout::default();
        assert_eq!(agix.worktree_branch_prefix(), "agix/worktree-");
        assert_eq!(agix.quarantine_prefix(), ".git.agix-quarantine-");
        assert_eq!(
            agix.user_agent(),
            format!("agix/{}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            agix.attribution(),
            "[Agix](https://github.com/openatta/agix)"
        );
        for layout in [&agix, &codix] {
            assert!(layout.machine_dir().is_absolute());
            assert_eq!(
                layout.managed_settings_file(),
                layout.machine_dir().join("managed-settings.json")
            );
            assert_eq!(
                layout.mcp_policy_file(),
                layout.machine_dir().join("mcp-policy.json")
            );
        }
        assert!(codix
            .machine_dir()
            .to_string_lossy()
            .to_lowercase()
            .ends_with("codix"));
        assert!(!codix
            .machine_dir()
            .to_string_lossy()
            .to_lowercase()
            .contains("agix"));
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
            let worktrees = layout.worktrees_dir_name().to_string();
            let root = Path::new("/repo");
            assert_eq!(layout.project_dir(root), root.join(&name));
            assert_eq!(layout.worktrees_dir(root), root.join(&worktrees));
            assert_eq!(
                layout.project_state_dir(root),
                root.join(&name).join("state")
            );
            assert_eq!(layout.teams_dir(root), root.join(&name).join("state/teams"));
            assert_eq!(
                layout.host_state_dir(root),
                root.join(&name).join("state").join(layout.product_id())
            );
            assert_eq!(layout.gitignore_entry(), format!("{worktrees}/"));
            assert!(layout.is_own_dir_component(&name.to_uppercase()));
        }
        assert_eq!(
            Layout::default().worktrees_dir(Path::new("/repo")),
            Path::new("/repo/.wkts")
        );
        assert_eq!(Layout::default().gitignore_entry(), ".wkts/");
        assert_eq!(
            Layout::default().worktrees_dir_names().collect::<Vec<_>>(),
            [".wkts"]
        );
        assert_eq!(
            Layout::default()
                .with_worktrees_dir(".trees")
                .unwrap()
                .worktrees_dir_names()
                .collect::<Vec<_>>(),
            [".trees", ".wkts"]
        );
    }

    #[test]
    fn a_path_in_a_worktree_has_its_main_checkout_copies() {
        for layout in test_layouts() {
            let wk = layout.worktrees_dir_name();
            let nested = format!("/repo/{wk}/a/{wk}/b/src/x.rs");
            assert_eq!(
                layout.main_checkout_copies(Path::new(&nested)),
                [
                    PathBuf::from(format!("/repo/{wk}/a/src/x.rs")),
                    PathBuf::from("/repo/src/x.rs")
                ]
            );
            assert_eq!(
                layout.worktree_of(Path::new(&nested)),
                Some(PathBuf::from(format!("/repo/{wk}/a")))
            );
            let upper = format!("/repo/{}/a/src/x.rs", wk.to_uppercase());
            assert_eq!(
                layout.main_checkout_copies(Path::new(&upper)),
                [PathBuf::from("/repo/src/x.rs")]
            );
            assert_eq!(
                layout.worktree_of(Path::new(&upper)),
                Some(PathBuf::from(format!("/repo/{}/a", wk.to_uppercase())))
            );
            assert!(layout
                .main_checkout_copies(Path::new(&format!("/repo/{wk}")))
                .is_empty());
            assert_eq!(layout.worktree_of(Path::new("/repo/src/x.rs")), None);
            assert_eq!(
                layout.main_checkout_copies(Path::new(&format!("/repo/{wk}/wt"))),
                [PathBuf::from("/repo")]
            );
            let own = layout.project_dir_name();
            for not_one in [
                format!("/repo/{own}/sessions/s/x"),
                format!("/repo/{own}/worktrees/a/x"),
                "/repo/src/a".to_string(),
                "/repo/wkts/a/x".to_string(),
            ] {
                assert!(
                    layout.main_checkout_copies(Path::new(&not_one)).is_empty(),
                    "{not_one}"
                );
                assert_eq!(layout.worktree_of(Path::new(&not_one)), None, "{not_one}");
            }
        }
        let trees = Layout::new(".codix")
            .unwrap()
            .with_worktrees_dir(".trees")
            .unwrap();
        assert_eq!(
            trees.main_checkout_copies(Path::new("/repo/.wkts/a/.trees/b/x")),
            [PathBuf::from("/repo/.wkts/a/x"), PathBuf::from("/repo/x")],
            "a worktree made under the default directory is still an alias of the main checkout"
        );
        assert_eq!(
            trees.worktree_of(Path::new("/repo/.wkts/a/x")),
            Some(PathBuf::from("/repo/.wkts/a"))
        );
        assert!(Layout::default()
            .main_checkout_copies(Path::new("/repo/.trees/a/x"))
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
