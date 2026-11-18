//! The files one plugin command's run may write (`[[command]] writes`): its
//! globs, narrowed by the rules a plugin skill's `allowed-tools` path grants
//! are held to (`ARCHITECTURE.md` §6.1), and whether a path a sub-call writes
//! falls inside them.

use crate::path_rule::{matches, resolve_pattern, ResolvedPath};
use base::permission::{PermissionRule, RuleBehavior, RuleSource};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteScope {
    /// Each kept glob resolved onto the project root as written.
    lexical: Vec<String>,
    /// The same, onto the root with its symbolic links followed.
    physical: Vec<String>,
}

impl WriteScope {
    /// `globs` relative to `root`. A glob that is absolute, starts with `~`,
    /// or reaches beyond the project — matched at any depth, the whole
    /// project, `../` out of it, or out through a symbolic link — is left
    /// out and returned second.
    pub fn new(globs: &[String], root: &Path) -> (Self, Vec<String>) {
        let physical_root = base::path::resolve_physically(root);
        let mut scope = Self::default();
        let mut dropped = Vec::new();
        for glob in globs {
            let glob = glob.trim();
            let anchored = Path::new(glob).is_absolute() || glob.starts_with('~');
            if anchored || crate::dangerous::is_unscoped_grant_in("Edit", Some(glob), root) {
                dropped.push(glob.to_string());
                continue;
            }
            scope.lexical.push(resolve_pattern(glob, root));
            scope.physical.push(resolve_pattern(glob, &physical_root));
        }
        (scope, dropped)
    }

    pub fn is_empty(&self) -> bool {
        self.lexical.is_empty()
    }

    /// Whether writing `raw`, as a tool resolves it from `cwd`, stays inside:
    /// the path it opens and every other spelling of it, symbolic links
    /// followed.
    pub fn covers(&self, raw: &str, cwd: &Path) -> bool {
        let path = ResolvedPath::new(raw, cwd);
        let inside = |p: &Path, patterns: &[String]| patterns.iter().any(|g| matches(g, p));
        inside(&path.primary, &self.lexical)
            && path
                .aliases
                .iter()
                .all(|alias| inside(alias, &self.lexical) || inside(alias, &self.physical))
    }

    /// Whether writing `raw` from `cwd` reaches configuration under any of
    /// its spellings ([`base::paths::SensitivePaths::names_configuration`]).
    /// No scope covers it: a command is installed code nobody watches, and
    /// a git hook it writes runs with the next commit.
    pub fn reaches_configuration(
        raw: &str,
        cwd: &Path,
        paths: &base::paths::SensitivePaths,
    ) -> bool {
        ResolvedPath::new(raw, cwd)
            .all()
            .any(|p| paths.names_configuration(&p.to_string_lossy()))
    }

    /// An `Edit(...)` allow per glob: what a delegated grant takes.
    pub fn grants(&self) -> Vec<PermissionRule> {
        self.lexical
            .iter()
            .map(|pattern| PermissionRule {
                source: RuleSource::Command,
                behavior: RuleBehavior::Allow,
                tool_name: "Edit".into(),
                rule_content: Some(pattern.clone()),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn globs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_globs_below_the_project_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (scope, dropped) = WriteScope::new(
            &globs(&["forge/**", "**", "*.md", "../x/**", "/etc/**", "~/x", "."]),
            root,
        );
        assert_eq!(scope.lexical.len(), 1, "{scope:?}");
        assert_eq!(
            dropped,
            globs(&["**", "*.md", "../x/**", "/etc/**", "~/x", "."])
        );
    }

    #[test]
    fn a_path_is_covered_when_every_spelling_of_it_is_inside() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("forge")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let (scope, _) = WriteScope::new(&globs(&["forge/**"]), root);
        assert!(scope.covers("forge/a.yaml", root));
        assert!(scope.covers(&root.join("forge/x/b.md").to_string_lossy(), root));
        assert!(!scope.covers("src/main.rs", root));
        assert!(!scope.covers("forge/../src/main.rs", root));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("src"), root.join("forge/link")).unwrap();
            assert!(
                !scope.covers("forge/link/main.rs", root),
                "a link out of the scope is not inside it"
            );
        }
    }

    #[test]
    fn configuration_is_reached_by_any_spelling_of_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = base::paths::SensitivePaths::new(None, Some(root), Default::default());
        let reaches = |raw: &str| WriteScope::reaches_configuration(raw, root, &paths);
        assert!(reaches(".git/hooks/pre-commit"));
        assert!(reaches("forge/../.git/config"));
        assert!(reaches(".agix/agents/a.md"));
        assert!(!reaches("forge/a.yaml"));
        assert!(!reaches(".github/workflows/ci.yml"));
        #[cfg(unix)]
        {
            std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
            std::fs::create_dir_all(root.join("forge")).unwrap();
            std::os::unix::fs::symlink(root.join(".git/hooks"), root.join("forge/h")).unwrap();
            assert!(reaches("forge/h/pre-commit"), "a link into .git reaches it");
        }
    }

    #[test]
    fn no_globs_cover_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (scope, _) = WriteScope::new(&[], dir.path());
        assert!(scope.is_empty());
        assert!(!scope.covers("a.txt", dir.path()));
        assert!(scope.grants().is_empty());
    }
}
