//! What a session runs on: the settings loaded for its (project, scene) pair,
//! shared by every session of the pair and never changed for one; the
//! session's own settings layer and the effective settings
//! [`LoadedSettings::overlay`] derives from the two; and what the session
//! decided for itself — its permission mode, its model, the directory it works
//! in and the worktree it was given — kept beside the settings, never written
//! into them.
//!
//! Code that needs a setting reads [`SessionConfig::settings`]; code that needs
//! what the session decided reads the accessor of that name; code that needs
//! what the files say reads [`SessionConfig::shared`].

use std::path::Path;
use std::sync::Arc;

use base::context::config::EngineConfig;
use base::interface::model::Effort;
use base::paths::{LinkedWorktree, Workspace, WorktreeGit};
use base::permission::PermissionMode;
use serde_json::{Map, Value};

use crate::overlay::{value_at, HeldKey, Subject};
use crate::section::Sections;
use crate::LoadedSettings;

#[derive(Debug, Clone)]
pub struct SessionConfig {
    shared: Arc<LoadedSettings>,
    layer: Option<Arc<Map<String, Value>>>,
    held: Arc<[HeldKey]>,
    effective: Arc<LoadedSettings>,
    permission_mode: PermissionMode,
    model_name: String,
    workspace: Workspace,
    worktree: Option<LinkedWorktree>,
}

/// What a delegate's agent type sets over the settings it inherits, already
/// held to what its parent allows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DelegateTuning {
    pub effort: Option<Effort>,
    pub max_model_calls: Option<u32>,
}

impl SessionConfig {
    /// `settings` with no session layer, and the session in their permission
    /// mode, on their main model, in their workspace.
    pub fn of(settings: Arc<LoadedSettings>) -> Self {
        Self::derived(settings.clone(), None, Vec::new(), settings)
    }

    /// `shared` with the session's own settings `layer` laid over it, as a
    /// session's ([`Subject::Session`]): what the layer may not set is left
    /// out, and what would loosen `shared` is held, in [`held`](Self::held).
    pub fn with_session_layer(
        shared: Arc<LoadedSettings>,
        layer: Option<&Map<String, Value>>,
        sections: &Sections,
    ) -> Self {
        let Some(layer) = layer else {
            return Self::of(shared);
        };
        let (effective, held) = shared.overlay(layer, Subject::Session, sections);
        Self::derived(
            shared,
            Some(Arc::new(layer.clone())),
            held,
            Arc::new(effective),
        )
    }

    fn derived(
        shared: Arc<LoadedSettings>,
        layer: Option<Arc<Map<String, Value>>>,
        held: Vec<HeldKey>,
        effective: Arc<LoadedSettings>,
    ) -> Self {
        Self {
            permission_mode: effective.values.permissions.mode,
            model_name: effective.main_model.clone(),
            workspace: effective.paths.workspace.clone(),
            worktree: None,
            shared,
            layer,
            held: held.into(),
            effective,
        }
    }

    /// The mode the session was decided to run in. The settings'
    /// `permissions.mode` stays what they say.
    pub fn with_permission_mode(mut self, mode: PermissionMode) -> Self {
        self.permission_mode = mode;
        self
    }

    pub fn with_model_name(mut self, model: impl Into<String>) -> Self {
        self.model_name = model.into();
        self
    }

    /// Where the session works when it is in no worktree: its project, or a
    /// scratch directory of its own.
    pub fn with_workspace(mut self, workspace: Workspace) -> Self {
        self.workspace = workspace;
        self
    }

    pub fn with_worktree(mut self, worktree: Option<LinkedWorktree>) -> Self {
        self.worktree = worktree;
        self
    }

    /// These settings as a delegate of an agent type runs on them: the
    /// effective settings with what the type sets over them, and the managed
    /// layer over that — a tighter value the type asks for stands, a looser
    /// one is held to the managed layer's. The shared settings and the
    /// session layer are the parent's.
    pub fn tuned_for_delegate(mut self, tuning: &DelegateTuning) -> Self {
        if *tuning == DelegateTuning::default() {
            return self;
        }
        let mut effective = (*self.effective).clone();
        if let Some(effort) = tuning.effort {
            effective.values.model.effort = Some(effort);
        }
        if let Some(calls) = tuning.max_model_calls {
            effective.values.turn.max_model_calls = calls;
        }
        effective.hold_to_managed(&["model.effort", "turn.max_model_calls"]);
        self.effective = Arc::new(effective);
        self
    }

    /// The settings as loaded for the session's (project, scene) pair, with
    /// the cli, host and managed layers: what the files say.
    pub fn shared(&self) -> &Arc<LoadedSettings> {
        &self.shared
    }

    /// The settings the session runs on: [`shared`](Self::shared) with its
    /// own layer laid over it.
    pub fn settings(&self) -> &Arc<LoadedSettings> {
        &self.effective
    }

    /// The session's own settings, as the host gave them.
    pub fn session_layer(&self) -> Option<&Map<String, Value>> {
        self.layer.as_deref()
    }

    /// The keys of the session layer the shared settings held to their own
    /// value, with what was asked for and what is in force.
    pub fn held(&self) -> &[HeldKey] {
        &self.held
    }

    /// What the session layer asks for at the dotted `key`.
    pub fn requested(&self, key: &str) -> Option<Value> {
        let layer = Value::Object(self.layer.as_deref()?.clone());
        Some(value_at(&layer, key)).filter(|v| !v.is_null())
    }

