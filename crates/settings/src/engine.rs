//! The settings projected into the types the engine and the permission gate
//! read: [`EngineConfig`], [`EngineBudget`], [`PermissionSettings`], and the
//! session's [`SandboxSettings`].

use base::context::config::EngineConfig;
use base::interface::budget_policy::EngineBudget;
use base::interface::exec::SandboxSettings;
use base::permission::PermissionSettings;

use crate::file::LocalRules;
use crate::{LoadedSettings, SandboxConfig, Settings};

impl LoadedSettings {
    /// The engine's read-only configuration: `EngineConfig::defaults_for`
    /// with every field these settings carry laid over it.
    pub fn engine_config(&self) -> EngineConfig {
        let loaded = self;
        let settings = &loaded.values;
        let mut c = EngineConfig::defaults_for(settings.model.model_name.clone());
        c.max_tokens = settings.model.max_tokens;
        c.fallback_model = settings.model.fallback_model.clone();
        c.permission_prompt_timeout_secs = settings.execution.permission_prompt_timeout_secs;
        c.max_parallelism = settings.execution.max_parallelism;
        c.max_api_calls_per_turn = settings.execution.max_api_calls_per_turn;
        c.max_agent_depth = settings.execution.max_agent_depth;

        c.permission_mode = settings.permission_mode;
        c.sandbox = std::sync::Arc::new(sandbox_settings(
            &settings.sandbox,
            &base::paths::ConfigPaths::from_settings(&loaded.paths),
        ));
        c.disable_skill_shell_execution = settings.disable_skill_shell_execution;
        c.project_trust = loaded.trust();
        c.prompt_files = base::paths::PromptFiles::for_project(
            &base::paths::ConfigPaths::from_settings(&loaded.paths),
        );
        c.path_style = settings.path_style.unwrap_or_default();
        c.subprocess_env = settings.sandbox.subprocess_env.clone();

        if let Some(chars) = settings.compaction.tool_result_inline_chars {
            c.tool_results.inline_chars = chars;
        }

        c.system_prompt = base::context::config::SystemPromptSettings {
            append: settings.prompt_append.clone(),
            override_text: settings.prompt_override.clone(),
            ..c.system_prompt
        };

        c
    }

    /// What the permission gate reads of these settings. A project's
    /// "always allow" is not written where it would not hold: in a `.agix`
    /// that is the global or scene directory (a repository linking its
    /// `.agix` there, a project rooted at `$HOME`) it would allow the call in
    /// every project, and an untrusted project's file is withheld on the next
    /// load.
    pub fn permission_settings(&self) -> PermissionSettings {
        let paths = &self.paths;
        let own = crate::trust::is_own_project_dir(
            &paths.local_data_dir,
            &paths.global_data_dir,
            &paths.user_data_dir,
        );
        let refusal = if !own {
            Some(
                "the project's .agix is the global or scene directory, where the rule would \
                 allow the call in every project",
            )
        } else if paths.project_untrusted {
            Some(
                "the project is not trusted, so its settings would not be read; trust it \
                 first (project.trust)",
            )
        } else {
            None
        };
        PermissionSettings {
            rules: self.rules.clone(),
            managed: self.managed.clone(),
            project_rules: paths.local_settings_file().map(|path| {
                std::sync::Arc::new(match refusal {
                    Some(reason) => LocalRules::refusing(path, reason),
                    None => LocalRules::new(path),
                }) as std::sync::Arc<dyn base::interface::permission::RuleFile>
            }),
        }
    }
}

