//! The settings projected into the types the engine and the permission gate
//! read: [`EngineConfig`], [`EngineBudget`], [`PermissionSettings`].

use base::context::config::EngineConfig;
use base::interface::budget_policy::EngineBudget;
use base::interface::tool::SandboxSettings;
use base::permission::PermissionSettings;

use crate::file::LocalRules;
use crate::{LoadedSettings, Settings};

impl LoadedSettings {
    /// The engine's read-only configuration: `EngineConfig::defaults_for`
    /// with every field these settings carry laid over it.
    pub fn engine_config(&self) -> EngineConfig {
        let loaded = self;
        let settings = &loaded.values;
        let mut c = EngineConfig::defaults_for(settings.model.model_name.clone());
        c.max_tokens = settings.model.max_tokens;
        c.fallback_model = settings.model.fallback_model.clone();
        c.thinking_mode = match settings.model.thinking_mode {
            base::interface::model::ThinkingMode::Auto => {
                base::context::config::ThinkingModeConfig::Auto
            }
            base::interface::model::ThinkingMode::Off => {
                base::context::config::ThinkingModeConfig::Off
            }
            base::interface::model::ThinkingMode::On => {
                base::context::config::ThinkingModeConfig::On
            }
            base::interface::model::ThinkingMode::OnBudget(n) => {
                base::context::config::ThinkingModeConfig::OnBudget(n)
            }
        };

        c.permission_prompt_timeout_secs = settings.execution.permission_prompt_timeout_secs;
        c.max_parallelism = settings.execution.max_parallelism;
        c.max_api_calls_per_turn = settings.execution.max_api_calls_per_turn;
        c.max_agent_depth = settings.execution.max_agent_depth;

        c.permission_mode = settings.permission_mode;
        c.dangerously_disable_sandbox = settings.sandbox.dangerously_disable_sandbox;
        // `Settings.sandbox.deny_read` is a plain `Vec`, empty when the user
        // never configured it — mapping that to `Some(vec![])` would read as
        // "use exactly this empty list", which *disables* the sandbox's
        // built-in deny defaults (~/.ssh, ~/.aws, etc). Empty must map to
        // `None` ("use built-in defaults"), matching `SandboxPolicyConfig`'s
        // own documented Option semantics.
        c.sandbox_policy = base::context::config::SandboxPolicyConfig {
            allow_read: settings.sandbox.allow_read.clone(),
            deny_write: settings.sandbox.deny_write.clone(),
            allow_write: settings.sandbox.allow_write.clone(),
            deny_read: if settings.sandbox.deny_read.is_empty() {
                None
            } else {
                Some(settings.sandbox.deny_read.clone())
            },
            network_mode: settings.sandbox.network_mode,
            allowed_domains: settings.sandbox.allowed_domains.clone(),
            // The instance's own root, so the profile denies writes to the
            // settings.json this session actually reads — not to whatever
            // sits under the invoking user's home.
            paths: std::sync::Arc::new(base::paths::SensitivePaths::for_paths(
                &base::paths::ConfigPaths::from_settings(&loaded.paths),
            )),
            require_enforcement: settings.sandbox.require_enforcement,
            unattended_requires_network_policy: settings.sandbox.unattended_requires_network_policy,
        };
        c.disable_skill_shell_execution = settings.disable_skill_shell_execution;
        c.project_trust = loaded.trust();
        c.prompt_files = base::paths::PromptFiles::for_project(
            &base::paths::ConfigPaths::from_settings(&loaded.paths),
        );
        c.path_style = settings.path_style.unwrap_or_default();
        c.subprocess_env = settings.sandbox.subprocess_env.clone();

        c.system_prompt = base::context::config::SystemPromptSettings {
            append: settings.prompt_append.clone(),
            override_text: settings.prompt_override.clone(),
            ..c.system_prompt
        };

        c
    }

