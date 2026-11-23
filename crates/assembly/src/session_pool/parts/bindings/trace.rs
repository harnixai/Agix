//! 一对 `(project, scene)` 的设置连同它的来历：加载器折叠的各层原样、叠在上面的宿主覆盖层，
//! 以及启动时读定的那些键当时的值——`config.explain` 从这些重算每个键为什么是这个值。

use std::path::Path;

use super::{BindingCache, LoadedSettings};
use crate::session_pool::off_the_runtime;
use crate::settings_source::{read_pair, trace_pair, Above, PairRead};

/// A pair's settings with what they were made of: the layers the loader
/// folded, as the source handed them over, and the host's layer laid over
/// what it made of them.
pub(in crate::session_pool) struct PairTrace {
    pub(in crate::session_pool) layers: Vec<base::interface::config_source::ConfigLayer>,
    pub(in crate::session_pool) host: Option<serde_json::Map<String, serde_json::Value>>,
    pub(in crate::session_pool) settings: LoadedSettings,
}

impl BindingCache {
    /// The pair's settings as [`load_pair`](Self::load_pair) reads them, with
    /// the layers the loader folded and the host's layer laid over them.
    pub(in crate::session_pool) async fn trace_pair(
        &self,
        project: Option<&Path>,
        scene: &str,
    ) -> PairTrace {
        let own = self.is_own_pair(project, scene).await;
        let (loaded, layers) = self.read_pair_files(project, scene, true).await;
        let host = self.layers.host.layer_over(&loaded, own);
        PairTrace {
            layers,
            host,
            settings: self.layers.over(loaded, own),
        }
    }

    /// The pair's files with the `cli` and managed layers over them; with
    /// `traced`, also the layers as the loader was handed them.
    pub(super) async fn read_pair_files(
        &self,
        project: Option<&Path>,
        scene: &str,
        traced: bool,
    ) -> (
        LoadedSettings,
        Vec<base::interface::config_source::ConfigLayer>,
    ) {
        let startup_scene = self.startup_scene().await;
        let default_model = self.settings.read().await.default_model.clone();
        let (global_root, scene_root, scene, project, cli, managed) = (
            self.paths.global_root(),
            self.scene_root(scene, &startup_scene),
            scene.to_string(),
            project.map(Path::to_path_buf),
            self.layers.cli.clone(),
            self.layers.managed.settings.clone(),
        );
        let layout = self.paths.layout();
        off_the_runtime(move || {
            let request = PairRead {
                global_root,
                scene_root,
                project: project.as_deref(),
                layout: &layout,
                scene: &scene,
                default_model: &default_model,
                above: Above {
                    cli: cli.as_ref(),
                    managed: managed.as_ref(),
                },
            };
            if traced {
                trace_pair(request)
            } else {
                (read_pair(request), Vec::new())
            }
        })
        .await
    }

    /// What this process started with for a key read only at start.
    pub(in crate::session_pool) fn started_with(&self, key: &str) -> Option<&serde_json::Value> {
        self.started_with
            .iter()
            .find(|(started, _)| started == key)
            .map(|(_, value)| value)
    }
}
