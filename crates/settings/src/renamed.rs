//! 改过名的设置键：旧键 → 新键。旧键不再读，读到时只用这张表在告警里报出新名字，
//! 不做迁移。

use serde_json::Value;

/// Dotted old key → what replaced it.
pub const RENAMED_KEYS: &[(&str, &str)] = &[
    ("permission_mode", "permissions.mode"),
    (
        "permission_rules",
        "permissions.allow / permissions.allow_exact / permissions.ask / permissions.deny",
    ),
    (
        "allow_client_permission_override",
        "permissions.client_may_loosen",
    ),
    (
        "disable_bypass_permissions_mode",
        "permissions.disable_bypass",
    ),
    (
        "execution",
        "turn / agents / permissions.prompt_timeout_secs",
    ),
    (
        "execution.permission_prompt_timeout_secs",
        "permissions.prompt_timeout_secs",
    ),
    ("execution.max_api_calls_per_turn", "turn.max_model_calls"),
    ("execution.max_budget_tokens", "turn.token_budget"),
    ("execution.max_parallelism", "turn.parallel_tools"),
    ("execution.max_agent_depth", "agents.max_depth"),
    (
        "execution.max_concurrent_subagents",
        "agents.max_concurrent",
    ),
    ("sandbox.additional_directories", "files.extra_dirs"),
    ("sandbox.read_directories", "files.read_dirs"),
    (
        "sandbox.read_anywhere",
        "files.read_dirs (listing \"/\" reads anywhere)",
    ),
    ("sandbox.allow_write", "files.allow_write"),
    ("sandbox.deny_write", "files.deny_write"),
    (
        "sandbox.dangerously_disable_sandbox",
        "sandbox.enabled (the other way round: false turns the sandbox off)",
    ),
    ("sandbox.require_enforcement", "sandbox.require"),
    ("sandbox.network_mode", "sandbox.network.mode"),
    ("sandbox.egress_proxy", "sandbox.network.proxy"),
    ("sandbox.subprocess_env", "sandbox.env"),
    ("sandbox.cache_directories", "sandbox.cache_dirs"),
    ("sandbox.reap_process_groups", "sandbox.reap_on_close"),
    (
        "sandbox.unattended_requires_network_policy",
        "sandbox.unattended_needs_network_policy",
    ),
    ("sandbox.allowed_domains", "web.allowed_domains"),
    ("network", "app.system_proxy"),
    ("network.use_system_proxy", "app.system_proxy"),
    (
        "project_trust",
        "app.trusted_projects / app.trust_all_projects",
    ),
    ("project_trust.trusted", "app.trusted_projects"),
    (
        "project_trust.default",
        "app.trust_all_projects (true for `trusted`)",
    ),
    ("history", "app.history"),
    ("history.location", "app.history"),
    ("worktree_dir", "app.worktree_dir"),
    ("team.max_persistent_members", "team.max_members"),
    ("team.member_idle_timeout_secs", "team.member_idle_secs"),
    (
        "compaction.cached_microcompact",
        "compaction.clear_on_cache_expiry",
    ),
    (
        "compaction.tool_result_inline_chars",
        "compaction.inline_result_chars",
    ),
    ("prompt_append", "prompt.append"),
    ("prompt_override", "prompt.override"),
    ("instruction_file", "prompt.instruction_files (a list)"),
    ("language", "prompt.language"),
    (
        "path_style",
        "prompt.path_style (`workspace_relative` is now `relative`)",
    ),
    ("hooks_config", "hooks"),
    ("mcp_servers", "mcp.servers"),
    ("mcp_enabled", "mcp.servers.<name>.enabled"),
    (
        "disable_skill_shell_execution",
        "skills.allow_shell (the other way round: false turns it off)",
    ),
    ("memory_enabled", "memory.enabled"),
    ("telemetry_enabled", "telemetry.enabled"),
    ("telemetry_url", "telemetry.otlp_endpoint"),
];

/// What replaced `key`, when it is an old key or lies beneath one. `key` may
/// carry a list index (`permission_rules[0]`).
pub fn renamed_to(key: &str) -> Option<&'static str> {
    let key = key.split('[').next().unwrap_or(key);
    RENAMED_KEYS
        .iter()
        .filter(|(old, _)| key == *old || key.starts_with(&format!("{old}.")))
        .max_by_key(|(old, _)| old.len())
        .map(|(_, new)| *new)
}

/// `error` about `key`, with the new name when `key` is an old one.
pub fn explain(key: &str, error: String) -> String {
    match renamed_to(key) {
        Some(new) => format!("renamed to `{new}`; `{key}` is no longer read"),
        None => error,
    }
}

/// The old keys a settings layer holds, each with what replaced it; the most
/// specific one where both an old block and an old key in it are there.
pub fn old_keys_in(layer: &Value) -> Vec<(&'static str, &'static str)> {
    let present: Vec<_> = RENAMED_KEYS
        .iter()
        .filter(|(old, _)| {
            old.split('.')
                .try_fold(layer, |node, segment| node.get(segment))
                .is_some()
        })
        .copied()
        .collect();
    present
        .iter()
        .filter(|(old, _)| {
            !present
                .iter()
                .any(|(other, _)| other.starts_with(&format!("{old}.")))
        })
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_key_names_its_replacement() {
        assert_eq!(renamed_to("permission_mode"), Some("permissions.mode"));
        assert_eq!(
            renamed_to("execution.permission_prompt_timeout_secs"),
            Some("permissions.prompt_timeout_secs")
        );
        assert!(renamed_to("permission_rules[3]").is_some());
        assert_eq!(renamed_to("mcp_servers.github"), Some("mcp.servers"));
        assert_eq!(renamed_to("permissions.mode"), None);
        assert_eq!(renamed_to("prompt.language"), None);
        assert!(explain("permission_mode", "x".into()).contains("permissions.mode"));
        assert_eq!(explain("prompt.language", "x".into()), "x");
    }

    #[test]
    fn a_layer_reports_its_old_keys_most_specific_first() {
        let layer = serde_json::json!({
            "execution": {"max_parallelism": 2},
            "sandbox": {"network_mode": "deny_all", "allow_read": []},
            "language": "en",
            "prompt": {"language": "en"},
        });
        let mut found = old_keys_in(&layer);
        found.sort();
        assert_eq!(
            found,
            [
                ("execution.max_parallelism", "turn.parallel_tools"),
                ("language", "prompt.language"),
                ("sandbox.network_mode", "sandbox.network.mode"),
            ]
        );
    }
}
