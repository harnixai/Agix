//! The one way the assembly reads a (project, scene) pair's settings from disk,
//! and the same read with some of its files replaced by what a write would put
//! there; both with the `cli` layer above the files and the managed layer above
//! everything. A read writes nothing: what an older engine left directly in a
//! project's project directory is moved into `state/` by [`move_into_state`]
//! when the project is opened — the daemon's own at start, any other when a
//! session's engine is built in it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base::interface::config_source::{Chain, ConfigLayer, ConfigSource, InMemoryLayers, Tier};
use base::paths::{Layout, PROJECT_STATE_ENTRIES};
use base::permission::ManagedSettings;

use settings::{FileTiers, LoadRequest, LoadedSettings};

/// What a read puts above the settings files: the `cli` layer, then the
/// deployment's managed layer.
#[derive(Clone, Copy, Default)]
pub(crate) struct Above<'a> {
    pub(crate) cli: Option<&'a serde_json::Value>,
    pub(crate) managed: Option<&'a ManagedSettings>,
}

/// The three tiers merged, then `cli` over them, then the managed layer; the
/// project tier is `layout`'s directory in `project`. The only place in the
/// assembly, with [`read_pair_as_if`] and [`read_app`], that calls
/// `LoadedSettings::load*`.
pub(crate) struct PairRead<'a> {
    pub(crate) global_root: PathBuf,
    pub(crate) scene_root: PathBuf,
    pub(crate) project: Option<&'a Path>,
    pub(crate) layout: &'a Layout,
    pub(crate) scene: &'a str,
    pub(crate) default_model: &'a str,
    pub(crate) above: Above<'a>,
}

pub(crate) fn read_pair(request: PairRead<'_>) -> LoadedSettings {
    read(request, None)
}

/// [`read_pair`], and the layers the loader folded as the source handed
/// them over: what `config.explain` reads each key's story off.
pub(crate) fn trace_pair(request: PairRead<'_>) -> (LoadedSettings, Vec<ConfigLayer>) {
    let seen = Mutex::new(Vec::new());
    let loaded = read(request, Some(&seen));
    (loaded, seen.into_inner().unwrap_or_default())
}

/// A source that keeps a copy of the layers it hands over.
struct Recorded<'a> {
    source: Chain,
    seen: Option<&'a Mutex<Vec<ConfigLayer>>>,
}

impl ConfigSource for Recorded<'_> {
    fn layers(&self) -> Vec<ConfigLayer> {
        let layers = self.source.layers();
        if let Some(seen) = self.seen {
            if let Ok(mut seen) = seen.lock() {
                seen.clone_from(&layers);
            }
        }
        layers
    }
}

fn read(request: PairRead<'_>, seen: Option<&Mutex<Vec<ConfigLayer>>>) -> LoadedSettings {
    let PairRead {
        global_root,
        scene_root,
        project,
        layout,
        scene,
        default_model,
        above,
    } = request;
    let sections = crate::settings_sections::all();
    let local_dir = match project {
        Some(root) => layout.project_dir(root),
        None => global_root.clone(),
    };
    let files = FileTiers::new(global_root.clone(), scene_root.clone(), local_dir.clone());
    let source = Recorded {
        source: above.over(files),
        seen,
    };
    let loaded = match project {
        Some(_) => LoadedSettings::load_from(LoadRequest {
            source: &source,
            global_dir: global_root,
            scene_dir: scene_root,
            local_dir,
            scope: scene,
            default_model,
            sections: &sections,
            layout: layout.clone(),
        }),
        None => LoadedSettings::load_without_project_from(
            &source,
            global_root,
            scene_root,
            scene,
            default_model,
            &sections,
            layout.clone(),
        ),
    };
    loaded.with_managed(above.managed.cloned(), &sections)
}

/// The `app` block of the global tier at `global_root` with what is `above`
/// it, for what a process decides before it builds anything; configures
/// nothing.
pub(crate) fn read_app(global_root: &Path, above: Above<'_>) -> settings::AppSettings {
    let root = global_root.to_path_buf();
    let files = FileTiers::new(root.clone(), root.clone(), root.clone());
    LoadedSettings::load_without_project_from(
        &above.over(files),
        root.clone(),
        root,
        "",
        "",
        &crate::settings_sections::all(),
        Layout::default(),
    )
    .with_managed(above.managed.cloned(), &crate::settings_sections::all())
    .values
    .app
}