    /// What the session runs with at the dotted `key`: the effective
    /// setting, and for `permissions.mode` the mode it was decided to run in.
    pub fn applied(&self, key: &str) -> Value {
        if key == "permissions.mode" {
            return serde_json::to_value(self.permission_mode).unwrap_or_default();
        }
        let values = serde_json::to_value(&self.effective.values).unwrap_or_default();
        value_at(&values, key)
    }

    pub fn permission_mode(&self) -> PermissionMode {
        self.permission_mode
    }

    /// The conversation's model.
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn worktree(&self) -> Option<&LinkedWorktree> {
        self.worktree.as_ref()
    }

    /// The tools' working directory: the root a write needs no approval in,
    /// a read none either, and a sandboxed command may write — the worktree
    /// when the session has one, else its workspace.
    pub fn working_dir(&self) -> &Path {
        if let Some(worktree) = &self.worktree {
            return &worktree.dir;
        }
        match &self.workspace {
            Workspace::Project(dir) | Workspace::Scratch(dir) => dir,
        }
    }

    /// What of the repository's git directory a sandboxed command in the
    /// session's worktree may write, while it is still the project's own.
    pub fn worktree_git(&self) -> Option<WorktreeGit> {
        let git = self.worktree.as_ref()?.git.as_ref()?;
        self.effective.paths.project_worktree_git(git)
    }

    /// [`LoadedSettings::engine_config`] of the effective settings, on the
    /// session's model, in its mode, with its worktree's git directory.
    pub fn engine_config(&self) -> EngineConfig {
        self.effective.engine_config_for(
            &self.model_name,
            self.permission_mode,
            self.worktree_git(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use serde_json::json;

    fn shared(f: impl FnOnce(&mut Settings)) -> Arc<LoadedSettings> {
        let mut values = Settings::defaults_for("m");
        f(&mut values);
        Arc::new(LoadedSettings::new(values, Default::default()))
    }

    fn layer(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn what_the_session_decides_stays_out_of_the_settings() {
        let shared = shared(|s| s.permissions.mode = PermissionMode::AcceptEdits);
        let config = SessionConfig::of(shared.clone())
            .with_permission_mode(PermissionMode::Plan)
            .with_model_name("other");
        assert_eq!(config.permission_mode(), PermissionMode::Plan);
        assert_eq!(config.model_name(), "other");
        assert_eq!(
            config.settings().values.permissions.mode,
            PermissionMode::AcceptEdits
        );
        assert_eq!(config.settings().main_model, "m");
        assert_eq!(config.engine_config().model, "other");
        assert_eq!(config.engine_config().permission_mode, PermissionMode::Plan);
        assert_eq!(config.applied("permissions.mode"), json!("plan"));
    }

    #[test]
    fn the_session_layer_is_laid_over_the_shared_settings_not_into_them() {
        let shared = shared(|s| s.turn.max_model_calls = 50);
        let config = SessionConfig::with_session_layer(
            shared.clone(),
            Some(&layer(json!({"turn": {"max_model_calls": 10}}))),
            &Sections::none(),
        );
        assert_eq!(config.settings().values.turn.max_model_calls, 10);
        assert_eq!(config.shared().values.turn.max_model_calls, 50);
        assert_eq!(config.requested("turn.max_model_calls"), Some(json!(10)));
        assert_eq!(config.applied("turn.max_model_calls"), json!(10));
        assert!(config.held().is_empty());
    }

    #[test]
    fn a_held_key_reports_what_was_asked_and_what_applies() {
        let shared = shared(|s| s.turn.max_model_calls = 5);
        let config = SessionConfig::with_session_layer(
            shared,
            Some(&layer(json!({"turn": {"max_model_calls": 500}}))),
            &Sections::none(),
        );
        assert_eq!(config.requested("turn.max_model_calls"), Some(json!(500)));
        assert_eq!(config.applied("turn.max_model_calls"), json!(5));
        assert_eq!(config.held()[0].key, "turn.max_model_calls");
    }

    fn under_managed(managed: Value) -> SessionConfig {
        let managed = crate::managed::from_value("it", managed, &Sections::none());
        let settings = shared(|_| {})
            .as_ref()
            .clone()
            .with_managed(Some(managed), &Sections::none());
        SessionConfig::of(Arc::new(settings))
    }

    #[test]
    fn a_delegates_tuning_stays_under_the_managed_layer() {
        let managed = json!({"turn": {"max_model_calls": 10}, "model": {"effort": "low"}});
        let raised = under_managed(managed.clone()).tuned_for_delegate(&DelegateTuning {
            effort: Some(Effort::High),
            max_model_calls: Some(50),
        });
        assert_eq!(raised.settings().values.turn.max_model_calls, 10);
        assert_eq!(raised.settings().values.model.effort, Some(Effort::Low));

        let lowered = under_managed(managed).tuned_for_delegate(&DelegateTuning {
            effort: None,
            max_model_calls: Some(5),
        });
        assert_eq!(lowered.settings().values.turn.max_model_calls, 5);
        assert!(lowered.settings().values.providers[0].builtin);
    }

    #[test]
    fn a_worktree_is_where_the_session_works() {
        let config = SessionConfig::of(shared(|_| {}))
            .with_workspace(Workspace::Project("/p".into()))
            .with_worktree(Some(LinkedWorktree {
                dir: "/p/.agix/worktrees/w".into(),
                git: None,
            }));
        assert_eq!(config.working_dir(), Path::new("/p/.agix/worktrees/w"));
        assert_eq!(config.settings().paths.workspace_dir(), Path::new(""));
    }
}
