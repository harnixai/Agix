//! The settings projected into the types the engine and the permission gate
//! read: [`EngineConfig`], [`EngineBudget`], [`PermissionSettings`], and the
//! session's [`SandboxSettings`].

use base::context::config::EngineConfig;
use base::interface::budget_policy::EngineBudget;
use base::interface::exec::SandboxSettings;
use base::paths::WorktreeGit;
use base::permission::{PermissionMode, PermissionSettings};

use crate::file::LocalRules;
use crate::{LoadedSettings, SandboxConfig, Settings};

impl LoadedSettings {
    /// The engine's read-only configuration: `EngineConfig::defaults_for`
    /// with every field these settings carry laid over it, on their main
    /// model and in their permission mode. A session's is
    /// [`SessionConfig::engine_config`](crate::SessionConfig::engine_config).
    pub fn engine_config(&self) -> EngineConfig {
        self.engine_config_for(&self.main_model, self.values.permissions.mode, None)
    }

    pub(crate) fn engine_config_for(
        &self,
        model: &str,
        mode: PermissionMode,
        worktree_git: Option<WorktreeGit>,
    ) -> EngineConfig {
        let loaded = self;
        let settings = &loaded.values;
        let mut c = EngineConfig::defaults_for(model.to_string());
        c.max_tokens = settings.model.max_tokens;
        c.permission_prompt_timeout_secs = settings.permissions.prompt_timeout_secs;
        c.max_parallelism = settings.turn.parallel_tools;
        c.max_api_calls_per_turn = settings.turn.max_model_calls;
        c.max_agent_depth = settings.agents.max_depth;
        c.model_autonomy.goal_max_rounds = settings.goal.max_rounds;
        c.model_autonomy.max_jobs = settings.schedule.max_model_jobs;
        c.model_autonomy.min_job_interval_minutes = settings.schedule.min_model_interval_minutes;

        c.permission_mode = mode;
        c.sandbox = std::sync::Arc::new(SandboxSettings {
            worktree_git,
            ..sandbox_settings(
                settings,
                &base::paths::ConfigPaths::from_settings(&loaded.paths),
            )
        });
        c.disable_skill_shell_execution = !settings.skills.allow_shell;
        c.project_trust = loaded.trust();
        c.prompt_files = base::paths::PromptFiles::for_project(
            &base::paths::ConfigPaths::from_settings(&loaded.paths),
        );
        c.layout = loaded.paths.layout.clone();
        c.path_style = settings.prompt.path_style.unwrap_or_default();
        c.subprocess_env = settings.sandbox.env.clone();

        if let Some(chars) = settings.compaction.inline_result_chars {
            c.tool_results.inline_chars = chars;
        }

        c
    }

