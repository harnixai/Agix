//! The one way the assembly reads a (project, scene) pair's settings from disk,
//! and the same read with some of its files replaced by what a write would put
//! there. Reading a project's is opening it: what an older engine left
//! directly in its project directory is moved into `state/` first.

use std::path::{Path, PathBuf};

use base::paths::{Layout, PROJECT_STATE_ENTRIES};

use settings::LoadedSettings;

/// The three tiers merged, then `--max-tokens` where no tier set it; the
/// project tier is `layout`'s directory in `project`. The only
/// place in the assembly, with [`read_pair_as_if`], that calls `LoadedSettings::load*`. It also hands
/// `network.use_system_proxy` to the process-wide HTTP clients; a later read
/// that disagrees is the `config.reload` of a setting that needs a restart.
pub(crate) fn read_pair(
    global_root: PathBuf,
    scene_root: PathBuf,
    project: Option<&Path>,
    layout: &Layout,
    scene: &str,
    default_model: &str,
    max_tokens_fallback: u32,
) -> LoadedSettings {
    let sections = crate::settings_sections::all();
    if let Some(root) = project {
        move_into_state(root, layout);
    }
    let mut loaded = match project {
        Some(root) => LoadedSettings::load(
            global_root,
            scene_root,
            layout.project_dir(root),
            scene,
            default_model,
            &sections,
            layout.clone(),
        ),
        None => LoadedSettings::load_without_project(
            global_root,
            scene_root,
            scene,
            default_model,
            &sections,
            layout.clone(),
        ),
    };
    fall_back_max_tokens(&mut loaded, max_tokens_fallback);
    platform::configure_system_proxy(loaded.values.network.use_system_proxy);
    loaded
}

/// [`read_pair`] of a project with each `(path, value)` of `in_memory` read in
/// place of that settings file. It writes nothing and leaves the process-wide
/// proxy choice alone.
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_pair_as_if(
    in_memory: Vec<(PathBuf, serde_json::Value)>,
    global_root: PathBuf,
    scene_root: PathBuf,
    project: &Path,
    layout: &Layout,
    scene: &str,
    default_model: &str,
    max_tokens_fallback: u32,
) -> LoadedSettings {
    let local_dir = layout.project_dir(project);
    let source = in_memory.into_iter().fold(
        settings::FileTiers::new(global_root.clone(), scene_root.clone(), local_dir.clone()),
        |source, (path, value)| source.with_contents(path, value),
    );
    let mut loaded = LoadedSettings::load_from(
        &source,
        global_root,
        scene_root,
        local_dir,
        scene,
        default_model,
        &crate::settings_sections::all(),
        layout.clone(),
    );
    fall_back_max_tokens(&mut loaded, max_tokens_fallback);
    loaded
}

/// Each of [`PROJECT_STATE_ENTRIES`] lying directly in `project`'s project
/// directory moved into its `state/`, where everything reads it from: only
/// where the project directory is a directory of its own inside the project
/// and `state/` is one too, never an entry that is a symbolic link, and never
/// over one already in `state/`. What cannot be moved is left with a warning.
fn move_into_state(project: &Path, layout: &Layout) {
    let dir = layout.project_dir(project);
    let real_dir = |p: &Path| p.symlink_metadata().is_ok_and(|m| m.is_dir());
    let inside = match (dir.canonicalize(), project.canonicalize()) {
        (Ok(dir), Ok(project)) => dir.starts_with(project),
        _ => false,
    };
    if !real_dir(&dir) || !inside {
        return;
    }
    let lying: Vec<&str> = PROJECT_STATE_ENTRIES
        .iter()
        .copied()
        .filter(|name| {
            dir.join(name)
                .symlink_metadata()
                .is_ok_and(|m| !m.file_type().is_symlink())
        })
        .collect();
    if lying.is_empty() {
        return;
    }
    let state = layout.project_state_dir(project);
    if state.symlink_metadata().is_err() {
        if let Err(e) = std::fs::create_dir(&state) {
            tracing::warn!(dir = %state.display(), error = %e, "could not make the project's state directory; its local data stays where it is and is not read");
            return;
        }
    }
    if !real_dir(&state) {
        tracing::warn!(dir = %state.display(), "the project's state directory is not a directory of its own; nothing is moved into it");
        return;
    }
    for name in lying {
        let (from, to) = (dir.join(name), state.join(name));
        if to.symlink_metadata().is_ok() {
            tracing::info!(old = %from.display(), new = %to.display(), "both exist; only the one in state/ is read");
            continue;
        }
        if let Err(e) = std::fs::rename(&from, &to) {
            tracing::warn!(from = %from.display(), to = %to.display(), error = %e, "could not move into the project's state directory; the old copy is not read");
        }
    }
}

fn fall_back_max_tokens(loaded: &mut LoadedSettings, max_tokens_fallback: u32) {
    if loaded.values.model.max_tokens == settings::DEFAULT_MAX_TOKENS {
        loaded.values.model.max_tokens = max_tokens_fallback;
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(path: PathBuf) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn what_lies_directly_in_the_project_directory_moves_into_state() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().canonicalize().unwrap();
        let elsewhere = project.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let layout = Layout::new(".codix").unwrap();
        let own = layout.project_dir(&project);
        let state = layout.project_state_dir(&project);
        write(&own.join("memory/a.md"), "a");
        write(&own.join("scratchpad/n.txt"), "n");
        write(&own.join(".imported.json"), "{}");
        write(&own.join("telemetry/s.jsonl"), "old");
        write(&state.join("telemetry/s.jsonl"), "new");
        write(&own.join("settings.local.json"), "{}");
        symlink(&elsewhere, own.join("teams")).unwrap();

        move_into_state(&project, &layout);
        assert_eq!(read(state.join("memory/a.md")), "a");
        assert_eq!(read(state.join("scratchpad/n.txt")), "n");
        assert!(state.join(".imported.json").is_file());
        for gone in ["memory", "scratchpad", ".imported.json"] {
            assert!(own.join(gone).symlink_metadata().is_err(), "{gone}");
        }
        assert_eq!(read(state.join("telemetry/s.jsonl")), "new");
        assert_eq!(read(own.join("telemetry/s.jsonl")), "old");
        assert!(own.join("teams").symlink_metadata().unwrap().is_symlink());
        assert!(state.join("teams").symlink_metadata().is_err());
        assert!(own.join("settings.local.json").is_file());

        move_into_state(&project, &layout);
        assert_eq!(read(state.join("memory/a.md")), "a");
    }

    #[test]
    fn nothing_moves_through_a_symbolic_link() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let (project, elsewhere) = (base.join("project"), base.join("elsewhere"));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let layout = Layout::default();
        let own = layout.project_dir(&project);

        write(&own.join("memory/a.md"), "a");
        symlink(&elsewhere, own.join("state")).unwrap();
        move_into_state(&project, &layout);
        assert!(own.join("memory/a.md").is_file());
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());

        std::fs::remove_dir_all(&own).unwrap();
        write(&elsewhere.join("memory/b.md"), "b");
        symlink(&elsewhere, &own).unwrap();
        move_into_state(&project, &layout);
        assert!(elsewhere.join("memory/b.md").is_file());
        assert!(!elsewhere.join("state").exists());
    }
}
