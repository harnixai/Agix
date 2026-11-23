//! Workspace trust: whether a project's own configuration counts as the
//! user's, or is held to what an installed plugin package may do.
//!
//! The project tier — `<root>/.agix/settings*.json`, `.agix/agents`,
//! `.agix/hooks`, `.agents/skills` — is repository content: on a cloned
//! repository, whatever its author wrote. Trust is a property of the project
//! root and lives in the global tier (`app.trusted_projects`,
//! `app.trust_all_projects`), so nothing inside the project can grant it. This
//! file decides who is trusted and carries what the loader found in the
//! project tier; what an untrusted tier may set key by key is
//! [`crate::tier_gate`]. Agent types, skill commands and the hooks search path
//! read [`ProjectTrust`] off `LoadedSettings::trust`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use base::permission::ProjectTrust;

/// What a project no `app.trusted_projects` pattern names is.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    Trusted,
    #[default]
    Untrusted,
}

/// What decided that a project is trusted or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TrustSource {
    /// `app.trust_all_projects`: nobody has said anything about this project.
    Default,
    /// An `app.trusted_projects` pattern in the global `settings.json`.
    User,
    /// An `app.trusted_projects` pattern in the global `settings.local.json`,
    /// where `project.trust` records the host's answer.
    Host,
}

/// What the global tier's `app.trusted_projects` and `app.trust_all_projects`
/// say. A scene or project tier that sets them is ignored with a warning.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectTrustSettings {
    /// A project that no `trusted` pattern matches.
    pub default: TrustLevel,
    /// `app.trusted_projects` of every global file, added up.
    pub trusted: Vec<String>,
    /// The `trusted` patterns that only a `settings.local.json` holds.
    pub recorded_by_host: Vec<String>,
}

impl ProjectTrustSettings {
    /// The `app` blocks of `layers`, lowest first, each with whether it is a
    /// `settings.local.json`: the last `trust_all_projects` wins, the
    /// `trusted_projects` lists are concatenated.
    pub fn from_layers<'a>(layers: impl IntoIterator<Item = (&'a Value, bool)>) -> Self {
        let mut out = Self::default();
        for (section, machine_local) in layers
            .into_iter()
            .filter_map(|(v, local)| Some((v.get("app")?, local)))
        {
            match section.get("trust_all_projects") {
                None => {}
                Some(Value::Bool(true)) => out.default = TrustLevel::Trusted,
                Some(Value::Bool(false)) => out.default = TrustLevel::Untrusted,
                Some(other) => {
                    tracing::warn!(value = %other, "app.trust_all_projects is not true or false, ignoring it")
                }
            }
            for pattern in section
                .get("trusted_projects")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !out.trusted.iter().any(|p| p == pattern) {
                    out.trusted.push(pattern.to_string());
                    if machine_local {
                        out.recorded_by_host.push(pattern.to_string());
                    }
                }
            }
        }
        out
    }

    /// These settings with what the managed layer's `app` block says in their
    /// place: its `trust_all_projects`, and its `trusted_projects` as the
    /// whole list.
    pub fn decided_by(mut self, app: &Value) -> Self {
        match app.get("trust_all_projects") {
            Some(Value::Bool(true)) => self.default = TrustLevel::Trusted,
            Some(Value::Bool(false)) => self.default = TrustLevel::Untrusted,
            _ => {}
        }
        if let Some(list) = app.get("trusted_projects").and_then(Value::as_array) {
            self.trusted = list
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect();
            self.recorded_by_host.clear();
        }
        self
    }

    /// What the global tier said, over `app`: the `trusted_projects` of both
    /// of its files, not only the higher one's as a merge would leave them.
    pub fn write_into(self, app: &mut crate::AppSettings) {
        app.trust_all_projects = self.default == TrustLevel::Trusted;
        app.trusted_projects = self.trusted;
    }

    /// What the global tier in `global_dir` says, read straight from its
    /// `settings.json` and `settings.local.json` — for a caller that needs
    /// the answer without loading a whole `Settings`.
    pub fn read_global(global_dir: &Path) -> Self {
        let layers: Vec<(Value, bool)> = [
            (base::paths::SETTINGS_FILE, false),
            (base::paths::SETTINGS_LOCAL_FILE, true),
        ]
        .iter()
        .filter_map(|(file, local)| {
            let text = std::fs::read_to_string(global_dir.join(file)).ok()?;
            Some((serde_json::from_str(&text).ok()?, *local))
        })
        .collect();
        Self::from_layers(layers.iter().map(|(v, local)| (v, *local)))
    }

    /// Whether `root` is trusted, the pattern that said so, and where that
    /// pattern was written.
    pub fn decide(&self, root: &Path) -> TrustDecision {
        self.decide_with_default(root, self.default)
    }

    /// The project tier at `project` (a `.agix` directory) as the loader
    /// starts it: who trusts it, before any of its keys are gated.
    pub fn decide_tier(
        &self,
        own_project: bool,
        project: &Path,
        global: &Path,
        scene: &Path,
    ) -> ProjectLayer {
        let root = project.parent().unwrap_or(project);
        let decided = if own_project {
            self.decide(root)
        } else if borrows_a_tier(project, global, scene) {
            self.decide_with_default(root, TrustLevel::Untrusted)
        } else {
            return ProjectLayer::default();
        };
        ProjectLayer {
            trust: decided.trust,
            matched: decided.matched,
            source: Some(decided.source),
            ..Default::default()
        }
    }

    /// [`decide`](Self::decide) with `default` for a root no pattern names.
    pub fn decide_with_default(&self, root: &Path, default: TrustLevel) -> TrustDecision {
        if let Some(pattern) = self.trusted.iter().find(|p| pattern_matches(p, root)) {
            let source = if self.recorded_by_host.contains(pattern) {
                TrustSource::Host
            } else {
                TrustSource::User
            };
            return TrustDecision {
                trust: ProjectTrust::Trusted,
                matched: Some(pattern.clone()),
                source,
            };
        }
        let trust = match default {
            TrustLevel::Trusted => ProjectTrust::Trusted,
            TrustLevel::Untrusted => ProjectTrust::Untrusted,
        };
        TrustDecision {
            trust,
            matched: None,
            source: TrustSource::Default,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustDecision {
    pub trust: ProjectTrust,
    pub matched: Option<String>,
    pub source: TrustSource,
}

/// A project-tier setting that takes effect only in a trusted project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withheld {
    /// Dotted (`hooks`, `mcp.servers.github`,
    /// `permissions.allow(Bash)`).
    pub key: String,
    /// The file it is in.
    pub origin: String,
}

/// What `LoadedSettings::load_from` found in the project tier.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectLayer {
    pub trust: ProjectTrust,
    /// The `app.trusted_projects` pattern that matched the root; `None`
    /// when the default decided.
    pub matched: Option<String>,
    /// `None` when there is no project tier.
    pub source: Option<TrustSource>,
    /// The project-tier settings that take effect only while the project is
    /// trusted. Withheld while it is not.
    pub requires_trust: Vec<Withheld>,
    /// The project tier's own `mcp.servers` entries, both of its files
    /// merged — whether or not they were withheld.
    pub mcp_servers: BTreeMap<String, Value>,
}