/// The engine's sandbox settings from the `sandbox` section, the one place
/// that maps one to the other. Every field is named, so a setting added to
/// [`SandboxConfig`] does not compile until it is placed here or set aside.
pub fn sandbox_settings(
    config: &SandboxConfig,
    paths: &base::paths::ConfigPaths,
) -> SandboxSettings {
    let SandboxConfig {
        additional_directories: _,
        read_directories: _,
        read_anywhere: _,
        cache_directories: _,
        deny_read,
        allow_read,
        deny_write,
        allow_write,
        allowed_domains,
        network_mode,
        egress_proxy,
        dangerously_disable_sandbox,
        require_enforcement,
        unattended_requires_network_policy,
        subprocess_env: _,
        reap_process_groups: _,
    } = config;
    SandboxSettings {
        allow_read: allow_read.clone(),
        deny_read: deny_read.clone(),
        network_mode: *network_mode,
        allowed_domains: allowed_domains.clone(),
        egress_proxy: egress_proxy.as_ref().map(|setting| setting.0.clone()),
        deny_write: deny_write.clone(),
        allow_write: allow_write.clone(),
        // The instance's own root, so the profile denies writes to the
        // settings.json this session actually reads — not to whatever sits
        // under the invoking user's home.
        paths: std::sync::Arc::new(
            base::paths::SensitivePaths::for_paths(paths).with_read_denials(
                &base::interface::exec::default_deny_read()
                    .into_iter()
                    .chain(deny_read.iter().cloned())
                    .collect::<Vec<_>>(),
                allow_read,
            ),
        ),
        require_enforcement: *require_enforcement,
        unattended_requires_network_policy: *unattended_requires_network_policy,
        disabled: *dangerously_disable_sandbox,
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
    use base::context::config::PathStyle;
    use base::interface::exec::NetworkMode;
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
    /// Each field below reaches `EngineConfig::sandbox`, which every tool
    /// reads and every `SandboxPolicy` is derived from — a gap anywhere on
    /// that chain makes the setting inert while still appearing in the schema.
    #[test]
    fn every_sandbox_setting_reaches_the_engine_config() {
        let s = settings_with_sandbox(|sb| {
            sb.network_mode = NetworkMode::Allowlist;
            sb.allowed_domains = vec!["api.example.com".into()];
            sb.egress_proxy = serde_json::from_value(serde_json::json!("127.0.0.1:3128")).unwrap();
            sb.allow_read = vec![PathBuf::from("/tmp/ok")];
            sb.deny_read = vec![PathBuf::from("/tmp/secret")];
            sb.deny_write = vec![".secret".into()];
            sb.allow_write = vec![PathBuf::from("/tmp/w")];
            sb.require_enforcement = true;
            sb.unattended_requires_network_policy = true;
            sb.dangerously_disable_sandbox = true;
        });
        let c = config_of(&s);

        assert_eq!(c.sandbox.network_mode, NetworkMode::Allowlist);
        assert_eq!(c.sandbox.allowed_domains, ["api.example.com"]);
        assert_eq!(
            c.sandbox.policy().egress_proxy.map(|p| p.http),
            Some("127.0.0.1:3128".parse().unwrap())
        );
        assert_eq!(c.sandbox.allow_read, [PathBuf::from("/tmp/ok")]);
        assert_eq!(c.sandbox.deny_read, [PathBuf::from("/tmp/secret")]);
        assert_eq!(c.sandbox.policy().deny_read, [PathBuf::from("/tmp/secret")]);
        assert_eq!(c.sandbox.deny_write, [".secret"]);
        assert_eq!(c.sandbox.allow_write, [PathBuf::from("/tmp/w")]);
        assert!(c.sandbox.require_enforcement);
        assert!(c.sandbox.unattended_requires_network_policy);
        assert!(c.sandbox.disabled);
    }

    /// An unconfigured `deny_read` means the built-in credential list, never
    /// "deny exactly nothing" — which would make `~/.ssh` and `~/.aws`
    /// readable by any command the classifier allows.
    #[test]
    fn empty_deny_read_means_defaults_not_an_empty_deny_list() {
        let c = config_of(&settings_with_sandbox(|sb| {
            sb.deny_read = Vec::new();
        }));
        assert_eq!(
            c.sandbox.policy().deny_read,
            base::interface::exec::default_deny_read()
        );
    }

    /// The file tools refuse what `deny_read` names too, not only the
    /// built-in credential list.
    #[test]
    fn a_configured_deny_read_is_a_credential_to_the_file_tools() {
        let c = config_of(&settings_with_sandbox(|sb| {
            sb.deny_read = vec![PathBuf::from("/tmp/agix-deny-read-case")];
        }));
        assert!(c
            .sandbox
            .paths
            .is_credential(std::path::Path::new("/tmp/agix-deny-read-case/key.txt")));
        assert!(!c
            .sandbox
            .paths
            .is_credential(std::path::Path::new("/tmp/agix-elsewhere/key.txt")));
    }

    /// The setting file says `"network_mode": "deny_all"`; that is what the
    /// engine type reads and writes.
    #[test]
    fn network_mode_keeps_its_wire_names() {
        for (mode, wire) in [
            (NetworkMode::Unrestricted, "unrestricted"),
            (NetworkMode::DenyAll, "deny_all"),
            (NetworkMode::Allowlist, "allowlist"),
            (NetworkMode::Proxy, "proxy"),
        ] {
            assert_eq!(serde_json::to_value(mode).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<NetworkMode>(serde_json::json!(wire)).unwrap(),
                mode
            );
        }
    }
}
