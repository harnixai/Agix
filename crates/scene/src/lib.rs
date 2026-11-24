//! Built-in scene implementations, declarative scenes, and the registry hosts
//! resolve them from.
//!
//! Agix provides `code` (full-capability coding agent), `harnix` (the
//! default scene of Harnix-hosted sessions) and `mini` (the smallest working
//! coding agent). [`declarative`] turns a `scene.json` + `prompt.md`
//! directory into a scene.

pub mod code;
mod common;
pub mod declarative;
pub mod harnix;
pub mod mini;

pub use declarative::{DeclaredScene, SceneLoadError};

use base::interface::scene::SceneSpec;
use base::paths::Layout;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

/// Scene metadata for discovery.
#[derive(Debug, Clone)]
pub struct SceneInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    /// See [`SceneSpec::requires_project`] — hosts use it to decide whether
    /// creating a session needs a project first.
    pub requires_project: bool,
    /// See [`SceneSpec::supports_team`].
    pub supports_team: bool,
}

/// A built-in scene by id: `code` (interactive), `harnix` or `mini`, named
/// for `layout`'s product.
pub fn builtin(id: &str, layout: &Layout) -> Option<SceneSpec> {
    builtins(layout).into_iter().find(|s| s.id == id)
}

/// `<product> <title>`: what `code` and `mini` are called.
pub(crate) fn product_title(layout: &Layout, title: &str) -> String {
    format!("{} {title}", layout.product_name())
}

fn builtins(layout: &Layout) -> [SceneSpec; 3] {
    let mut code = code::spec(code::Interaction::Interactive);
    code.name = product_title(layout, code::TITLE);
    let mut mini = mini::spec();
    mini.name = product_title(layout, mini::TITLE);
    [code, harnix::spec(), mini]
}

/// Registry of available scenes. Populated at process startup.
#[derive(Default)]
pub struct SceneRegistry {
    scenes: HashMap<String, Arc<SceneSpec>>,
    withheld: HashSet<String>,
}

impl SceneRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, scene: Arc<SceneSpec>) {
        self.scenes.insert(scene.id.clone(), scene);
    }
    pub fn resolve(&self, id: &str) -> Option<Arc<SceneSpec>> {
        self.scenes.get(id).cloned()
    }
    pub fn ids(&self) -> Vec<String> {
        self.scenes.keys().cloned().collect()
    }
    /// This registry as one session sees it: the scenes `keep` turns down
    /// resolve to nothing, and are [`withheld`](Self::withholds).
    pub fn narrowed(&self, keep: impl Fn(&str) -> bool) -> Self {
        let (scenes, dropped): (HashMap<_, _>, HashMap<_, _>) = self
            .scenes
            .iter()
            .map(|(id, scene)| (id.clone(), scene.clone()))
            .partition(|(id, _)| keep(id));
        Self {
            scenes,
            withheld: self
                .withheld
                .iter()
                .cloned()
                .chain(dropped.into_keys())
                .collect(),
        }
    }
    /// Whether `id` is a scene this registry has but keeps from its session.
    pub fn withholds(&self, id: &str) -> bool {
        self.withheld.contains(id)
    }
    pub fn list_all(&self) -> Vec<SceneInfo> {
        self.scenes
            .iter()
            .map(|(id, s)| SceneInfo {
                id: id.clone(),
                name: s.name.clone(),
                description: s.description.clone(),
                requires_project: s.requires_project,
                supports_team: s.supports_team,
            })
            .collect()
    }
    /// Register a declarative scene unless its id is already taken.
    ///
    /// Call after the built-in and host scenes are in: a file in a user's
    /// home directory must not be able to stand in for `code` under the
    /// same name.
    pub fn register_declarative(&mut self, scene: DeclaredScene) -> Result<(), SceneLoadError> {
        if self.scenes.contains_key(&scene.spec.id) {
            return Err(SceneLoadError::IdTaken {
                path: scene.dir.join(declarative::SCENE_FILE),
                id: scene.spec.id,
            });
        }
        self.register(Arc::new(scene.spec));
        Ok(())
    }

    /// Load every scene under `root` (see [`declarative::load_all`]) and
    /// register those whose id is free. Returns what could not be loaded or
    /// registered; the rest are in.
    pub fn register_declarative_dir(&mut self, root: &Path) -> Vec<SceneLoadError> {
        declarative::load_all(root)
            .into_iter()
            .filter_map(|loaded| loaded.and_then(|s| self.register_declarative(s)).err())
            .collect()
    }

    /// The built-in scenes, named for `layout`'s product.
    pub fn register_builtin(&mut self, layout: &Layout) {
        for spec in builtins(layout) {
            self.register(Arc::new(spec));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_narrowed_registry_resolves_only_what_it_keeps_and_knows_the_rest() {
        let mut registry = SceneRegistry::new();
        registry.register_builtin(&Layout::default());
        let narrowed = registry.narrowed(|id| id != "mini");
        assert!(narrowed.resolve("mini").is_none());
        assert!(narrowed.withholds("mini"));
        assert!(narrowed.resolve("code").is_some());
        assert!(!narrowed.withholds("code") && !narrowed.withholds("nowhere"));
        assert!(narrowed.narrowed(|_| true).withholds("mini"));
    }

    #[test]
    fn the_built_in_scenes_are_named_for_the_product() {
        let codix = Layout::default().with_product_name("Codix").unwrap();
        assert_eq!(builtin("code", &codix).unwrap().name, "Codix Code");
        assert_eq!(builtin("mini", &codix).unwrap().name, "Codix Mini");
        assert_eq!(builtin("code", &codix).unwrap().id, "code");
        assert_eq!(
            builtin("code", &Layout::default()).unwrap().name,
            "Agix Code"
        );
        let mut registry = SceneRegistry::new();
        registry.register_builtin(&codix);
        assert_eq!(registry.resolve("mini").unwrap().name, "Codix Mini");
    }

    fn write_scene(root: &Path, id: &str) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(declarative::SCENE_FILE),
            format!(r#"{{"id": "{id}", "name": "{id}"}}"#),
        )
        .unwrap();
        std::fs::write(dir.join(declarative::PROMPT_FILE), "prompt").unwrap();
    }

    #[test]
    fn a_declarative_scene_cannot_shadow_a_built_in_one() {
        let root = tempfile::tempdir().unwrap();
        write_scene(root.path(), "code");
        write_scene(root.path(), "review");

        let mut registry = SceneRegistry::new();
        registry.register_builtin(&Default::default());
        let refused = registry.register_declarative_dir(root.path());

        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(
            matches!(&refused[0], SceneLoadError::IdTaken { id, .. } if id == "code"),
            "{refused:?}"
        );
        assert!(refused[0].to_string().contains("already registered"));
        assert_eq!(
            registry.resolve("code").unwrap().name,
            builtin("code", &Layout::default()).unwrap().name,
            "the built-in stays in place"
        );
        assert_eq!(registry.resolve("review").unwrap().name, "review");
    }

    #[test]
    fn listing_includes_declarative_scenes() {
        let root = tempfile::tempdir().unwrap();
        write_scene(root.path(), "review");
        let mut registry = SceneRegistry::new();
        assert!(registry.register_declarative_dir(root.path()).is_empty());
        let listed = registry.list_all();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "review");
    }
}