impl ProjectLayer {
    /// `requires_trust` with each key of each file once.
    pub(crate) fn dedup_requires_trust(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.requires_trust
            .retain(|w| seen.insert((w.key.clone(), w.origin.clone())));
    }
}

/// Whether `project` (a `.agix` directory) is a project tier of its own,
/// rather than the global or scene directory a project rooted at `$HOME` (or
/// no project) resolves to.
pub fn is_own_project_dir(project: &Path, global: &Path, scene: &Path) -> bool {
    !same_dir(project, global) && !same_dir(project, scene)
}

/// Why a write to the project tier at `project` (a `.agix` directory) would
/// not land in that project's own tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotItsOwnTier {
    /// It is the global or scene directory: a write would reach every project.
    SharedTier,
    /// It resolves outside the project, to this directory (`None` where it
    /// resolves nowhere): a write would change that directory's settings.
    Elsewhere(Option<PathBuf>),
}

/// The one check every write to a project's own tier makes — the settings
/// methods and "always allow, this project" alike.
pub fn project_tier_refusal(project: &Path, global: &Path, scene: &Path) -> Option<NotItsOwnTier> {
    if !is_own_project_dir(project, global, scene) {
        return Some(NotItsOwnTier::SharedTier);
    }
    (!stays_in_its_project(project)).then(|| NotItsOwnTier::Elsewhere(project.canonicalize().ok()))
}

/// Whether `project` (a `.agix` directory) is its project's own: absent, or
/// resolving to a directory directly in the project as that resolves — not a
/// link into another project's, whose settings a write would change.
fn stays_in_its_project(project: &Path) -> bool {
    let Some(root) = project.parent() else {
        return false;
    };
    if project.symlink_metadata().is_err() {
        return true;
    }
    match (project.canonicalize(), root.canonicalize()) {
        (Ok(dir), Ok(root)) => dir.parent() == Some(root.as_path()),
        _ => false,
    }
}

/// Whether `project` (a `.agix` directory) resolves to the global or scene
/// directory although its project is not the directory that holds that one —
/// a `.agix` linked there from a repository. Such a project has no tier of
/// its own, but it is still somebody's repository.
pub fn borrows_a_tier(project: &Path, global: &Path, scene: &Path) -> bool {
    let root = project.parent().unwrap_or(project);
    [global, scene].into_iter().any(|tier| {
        project != tier
            && same_dir(project, tier)
            && !tier.parent().is_some_and(|holder| same_dir(root, holder))
    })
}

