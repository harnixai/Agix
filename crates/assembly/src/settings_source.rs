//! The one way the assembly reads a (project, scene) pair's settings from disk,
//! and the same read with some of its files replaced by what a write would put
//! there.

use std::path::{Path, PathBuf};

use settings::LoadedSettings;

/// The three tiers merged, then `--max-tokens` where no tier set it. The only
/// place in the assembly, with [`read_pair_as_if`], that calls `LoadedSettings::load*`. It also hands
/// `network.use_system_proxy` to the process-wide HTTP clients; a later read
/// that disagrees is the `config.reload` of a setting that needs a restart.
pub(crate) fn read_pair(
    global_root: PathBuf,
    scene_root: PathBuf,
    project: Option<&Path>,
    scene: &str,
    default_model: &str,
    max_tokens_fallback: u32,
) -> LoadedSettings {
    let sections = crate::settings_sections::all();
    let mut loaded = match project {
        Some(root) => LoadedSettings::load(
            global_root,
            scene_root,
            root.join(".agix"),
            scene,
            default_model,
            &sections,
        ),
        None => LoadedSettings::load_without_project(
            global_root,
            scene_root,
            scene,
            default_model,
            &sections,
        ),
    };
    fall_back_max_tokens(&mut loaded, max_tokens_fallback);
    platform::configure_system_proxy(loaded.values.network.use_system_proxy);
    loaded
}

/// [`read_pair`] of a project with each `(path, value)` of `in_memory` read in
/// place of that settings file. It writes nothing and leaves the process-wide
/// proxy choice alone.
pub(crate) fn read_pair_as_if(
    in_memory: Vec<(PathBuf, serde_json::Value)>,
    global_root: PathBuf,
    scene_root: PathBuf,
    project: &Path,
    scene: &str,
    default_model: &str,
    max_tokens_fallback: u32,
) -> LoadedSettings {
    let local_dir = project.join(".agix");
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
    );
    fall_back_max_tokens(&mut loaded, max_tokens_fallback);
    loaded
}

fn fall_back_max_tokens(loaded: &mut LoadedSettings, max_tokens_fallback: u32) {
    if loaded.values.model.max_tokens == settings::DEFAULT_MAX_TOKENS {
        loaded.values.model.max_tokens = max_tokens_fallback;
    }
}
