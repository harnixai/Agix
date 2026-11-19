//! 改过名的设置键：旧键 → 新键。旧键不再读，读到时只用这张表在告警里报出新名字，
//! 不做迁移。

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
        "execution.permission_prompt_timeout_secs",
        "permissions.prompt_timeout_secs",
    ),
];

/// What replaced `key`, when it is an old key. `key` may carry a list index
/// (`permission_rules[0]`).
pub fn renamed_to(key: &str) -> Option<&'static str> {
    let key = key.split('[').next().unwrap_or(key);
    RENAMED_KEYS
        .iter()
        .find(|(old, _)| *old == key)
        .map(|(_, new)| *new)
}

/// `error` about `key`, with the new name when `key` is an old one.
pub fn explain(key: &str, error: String) -> String {
    match renamed_to(key) {
        Some(new) => format!("renamed to `{new}`; `{key}` is no longer read"),
        None => error,
    }
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
        assert_eq!(renamed_to("permissions.mode"), None);
        assert!(explain("permission_mode", "x".into()).contains("permissions.mode"));
        assert_eq!(explain("language", "x".into()), "x");
    }
}