pub fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (a.canonicalize(), b.canonicalize()),
            (Ok(a), Ok(b)) if a == b
        )
}

/// Whether an `app.trusted_projects` pattern names `root`. Compared against
/// the root as the file system resolves it, and as given when that has no
/// `.` or `..` in it — so a pattern written either way matches a root reached
/// through a symlink, and `/home/u/work/**` does not match
/// `/home/u/work/../../tmp/x`.
pub fn pattern_matches(pattern: &str, root: &Path) -> bool {
    use std::path::Component;
    let pattern = expand_home(pattern);
    let as_given = root.is_absolute()
        && root.components().all(|c| {
            matches!(
                c,
                Component::RootDir | Component::Prefix(_) | Component::Normal(_)
            )
        });
    let roots: Vec<PathBuf> = root
        .canonicalize()
        .ok()
        .into_iter()
        .chain(as_given.then(|| root.to_path_buf()))
        .collect();
    if pattern.contains(['*', '?', '[']) {
        let Ok(glob) = globset::GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()
        else {
            tracing::warn!(%pattern, "app.trusted_projects pattern does not parse, ignoring it");
            return false;
        };
        let glob = glob.compile_matcher();
        return roots.iter().any(|r| glob.is_match(r));
    }
    let path = Path::new(&pattern);
    roots
        .iter()
        .any(|r| r == path || path.canonicalize().is_ok_and(|p| &p == r))
}

/// The `app.trusted_projects` entry naming `root` and nothing else: `root` with its glob
/// metacharacters escaped, so `Project [v2]` is not read as a pattern that
/// matches `Project v` and not itself.
pub fn exact_pattern(root: &Path) -> String {
    let text = root.display().to_string();
    if !text.contains(['*', '?', '[']) {
        return text;
    }
    text.chars()
        .map(|c| match c {
            '\\' => "[\\\\]".to_string(),
            '*' | '?' | '[' | ']' | '{' | '}' => format!("[{c}]"),
            c => c.to_string(),
        })
        .collect()
}

fn expand_home(pattern: &str) -> String {
    match (pattern.strip_prefix("~/"), base::paths::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest).display().to_string(),
        _ => pattern.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn patterns_name_roots_exactly_or_by_glob() {
        let root = Path::new("/srv/ipd/lines/a");
        assert!(pattern_matches("/srv/ipd/lines/a", root));
        assert!(pattern_matches("/srv/ipd/lines/a/", root));
        assert!(pattern_matches("/srv/ipd/lines/*", root));
        assert!(pattern_matches("/srv/**", root));
        assert!(!pattern_matches("/srv/*", root));
        assert!(!pattern_matches("/srv/ipd/lines", root));
    }

    #[test]
    fn an_exact_entry_names_its_root_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        for name in ["Project [v2]", "a*b?c", "x{1,2}]", "back\\slash [", "plain"] {
            let root = base.join(name);
            std::fs::create_dir_all(&root).unwrap();
            let entry = exact_pattern(&root);
            assert!(pattern_matches(&entry, &root), "{name}: {entry}");
            for other in ["Project v", "Project 2", "aXbYc", "x1]", "plai"] {
                let other = base.join(other);
                std::fs::create_dir_all(&other).unwrap();
                assert!(
                    !pattern_matches(&entry, &other),
                    "{entry} matched {other:?}"
                );
            }
        }
    }

    #[test]
    fn a_root_spelled_with_dot_dot_is_matched_where_it_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (work, evil) = (base.join("work"), base.join("evil"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&evil).unwrap();
        let through_work = work.join("..").join("evil");
        let glob = format!("{}/**", work.display());
        assert!(!pattern_matches(&glob, &through_work));
        assert!(pattern_matches(&glob, &work.join("sub")));
        assert!(pattern_matches(&evil.display().to_string(), &through_work));
    }

    #[test]
    fn the_trusted_lists_add_up_and_the_last_default_wins() {
        let file = json!({"app": {"trust_all_projects": true, "trusted_projects": ["/a"]}});
        let local = json!({"app": {"trust_all_projects": false, "trusted_projects": ["/b", "/a"]}});
        let settings = ProjectTrustSettings::from_layers([(&file, false), (&local, true)]);
        assert_eq!(settings.default, TrustLevel::Untrusted);
        assert_eq!(settings.trusted, ["/a", "/b"]);
        let decided = |root: &str| {
            let d = settings.decide(Path::new(root));
            (d.trust, d.matched, d.source)
        };
        assert_eq!(
            decided("/a"),
            (ProjectTrust::Trusted, Some("/a".into()), TrustSource::User)
        );
        assert_eq!(
            decided("/b"),
            (ProjectTrust::Trusted, Some("/b".into()), TrustSource::Host)
        );
        assert_eq!(
            decided("/c"),
            (ProjectTrust::Untrusted, None, TrustSource::Default)
        );
    }
}