    /// What the permission gate reads of these settings.
    pub fn permission_settings(&self) -> PermissionSettings {
        let sandbox = &self.values.sandbox;
        PermissionSettings {
            rules: self.rules.clone(),
            managed: self.managed.clone(),
            sandbox: SandboxSettings {
                allow_read: sandbox.allow_read.clone(),
                deny_read: sandbox.deny_read.clone(),
                allowed_domains: sandbox.allowed_domains.clone(),
                network_mode: sandbox.network_mode,
                deny_write: sandbox.deny_write.clone(),
                allow_write: sandbox.allow_write.clone(),
                paths: std::sync::Arc::new(base::paths::SensitivePaths::for_paths(
                    &base::paths::ConfigPaths::from_settings(&self.paths),
                )),
                require_enforcement: sandbox.require_enforcement,
                unattended_requires_network_policy: sandbox.unattended_requires_network_policy,
            },
            project_rules: self.paths.local_settings_file().map(|path| {
                std::sync::Arc::new(LocalRules::new(path))
                    as std::sync::Arc<dyn base::interface::permission::RuleFile>
            }),
        }
    }
}

impl Settings {
    pub fn engine_budget(&self) -> EngineBudget {
        EngineBudget {
            max_total_tokens: self.execution.max_budget_tokens,
            compact_threshold: self.compaction.threshold_tokens,
            compact_keep_recent: self.compaction.keep_recent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SandboxConfig;
    use base::context::config::{NetworkModeConfig, PathStyle};
    use base::paths::PathSettings;
    use std::path::PathBuf;

    fn config_of(s: &Settings) -> EngineConfig {
        LoadedSettings::new(s.clone(), PathSettings::default()).engine_config()
    }

    #[test]
    fn path_style_is_absolute_unless_settings_say_otherwise() {
        let mut s = Settings::defaults_for("test-model");
        assert_eq!(config_of(&s).path_style, PathStyle::Absolute);
        s = serde_json::from_value(serde_json::json!({
            "model": serde_json::to_value(&s.model).unwrap(),
            "path_style": "workspace_relative",
        }))
        .unwrap();
        assert_eq!(config_of(&s).path_style, PathStyle::WorkspaceRelative);
    }

    #[test]
    fn the_delegation_depth_is_read_from_settings() {
        let mut s = Settings::defaults_for("test-model");
        assert_eq!(config_of(&s).max_agent_depth, 3);
        s.execution.max_agent_depth = 1;
        assert_eq!(config_of(&s).max_agent_depth, 1);
    }

    fn settings_with_sandbox(f: impl FnOnce(&mut SandboxConfig)) -> Settings {
        let mut s = Settings::defaults_for("test-model");
        f(&mut s.sandbox);
        s
    }

    /// The whole point of `settings.sandbox` is that a user can configure it.
    /// Each field below reaches `EngineConfig`, which `runtime::turn` copies
    /// into `ToolContext.sandbox` and `tools::bash` lifts into a real
    /// `SandboxPolicy` — a gap anywhere on that chain makes the setting inert
    /// while still appearing in the schema.
    #[test]
    fn every_sandbox_setting_reaches_the_engine_config() {
        let s = settings_with_sandbox(|sb| {
            sb.network_mode = NetworkModeConfig::Allowlist;
            sb.allowed_domains = vec!["api.example.com".into()];
            sb.allow_read = vec![PathBuf::from("/tmp/ok")];
            sb.deny_read = vec![PathBuf::from("/tmp/secret")];
            sb.dangerously_disable_sandbox = true;
        });
        let c = config_of(&s);

        assert_eq!(c.sandbox_policy.network_mode, NetworkModeConfig::Allowlist);
        assert_eq!(c.sandbox_policy.allowed_domains, ["api.example.com"]);
        assert_eq!(c.sandbox_policy.allow_read, [PathBuf::from("/tmp/ok")]);
        assert_eq!(
            c.sandbox_policy.deny_read,
            Some(vec![PathBuf::from("/tmp/secret")])
        );
        assert!(c.dangerously_disable_sandbox);
    }

    /// An unconfigured `deny_read` must arrive as `None` ("use the built-in
    /// credential deny defaults"), never as `Some(vec![])` — which reads as
    /// "deny exactly nothing" and would switch those defaults off, making
    /// `~/.ssh` and `~/.aws` readable by any command the classifier allows.
    #[test]
    fn empty_deny_read_means_defaults_not_an_empty_deny_list() {
        let c = config_of(&settings_with_sandbox(|sb| {
            sb.deny_read = Vec::new();
        }));
        assert_eq!(c.sandbox_policy.deny_read, None);
    }
}
