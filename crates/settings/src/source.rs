//! The files-on-disk [`ConfigSource`]: three directories, each with its
//! gitignored overlay.

use std::path::{Path, PathBuf};

use base::interface::config_source::{ConfigLayer, ConfigSource, Tier};
use base::paths::{SETTINGS_FILE, SETTINGS_LOCAL_FILE};

/// The three directories on disk, each with its overlay.
pub struct FileTiers {
    global_dir: PathBuf,
    scene_dir: PathBuf,
    local_dir: PathBuf,
}

impl FileTiers {
    pub fn new(global_dir: PathBuf, scene_dir: PathBuf, local_dir: PathBuf) -> Self {
        Self {
            global_dir,
            scene_dir,
            local_dir,
        }
    }
}

impl ConfigSource for FileTiers {
    fn layers(&self) -> Vec<ConfigLayer> {
        let tiers = [
            (Tier::Global, &self.global_dir),
            (Tier::Scene, &self.scene_dir),
            (Tier::Project, &self.local_dir),
        ];
        let mut layers = Vec::new();
        for (i, (tier, dir)) in tiers.iter().enumerate() {
            // Two tiers can be one directory — a project rooted at `$HOME`
            // shares `~/.agix` with the global tier. Read it once, at the
            // higher tier: a merge applying it twice changes nothing, but
            // `permission_rules` are concatenated and would be doubled.
            if tiers[i + 1..].iter().any(|(_, later)| later == dir) {
                continue;
            }
            let tier = *tier;
            for (file, machine_local) in [(SETTINGS_FILE, false), (SETTINGS_LOCAL_FILE, true)] {
                let path = dir.join(file);
                if let Some(value) = read_settings_file(tier, &path) {
                    layers.push(ConfigLayer {
                        tier,
                        machine_local,
                        origin: path.display().to_string(),
                        value,
                    });
                }
            }
        }
        layers
    }
}

/// Read and parse one settings file. `None` when it doesn't exist, is a
/// symlink (a link in a project is the repository's choice of what to read),
/// can't be read, or doesn't parse — each of the latter three warns rather
/// than aborting, so one broken file can't stop the process from starting.
pub fn read_settings_file(tier: Tier, path: &Path) -> Option<serde_json::Value> {
    match std::fs::symlink_metadata(path) {
        Err(_) => return None,
        Ok(meta) if meta.file_type().is_symlink() => {
            tracing::warn!(layer = tier.name(), path = %path.display(), "settings file is a symlink; not read");
            return None;
        }
        Ok(_) => {}
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(layer = tier.name(), path = %path.display(), error = %e, "failed to read settings file, skipping this layer");
            return None;
        }
    };
    match serde_json::from_str(&content) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(layer = tier.name(), path = %path.display(), error = %e, "failed to parse settings file, skipping this layer");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tier_yields_its_shared_file_below_its_overlay() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SETTINGS_FILE), r#"{"a":1}"#).unwrap();
        std::fs::write(dir.path().join(SETTINGS_LOCAL_FILE), r#"{"a":2}"#).unwrap();
        let source = FileTiers::new(
            dir.path().to_path_buf(),
            dir.path().join("absent"),
            dir.path().join("absent"),
        );

        let layers = source.layers();
        assert_eq!(layers.len(), 2);
        assert!(!layers[0].machine_local);
        assert!(layers[1].machine_local);
        assert_eq!(layers[1].value["a"], 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_settings_file_that_is_a_symlink_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere.json");
        std::fs::write(&elsewhere, r#"{"a":1}"#).unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::os::unix::fs::symlink(&elsewhere, project.join(SETTINGS_FILE)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, project.join(SETTINGS_LOCAL_FILE)).unwrap();
        let source = FileTiers::new(
            dir.path().join("absent"),
            dir.path().join("absent"),
            project,
        );
        assert!(source.layers().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_users_settings_file_may_link_within_the_state_directory_but_not_out_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        std::fs::create_dir_all(global.join("dotfiles")).unwrap();
        std::fs::write(global.join("dotfiles/settings.json"), r#"{"a":1}"#).unwrap();
        std::fs::write(outside.path().join("local.json"), r#"{"b":1}"#).unwrap();
        std::os::unix::fs::symlink(
            global.join("dotfiles/settings.json"),
            global.join(SETTINGS_FILE),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("local.json"),
            global.join(SETTINGS_LOCAL_FILE),
        )
        .unwrap();
        let source = FileTiers::new(global, dir.path().join("absent"), dir.path().join("absent"));
        let layers = source.layers();
        assert_eq!(
            layers.len(),
            1,
            "{:?}",
            layers.iter().map(|l| &l.origin).collect::<Vec<_>>()
        );
        assert_eq!(layers[0].value["a"], 1);
    }

    #[test]
    fn a_file_that_will_not_parse_is_skipped_rather_than_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SETTINGS_FILE), "{not json").unwrap();
        let source = FileTiers::new(
            dir.path().to_path_buf(),
            dir.path().join("absent"),
            dir.path().join("absent"),
        );
        assert!(source.layers().is_empty());
    }
}
