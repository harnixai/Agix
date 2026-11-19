//! The engine's side of keeping history in projects: a project's history
//! directory (a worktree's is its main checkout's), whether a new session goes
//! there (`history.location` and the project's trust), and whether a project
//! is trusted now — read from the global tier's `project_trust` every time, so
//! that taking trust back stops reads at once.

use std::path::{Path, PathBuf};

use base::paths::{entry, Layout};
use history::placement::{ProjectHistory, ProjectHistoryDir};
use settings::HistoryLocation;

pub(crate) struct ProjectHistoryPolicy {
    location: HistoryLocation,
    layout: Layout,
    global_dir: PathBuf,
}

impl ProjectHistoryPolicy {
    pub(crate) fn new(location: HistoryLocation, layout: Layout, global_dir: PathBuf) -> Self {
        Self {
            location,
            layout,
            global_dir,
        }
    }

    /// The project directory at `root` made ready to hold history: a
    /// directory of its own inside the project, with its `.gitignore`.
    fn prepare(&self, root: &Path) -> bool {
        let dir = self.layout.project_dir(root);
        if dir.symlink_metadata().is_err() {
            if let Err(e) = std::fs::create_dir(&dir) {
                tracing::warn!(dir = %dir.display(), error = %e, "could not make the project directory; this session's history is kept globally");
                return false;
            }
        }
        let own_dir = dir.symlink_metadata().is_ok_and(|m| m.is_dir());
        let inside = match (dir.canonicalize(), root.canonicalize()) {
            (Ok(dir), Ok(root)) => dir.starts_with(root),
            _ => false,
        };
        if !(own_dir && inside) {
            tracing::warn!(dir = %dir.display(), "the project directory is not a directory of its own inside the project; this session's history is kept globally");
            return false;
        }
        base::paths::ensure_local_data_gitignore(&dir);
        true
    }
}

impl ProjectHistory for ProjectHistoryPolicy {
    fn history_of(&self, root: &Path) -> Option<ProjectHistoryDir> {
        let main = self
            .layout
            .main_checkout_copies(root)
            .pop()
            .unwrap_or_else(|| root.to_path_buf());
        Some(ProjectHistoryDir {
            dir: self.layout.project_state_dir(&main).join(entry::HISTORY),
            root: main,
        })
    }

    fn keeps_new(&self, root: &Path) -> bool {
        if self.location != HistoryLocation::Project {
            return false;
        }
        let Some(found) = self.history_of(root) else {
            return false;
        };
        self.trusted(root) && self.trusted(&found.root) && self.prepare(&found.root)
    }

    fn trusted(&self, root: &Path) -> bool {
        !settings::trust::ProjectTrustSettings::read_global(&self.global_dir)
            .decide(root)
            .trust
            .is_untrusted()
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn policy(location: HistoryLocation, global: &Path, trusted: &[&Path]) -> ProjectHistoryPolicy {
        let patterns: Vec<String> = trusted.iter().map(|p| p.display().to_string()).collect();
        std::fs::write(
            global.join(base::paths::SETTINGS_FILE),
            serde_json::json!({"project_trust": {"trusted": patterns}}).to_string(),
        )
        .unwrap();
        ProjectHistoryPolicy::new(
            location,
            Layout::new(".codix").unwrap(),
            global.to_path_buf(),
        )
    }

    #[test]
    fn a_worktrees_history_is_its_main_checkouts() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("repo");
        let p = policy(HistoryLocation::Project, tmp.path(), &[]);
        let found = p.history_of(&main.join(".wkts/w1")).unwrap();
        assert_eq!(found.root, main);
        assert_eq!(found.dir, main.join(".codix/state/history"));
    }

    #[test]
    fn only_a_trusted_project_under_the_project_setting_keeps_new_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let global = tmp.path().join("global");
        std::fs::create_dir_all(&global).unwrap();

        assert!(!policy(HistoryLocation::Project, &global, &[]).keeps_new(&root));
        assert!(!policy(HistoryLocation::Global, &global, &[&root]).keeps_new(&root));
        let keeps = policy(HistoryLocation::Project, &global, &[&root]);
        assert!(keeps.keeps_new(&root));
        let ignored = std::fs::read_to_string(root.join(".codix/.gitignore")).unwrap();
        assert!(ignored.lines().any(|l| l == "state/"));

        std::fs::write(global.join(base::paths::SETTINGS_FILE), "{}").unwrap();
        assert!(!keeps.trusted(&root));
    }

    #[test]
    fn a_project_directory_that_leads_elsewhere_keeps_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("repo");
        let elsewhere = tmp.path().canonicalize().unwrap().join("elsewhere");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(".codix")).unwrap();
        let global = tmp.path().join("global");
        std::fs::create_dir_all(&global).unwrap();
        assert!(!policy(HistoryLocation::Project, &global, &[&root]).keeps_new(&root));
    }
}
