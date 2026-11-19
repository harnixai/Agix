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

    /// The project at `root` made ready to hold history: its project
    /// directory, `state/` and `state/history` each a directory of its own
    /// inside the project, the first with its `.gitignore`.
    fn prepare(&self, root: &Path) -> bool {
        let Ok(real_root) = root.canonicalize() else {
            return false;
        };
        let dir = self.layout.project_dir(root);
        let state = self.layout.project_state_dir(root);
        for level in [&dir, &state, &state.join(entry::HISTORY)] {
            if let Err(why) = own_dir_inside(level, &real_root) {
                tracing::warn!(dir = %level.display(), "{why}; this session's history is kept globally");
                return false;
            }
        }
        base::paths::ensure_local_data_gitignore(&dir);
        true
    }
}

fn own_dir_inside(dir: &Path, real_root: &Path) -> Result<(), String> {
    if dir.symlink_metadata().is_err() {
        std::fs::create_dir(dir).map_err(|e| format!("could not be made: {e}"))?;
    }
    let own = dir.symlink_metadata().is_ok_and(|m| m.is_dir());
    let inside = dir.canonicalize().is_ok_and(|d| d.starts_with(real_root));
    if own && inside {
        Ok(())
    } else {
        Err("not a directory of its own inside the project".into())
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

    #[test]
    fn a_state_or_history_directory_that_is_not_its_own_keeps_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let global = base.join("global");
        std::fs::create_dir_all(&global).unwrap();

        let linked = base.join("linked");
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(linked.join(".codix")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, linked.join(".codix/state")).unwrap();

        let filed = base.join("filed");
        std::fs::create_dir_all(filed.join(".codix/state")).unwrap();
        std::fs::write(filed.join(".codix/state/history"), "").unwrap();

        let history_linked = base.join("history-linked");
        std::fs::create_dir_all(history_linked.join(".codix/state")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, history_linked.join(".codix/state/history"))
            .unwrap();

        let p = policy(
            HistoryLocation::Project,
            &global,
            &[&linked, &filed, &history_linked],
        );
        for root in [&linked, &filed, &history_linked] {
            assert!(!p.keeps_new(root), "{}", root.display());
        }
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
    }
}
