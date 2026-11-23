//! The `permissions` block: modes, old keys that are no longer read, and the managed-only bypass switch.

use super::*;

#[test]
fn permission_mode_deserializes_bubble_and_dontask() {
    let b: PermissionMode = serde_json::from_str("\"bubble\"").unwrap();
    assert_eq!(b, PermissionMode::Bubble);
    assert!(!b.is_user_settable(), "Bubble is program-only");

    let d: PermissionMode = serde_json::from_str("\"dontAsk\"").unwrap();
    assert_eq!(d, PermissionMode::DontAsk);
    assert!(d.is_user_settable(), "DontAsk is user-settable");

    let mut s = Settings::defaults_for("test");
    s.permissions.mode = PermissionMode::Bubble;
    assert!(s.validate().is_err());
}

#[test]
fn a_delegate_runs_in_its_own_mode_unless_its_parent_holds_or_carries_it() {
    use PermissionMode::*;
    let all = [
        Default,
        AcceptEdits,
        BypassPermissions,
        Plan,
        DontAsk,
        Bubble,
    ];
    for parent in all {
        for delegate in all {
            let expected = match parent {
                Plan | DontAsk => delegate.min_permissive(parent),
                BypassPermissions => BypassPermissions,
                Default | AcceptEdits | Bubble => delegate,
            };
            assert_eq!(
                parent.for_delegate(delegate),
                expected,
                "parent {parent:?}, delegate {delegate:?}"
            );
        }
    }
    assert_eq!(DontAsk.for_delegate(BypassPermissions), DontAsk);
    assert_eq!(DontAsk.for_delegate(AcceptEdits), DontAsk);
    assert_eq!(DontAsk.for_delegate(Default), DontAsk);
    assert_eq!(Plan.for_delegate(BypassPermissions), Plan);
    assert_eq!(Default.for_delegate(BypassPermissions), BypassPermissions);
}

#[test]
fn auto_and_yolo_are_no_modes() {
    for spelling in ["auto", "yolo"] {
        let settings = load_global(&format!(
            r#"{{"permissions": {{"mode": "{spelling}"}}, "prompt": {{"language": "x"}}}}"#
        ));
        assert_eq!(ignored_keys(&settings), ["permissions.mode"]);
        assert_eq!(settings.values.permissions.mode, PermissionMode::Default);
        assert_eq!(settings.values.prompt.language.as_deref(), Some("x"));
    }
}

#[test]
fn the_permissions_block_reads_every_key() {
    let settings = load_global(
        r#"{"permissions": {
            "mode": "acceptEdits",
            "allow": ["Bash(git status:*)"],
            "allow_exact": ["Bash(rm a*.tmp)"],
            "ask": ["Bash(git push:*)"],
            "deny": ["Read(~/.env)"],
            "prompt_timeout_secs": 30,
            "client_may_loosen": true
        }}"#,
    );
    assert!(settings.ignored.is_empty(), "{:?}", settings.ignored);
    let permissions = &settings.values.permissions;
    assert_eq!(permissions.mode, PermissionMode::AcceptEdits);
    assert_eq!(permissions.prompt_timeout_secs, 30);
    assert!(permissions.client_may_loosen);
    assert!(permissions.allow.is_empty() && permissions.deny.is_empty());
    let rules: Vec<_> = settings
        .rules
        .iter()
        .map(|r| {
            (
                r.rule.tool.as_str(),
                r.rule.action,
                r.rule.matching.is_pattern(),
            )
        })
        .collect();
    assert_eq!(
        rules,
        [
            ("Bash(git status:*)", RuleBehavior::Allow, true),
            ("Bash(rm a*.tmp)", RuleBehavior::Allow, false),
            ("Bash(git push:*)", RuleBehavior::Ask, true),
            ("Read(~/.env)", RuleBehavior::Deny, true),
        ]
    );
    assert_eq!(settings.engine_config().permission_prompt_timeout_secs, 30);
}

/// No old key is read; the report of each names the key that replaced it.
#[test]
fn an_old_permission_key_is_not_read_and_names_its_new_key() {
    let settings = load_global(
        r#"{
            "permission_mode": "plan",
            "permission_rules": [{"tool": "Bash", "action": "allow"}],
            "allow_client_permission_override": true,
            "execution": {"permission_prompt_timeout_secs": 1}
        }"#,
    );
    assert_eq!(settings.values.permissions, PermissionsSettings::default());
    assert!(settings.rules.is_empty());
    assert!(settings.ignored.is_empty(), "{:?}", settings.ignored);
    for old in [
        "permission_mode",
        "permission_rules",
        "allow_client_permission_override",
    ] {
        assert!(crate::renamed::renamed_to(old).is_some(), "{old}");
    }
    assert_eq!(
        crate::renamed::renamed_to("execution.permission_prompt_timeout_secs"),
        Some("permissions.prompt_timeout_secs")
    );
}

/// Every old key of the blocks this layout replaced is reported with its
/// new name, and none of them is read.
#[test]
fn an_old_key_of_any_block_is_not_read_and_names_its_new_key() {
    let file = serde_json::json!({
        "sandbox": {
            "additional_directories": ["/x"], "read_directories": ["/y"], "read_anywhere": true,
            "allow_write": ["/w"], "deny_write": [".d"], "dangerously_disable_sandbox": true,
            "require_enforcement": true, "network_mode": "deny_all", "egress_proxy": "127.0.0.1:1",
            "subprocess_env": {"mode": "allowlist"}, "cache_directories": {},
            "reap_process_groups": true, "unattended_requires_network_policy": true,
            "allowed_domains": ["a.example"],
        },
        "network": {"use_system_proxy": false},
        "project_trust": {"trusted": ["/**"], "default": "trusted"},
        "history": {"location": "project"},
        "worktree_dir": ".trees",
        "execution": {"max_api_calls_per_turn": 1, "max_budget_tokens": 1, "max_parallelism": 1,
                      "max_agent_depth": 1, "max_concurrent_subagents": 1},
        "compaction": {"cached_microcompact": true, "tool_result_inline_chars": 1},
        "prompt_append": "a", "prompt_override": "o", "instruction_file": "/i", "language": "fr",
        "path_style": "workspace_relative", "hooks_config": {}, "mcp_servers": {},
        "mcp_enabled": {}, "disable_skill_shell_execution": true, "memory_enabled": false,
        "telemetry_enabled": false, "telemetry_url": "u",
    });
    let settings = load_global(&file.to_string());
    let defaults = Settings::defaults_for("m");
    let read = serde_json::to_value(&settings.values).unwrap();
    let mut expected = serde_json::to_value(&defaults).unwrap();
    expected["model"] = read["model"].clone();
    assert_eq!(read, expected);
    let old = crate::renamed::old_keys_in(&file);
    assert_eq!(old.len(), 38, "{old:?}");
    assert!(old
        .iter()
        .all(|(key, _)| *key != "execution" && *key != "network"));
}

/// `disable_bypass` is the managed layer's; a user tier that writes it has
/// written nothing.
#[test]
fn only_the_managed_layer_disables_bypass() {
    let settings =
        load_global(r#"{"permissions": {"mode": "bypassPermissions", "disable_bypass": true}}"#);
    assert_eq!(ignored_keys(&settings), ["permissions.disable_bypass"]);
    assert_eq!(
        settings.values.permissions.mode,
        PermissionMode::BypassPermissions
    );
    assert!(!settings.bypass_disabled());
}