    /// What the permission gate reads of these settings. A project's
    /// "always allow" is not written where it would not hold: in a project directory
    /// that is the global or scene directory (a repository linking its
    /// project directory there, a project rooted at `$HOME`) it would allow the call in
    /// every project, in one linked into another project it would allow it
    /// there, and an untrusted project's file is withheld on the next load.
    pub fn permission_settings(&self) -> PermissionSettings {
        let paths = &self.paths;
        use crate::trust::NotItsOwnTier;
        let not_own = crate::trust::project_tier_refusal(
            &paths.local_data_dir,
            &paths.global_data_dir,
            &paths.user_data_dir,
        );
        let refusal = if let Some(NotItsOwnTier::SharedTier) = not_own {
            Some(format!(
                "the project's {} is the global or scene directory, where the rule would \
                 allow the call in every project",
                paths.layout.project_dir_name()
            ))
        } else if let Some(NotItsOwnTier::Elsewhere(to)) = not_own {
            Some(format!(
                "the project's {} resolves outside it{}, where the rule would allow the call \
                 in whatever that directory belongs to",
                paths.layout.project_dir_name(),
                to.map(|d| format!(", to {}", d.display()))
                    .unwrap_or_default()
            ))
        } else if paths.project_untrusted {
            Some(
                "the project is not trusted, so its settings would not be read; trust it \
                 first (project.trust)"
                    .to_string(),
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
            layout: paths.layout.clone(),
        }
    }
}

/// The engine's sandbox settings from the `sandbox`, `files` and `web`
/// blocks, the one place that maps them to it. Every field of
/// [`SandboxConfig`] is named, so a setting added there does not compile
/// until it is placed here or set aside.
pub fn sandbox_settings(settings: &Settings, paths: &base::paths::ConfigPaths) -> SandboxSettings {
    let SandboxConfig {
        enabled,
        require,
        allow_read,
        deny_read,
        network,
        env: _,
        cache_dirs: _,
        reap_on_close: _,
        unattended_needs_network_policy,
    } = &settings.sandbox;
    let deny_read: Vec<_> = base::interface::exec::default_deny_read()
        .into_iter()
        .chain(deny_read.iter().cloned())
        .collect();
    SandboxSettings {
        allow_read: allow_read.clone(),
        deny_read: deny_read.clone(),
        network_mode: network.mode,
        allowed_domains: settings.web.allowed_domains.clone(),
        egress_proxy: network.proxy.as_ref().map(|setting| setting.0.clone()),
        deny_write: settings.files.deny_write.clone(),
        allow_write: settings.files.allow_write.clone(),
        // The instance's own root, so the profile denies writes to the
        // settings.json this session actually reads — not to whatever sits
        // under the invoking user's home.
        paths: std::sync::Arc::new(
            base::paths::SensitivePaths::for_paths(paths).with_read_denials(&deny_read, allow_read),
        ),
        require_enforcement: *require,
        unattended_allows_open_network: !*unattended_needs_network_policy,
        disabled: !*enabled,
        worktree_git: None,
    }
}

impl Settings {
    pub fn engine_budget(&self) -> EngineBudget {
        EngineBudget {
            compact_threshold: self.compaction.threshold_tokens,
            compact_keep_recent: self.compaction.keep_recent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            "prompt": {"path_style": "relative"},
        }))
        .unwrap();
        assert_eq!(config_of(&s).path_style, PathStyle::Relative);
    }

    #[test]
    fn the_delegation_depth_is_read_from_settings() {
        let mut s = Settings::defaults_for("test-model");
        assert_eq!(config_of(&s).max_agent_depth, 3);
        s.agents.max_depth = 1;
        assert_eq!(config_of(&s).max_agent_depth, 1);
    }

    fn settings_with_sandbox(f: impl FnOnce(&mut Settings)) -> Settings {
        let mut s = Settings::defaults_for("test-model");
        f(&mut s);
        s
    }

    /// The whole point of `settings.sandbox` is that a user can configure it.
    /// Each field below reaches `EngineConfig::sandbox`, which every tool
    /// reads and every `SandboxPolicy` is derived from — a gap anywhere on
    /// that chain makes the setting inert while still appearing in the schema.
    #[test]
    fn every_sandbox_setting_reaches_the_engine_config() {
        let s = settings_with_sandbox(|s| {
            s.sandbox.network.mode = NetworkMode::Allowlist;
            s.web.allowed_domains = vec!["api.example.com".into()];
            s.sandbox.network.proxy =
                serde_json::from_value(serde_json::json!("127.0.0.1:3128")).unwrap();
            s.sandbox.allow_read = vec![PathBuf::from("/tmp/ok")];
            s.sandbox.deny_read = vec![PathBuf::from("/tmp/secret")];
            s.files.deny_write = vec![".secret".into()];
            s.files.allow_write = vec![PathBuf::from("/tmp/w")];
            s.sandbox.require = true;
            s.sandbox.unattended_needs_network_policy = false;
            s.sandbox.enabled = false;
        });
        let c = config_of(&s);

        assert_eq!(c.sandbox.network_mode, NetworkMode::Allowlist);
        assert_eq!(c.sandbox.allowed_domains, ["api.example.com"]);
        assert_eq!(
            c.sandbox.policy().egress_proxy.map(|p| p.http),
            Some("127.0.0.1:3128".parse().unwrap())
        );
        assert_eq!(c.sandbox.allow_read, [PathBuf::from("/tmp/ok")]);
        let with_secret: Vec<_> = base::interface::exec::default_deny_read()
            .into_iter()
            .chain([PathBuf::from("/tmp/secret")])
            .collect();
        assert_eq!(c.sandbox.deny_read, with_secret);
        assert_eq!(c.sandbox.policy().deny_read, with_secret);
        assert_eq!(c.sandbox.deny_write, [".secret"]);
        assert_eq!(c.sandbox.allow_write, [PathBuf::from("/tmp/w")]);
        assert!(c.sandbox.require_enforcement);
        assert!(c.sandbox.unattended_allows_open_network);
        assert!(c.sandbox.disabled);
    }

    /// An unconfigured `deny_read` means the built-in credential list, never
    /// "deny exactly nothing" — which would make `~/.ssh` and `~/.aws`
    /// readable by any command the classifier allows; a configured one adds
    /// to that list rather than replacing it.
    #[test]
    fn deny_read_adds_to_the_built_in_credential_list() {
        let defaults = base::interface::exec::default_deny_read();
        let c = config_of(&settings_with_sandbox(|s| {
            s.sandbox.deny_read = Vec::new();
        }));
        assert_eq!(c.sandbox.policy().deny_read, defaults);

        let c = config_of(&settings_with_sandbox(|s| {
            s.sandbox.deny_read = vec![PathBuf::from("/srv/secret")];
        }));
        let denied = c.sandbox.policy().deny_read;
        assert!(defaults.iter().all(|d| denied.contains(d)), "{denied:?}");
        assert!(denied.contains(&PathBuf::from("/srv/secret")));
    }

    /// The file tools refuse what `deny_read` names too, not only the
    /// built-in credential list.
    #[test]
    fn a_configured_deny_read_is_a_credential_to_the_file_tools() {
        let c = config_of(&settings_with_sandbox(|s| {
            s.sandbox.deny_read = vec![PathBuf::from("/tmp/agix-deny-read-case")];
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

    #[test]
    fn the_layout_reaches_the_engine_the_gate_and_the_sandbox() {
        let mut paths = PathSettings::default();
        paths.layout = base::paths::Layout::new(".codix").unwrap();
        let loaded = LoadedSettings::new(Settings::defaults_for("test-model"), paths);
        let c = loaded.engine_config();
        assert_eq!(c.layout.project_dir_name(), ".codix");
        assert_eq!(c.sandbox.paths.layout(), &c.layout);
        assert_eq!(c.sandbox.policy().layout, c.layout);
        assert_eq!(loaded.permission_settings().layout, c.layout);
    }

    /// The setting file says `"network": {"mode": "deny_all"}`; that is what
    /// the engine type reads and writes.
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