impl Above<'_> {
    /// `files` with these layers above them. A managed layer that cannot be
    /// honoured is left out of the read: it sets nothing, and the permission
    /// gate refuses every call under it.
    fn over(self, files: FileTiers) -> Chain {
        let mut above = InMemoryLayers::new();
        if let Some(layer) = self.cli {
            above = above.with(Tier::Cli, false, Tier::Cli.name(), layer.clone());
        }
        if let Some(managed) = self.managed.filter(|m| m.error.is_none()) {
            above = above.with(
                Tier::Managed,
                false,
                managed.origin.clone(),
                serde_json::Value::Object(settings::managed::as_file(managed)),
            );
        }
        Chain(vec![Arc::new(files), Arc::new(above)])
    }
}

/// The managed settings file at `path` — one that must exist when `required`
/// — read against every settings section this build has.
pub(crate) fn read_managed(path: &Path, required: bool) -> Option<ManagedSettings> {
    settings::managed::read(path, required, &crate::settings_sections::all())
}

/// Why `layer` cannot be the `cli` layer of settings: the first key in it no
/// setting reads — an old one named with what replaced it — else the first of
/// its values that `loaded`, read with it, left out. As `(dotted key, reason)`.
pub(crate) fn cli_layer_refusal(
    layer: &serde_json::Value,
    loaded: &LoadedSettings,
) -> Option<(String, String)> {
    if !layer.is_object() {
        return Some((String::new(), "the settings are not a JSON object".into()));
    }
    let keys = settings::SchemaKeys::of(&crate::settings_sections::all());
    if let Some(key) = keys.unknown_in(&[], layer).into_iter().next() {
        let reason = match settings::renamed::renamed_to(&key) {
            Some(new) => format!("renamed to `{new}`; `{key}` is no longer read"),
            None => format!("no setting reads `{key}`"),
        };
        return Some((key, reason));
    }
    loaded
        .ignored
        .iter()
        .find(|ignored| ignored.tier == Tier::Cli.name())
        .map(|ignored| (ignored.key.clone(), ignored.error.clone()))
}

/// [`read_pair`] of a project with each `(path, value)` of `in_memory` read in
/// place of that settings file. It writes nothing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_pair_as_if(
    in_memory: Vec<(PathBuf, serde_json::Value)>,
    global_root: PathBuf,
    scene_root: PathBuf,
    project: &Path,
    layout: &Layout,
    scene: &str,
    default_model: &str,
    above: Above<'_>,
) -> LoadedSettings {
    let local_dir = layout.project_dir(project);
    let files = in_memory.into_iter().fold(
        FileTiers::new(global_root.clone(), scene_root.clone(), local_dir.clone()),
        |source, (path, value)| source.with_contents(path, value),
    );
    let source = above.over(files);
    LoadedSettings::load_from(LoadRequest {
        source: &source,
        global_dir: global_root,
        scene_dir: scene_root,
        local_dir,
        scope: scene,
        default_model,
        sections: &crate::settings_sections::all(),
        layout: layout.clone(),
    })
    .with_managed(above.managed.cloned(), &crate::settings_sections::all())
}

/// Each of [`PROJECT_STATE_ENTRIES`] lying directly in `project`'s project
/// directory moved into its `state/`, where everything reads it from: only
/// where the project directory is a directory of its own inside the project
/// and `state/` is one too, never an entry that is a symbolic link, and never
/// over one already in `state/`. What cannot be moved is left with a warning.
/// A project directory that is one of `state_roots`, holds one or lies in one
/// is left alone: what lies there is the instance's own, not an older
/// engine's project data.
pub(crate) fn move_into_state(project: &Path, layout: &Layout, state_roots: [&Path; 2]) {
    let dir = layout.project_dir(project);
    let real_dir = |p: &Path| p.symlink_metadata().is_ok_and(|m| m.is_dir());
    let Ok(real) = dir.canonicalize() else {
        return;
    };
    let inside = project
        .canonicalize()
        .is_ok_and(|project| real.starts_with(project));
    let overlaps_a_state_root = state_roots.iter().any(|root| {
        root.canonicalize()
            .is_ok_and(|root| real.starts_with(&root) || root.starts_with(&real))
    });
    if !real_dir(&dir) || !inside || overlaps_a_state_root {
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

        move_into_state(&project, &layout, [&elsewhere, &elsewhere]);
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

        move_into_state(&project, &layout, [&elsewhere, &elsewhere]);
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
        move_into_state(&project, &layout, [&base.join("data"), &base.join("data")]);
        assert!(own.join("memory/a.md").is_file());
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());

        std::fs::remove_dir_all(&own).unwrap();
        write(&elsewhere.join("memory/b.md"), "b");
        symlink(&elsewhere, &own).unwrap();
        move_into_state(&project, &layout, [&base.join("data"), &base.join("data")]);
        assert!(elsewhere.join("memory/b.md").is_file());
        assert!(!elsewhere.join("state").exists());
    }
}
