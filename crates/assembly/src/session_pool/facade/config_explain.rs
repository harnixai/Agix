//! `config.explain`：一个设置键为什么是这个值。照加载时的样子重读这一对的设置并留下各层原样，
//! 交给 `settings::explain` 逐层判；这里补上会话那一层、启动时读定的键是否待重启，并按需脱敏。

use crate::session_pool::parts::factory::session_settings;
use crate::session_pool::*;
use settings::explain::{KeyExplanation, SettingExplanation, Sources};

impl SessionPool {
    /// `config.explain {key, scene?, project_root?, session_id?}` — why the
    /// dotted `key` (or each key of a block) has its value: every layer that
    /// went into it and what became of it there, read from the files as they
    /// are now.
    pub async fn explain_setting(
        &self,
        key: &str,
        target: api::ExplainTarget,
        include_secrets: bool,
    ) -> Result<SettingExplanation, PoolError> {
        let (project, scene, spec) = match target {
            api::ExplainTarget::Pair {
                project_root,
                scene,
            } => (project_root, scene, None),
            api::ExplainTarget::Session(session_id) => {
                let (project, scene) = self.pair_of_session(&session_id).await?;
                let spec = match self.specs.get(&session_id) {
                    Some(spec) => spec,
                    None => self.specs.recorded(&session_id).await,
                };
                (project, scene, Some(spec))
            }
        };
        if self.scenes.registry().resolve(&scene).is_none() {
            return Err(PoolError::SceneNotFound { scene });
        }
        let trace = self.bindings.trace_pair(project.as_deref(), &scene).await;
        let shared = Arc::new(trace.settings);
        let session = spec.map(|spec| {
            session_settings(
                &shared,
                spec.settings_layer(),
                spec.requested_permission_mode(),
            )
            .0
        });
        let sources = Sources {
            layers: &trace.layers,
            host: trace.host.as_ref(),
            settings: &shared,
            session: session.as_ref(),
        };
        let mut explanation =
            settings::explain::explain(&sources, key, &crate::settings_sections::all())
                .map_err(PoolError::InvalidRequest)?;
        for explained in &mut explanation.keys {
            if explained.read_at_start {
                explained.restart_pending = self
                    .bindings
                    .started_with(&explained.key)
                    .map(|then| *then != explained.value);
            }
            if !include_secrets {
                redact_explained(explained);
            }
        }
        Ok(explanation)
    }

    /// The project and scene of a session in memory, or of one this pool can
    /// resume.
    async fn pair_of_session(
        &self,
        session_id: &str,
    ) -> Result<(Option<PathBuf>, String), PoolError> {
        if let Some(core) = self.live.core_of(session_id).await {
            return Ok((
                core.project().map(Path::to_path_buf),
                core.scene().id.clone(),
            ));
        }
        let info = self.cold_session_info(session_id).await?;
        let scene = match info.scene {
            Some(scene) => scene,
            None => self.bindings.startup_scene().await,
        };
        Ok((info.project_root.map(PathBuf::from), scene))
    }
}

/// Every value of `explained` redacted as `config.get` redacts a settings
/// file: each put back at its key first, since what is a secret depends on
/// the keys it sits under. Why a value was ignored is said without it.
fn redact_explained(explained: &mut KeyExplanation) {
    let key = explained.key.clone();
    let redact = |value: &mut serde_json::Value| {
        let mut file = value.take();
        for segment in key.rsplit('.') {
            let mut wrapped = serde_json::Map::new();
            wrapped.insert(segment.to_string(), file);
            file = serde_json::Value::Object(wrapped);
        }
        redact_settings_secrets(&mut file);
        *value = settings_value_at(&file, &key);
    };
    redact(&mut explained.value);
    for layer in &mut explained.layers {
        if let Some(error) = &mut layer.error {
            *error = shown_error(&key, layer.value.as_ref(), error);
        }
        if let Some(value) = &mut layer.value {
            redact(value);
        }
    }
    if let Some(managed) = &mut explained.managed {
        redact(&mut managed.value);
    }
    if let Some(session) = &mut explained.session {
        if let Some(error) = &mut session.error {
            *error = shown_error(&key, session.requested.as_ref(), error);
        }
        redact(&mut session.applied);
        if let Some(requested) = &mut session.requested {
            redact(requested);
        }
    }
}

fn settings_value_at(root: &serde_json::Value, dotted: &str) -> serde_json::Value {
    dotted
        .split('.')
        .try_fold(root, |node, segment| node.get(segment))
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}
