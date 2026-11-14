//! The one way the assembly reads a (project, scene) pair's settings from disk.

use std::path::{Path, PathBuf};

use settings::LoadedSettings;

/// The three tiers merged, then `--max-tokens` where no tier set it. The only
/// place in the assembly that calls `LoadedSettings::load*`.
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
    if loaded.values.model.max_tokens == settings::DEFAULT_MAX_TOKENS {
        loaded.values.model.max_tokens = max_tokens_fallback;
    }
    loaded
}
