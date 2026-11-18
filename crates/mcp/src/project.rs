//! The project a connection serves, as its server learns it: the `roots` the
//! client lists (the project root as one `file://` URI, none without a
//! project) and, for a stdio server, the `AGIX_PROJECT_ROOT` variable. A pool
//! connects servers per (project, scene), so every connection serves exactly
//! one project, or none.

use std::path::{Path, PathBuf};

use base::interface::exec::SubprocessEnv;

pub const PROJECT_ROOT_ENV: &str = "AGIX_PROJECT_ROOT";

/// How a manager starts its servers: under the `sandbox.subprocess_env` of
/// the settings they were declared in, for the project their connections
/// serve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpLaunch {
    pub subprocess_env: SubprocessEnv,
    project_root: Option<PathBuf>,
}

impl McpLaunch {
    /// The project is resolved first, so one directory reached through a
    /// symlink or another spelling is told to its servers as one path.
    pub fn new(subprocess_env: SubprocessEnv, project_root: Option<&Path>) -> Self {
        let project_root =
            project_root.map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));
        Self {
            subprocess_env,
            project_root,
        }
    }

    pub fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    /// Set after the server's own `env`: what the variable says is the
    /// project, whatever a configuration or the daemon's environment holds.
    pub(crate) fn tell_project(&self, cmd: &mut tokio::process::Command) {
        match &self.project_root {
            Some(root) => cmd.env(PROJECT_ROOT_ENV, root),
            None => cmd.env_remove(PROJECT_ROOT_ENV),
        };
    }
}

/// `roots/list` as this client answers it for a connection serving `project`.
#[expect(
    deprecated,
    reason = "roots are deprecated by SEP-2577 yet remain how a server learns the client's directory"
)]
pub(crate) fn roots_of(project: Option<&Path>) -> rmcp::model::ListRootsResult {
    let roots = project
        .and_then(|root| {
            let uri = url::Url::from_file_path(root).ok()?;
            let root_entry = rmcp::model::Root::new(uri.to_string());
            Some(match root.file_name() {
                Some(name) => root_entry.with_name(name.to_string_lossy()),
                None => root_entry,
            })
        })
        .into_iter()
        .collect();
    rmcp::model::ListRootsResult::new(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_is_listed_as_one_file_uri_and_no_project_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let listed = serde_json::to_value(roots_of(Some(&root))).unwrap();
        let uri = url::Url::from_file_path(&root).unwrap().to_string();
        assert_eq!(listed["roots"][0]["uri"], uri, "{listed}");
        assert_eq!(listed["roots"].as_array().map(Vec::len), Some(1));
        let none = serde_json::to_value(roots_of(None)).unwrap();
        assert_eq!(none["roots"], serde_json::json!([]));
    }

    #[cfg(unix)]
    #[test]
    fn a_project_reached_through_a_symlink_is_told_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let launch = McpLaunch::new(SubprocessEnv::default(), Some(&link));
        assert_eq!(
            launch.project_root(),
            Some(std::fs::canonicalize(&real).unwrap().as_path())
        );
    }
}
