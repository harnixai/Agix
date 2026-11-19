//! `redact_tests` 原样搬来的用例。

use super::{redact_secret, redact_settings_secrets};

#[test]
fn mcp_headers_and_env_are_redacted_whatever_their_keys() {
    let mut settings = serde_json::json!({
        "mcp": {"servers": {
            "remote": {"type": "http", "url": "https://x",
                       "headers": {"Authorization": "Bearer sk-live-9999", "X-Trace": "on-1234"}},
            "local": {"type": "stdio", "command": "srv", "env": {"GITHUB_PAT": "ghp_abcd5678"}}
        }},
        "providers": {"p": {"API_KEY": "sk-upper-4321", "Client_Secret": "cs-0000"}},
        "model": {"order": ["keep-me"]}
    });
    redact_settings_secrets(&mut settings);
    let dumped = settings.to_string();
    for leaked in ["sk-live", "on-1", "ghp_", "sk-upper", "cs-"] {
        assert!(!dumped.contains(leaked), "{leaked} in {dumped}");
    }
    assert_eq!(settings["model"]["order"][0], "keep-me");
    assert_eq!(settings["mcp"]["servers"]["remote"]["url"], "https://x");
    assert_eq!(settings["mcp"]["servers"]["local"]["command"], "srv");
}

#[test]
fn redacts_long_secret_keeping_last_four() {
    assert_eq!(redact_secret("sk-ant-abcdef1234"), "***1234");
}

#[test]
fn short_secret_fully_redacted() {
    assert_eq!(redact_secret("abcd"), "***");
    assert_eq!(redact_secret(""), "***");
}

#[test]
fn handles_multibyte_chars_without_panicking() {
    // 4-byte-per-char emoji tail — a byte-slice `&s[len-4..]` would
    // panic here; char-based slicing must not.
    let s = "sk-😀😀😀😀";
    assert_eq!(redact_secret(s), "***😀😀😀😀");
}

#[test]
fn credentials_inside_urls_and_flag_arguments_are_redacted() {
    let mut settings = serde_json::json!({
        "mcp": {"servers": {
            "remote": {"type": "http", "url": "https://x.test/mcp?token=queryleak9&v=2"},
            "local": {"type": "stdio", "command": "srv",
                      "args": ["--api-key", "argleak777", "--port", "80", "--secret=eqleak55"]}
        }},
        "hooks": {"PreToolUse": [{"type": "http", "url": "https://h.test/?key=hookleak3"}]}
    });
    redact_settings_secrets(&mut settings);
    let dumped = settings.to_string();
    for leaked in ["queryleak9", "argleak777", "eqleak55", "hookleak3"] {
        assert!(!dumped.contains(leaked), "{leaked} in {dumped}");
    }
    assert!(
        dumped.contains("v=2") && dumped.contains("\"80\""),
        "{dumped}"
    );
}

#[test]
fn every_setting_kept_out_of_settings_json_is_redacted_when_shown() {
    for pattern in settings::edit::SECRET_PATHS {
        let segments: Vec<&str> = pattern
            .split('.')
            .map(|s| if s == "*" { "any" } else { s })
            .collect();
        let leaf = match segments.last() {
            Some(&"headers") | Some(&"env") => serde_json::json!({"K": "plain-secret-value"}),
            _ => serde_json::json!("plain-secret-value"),
        };
        let mut shown = segments
            .iter()
            .rev()
            .fold(leaf, |inner, key| serde_json::json!({ *key: inner }));
        redact_settings_secrets(&mut shown);
        assert!(
            !shown.to_string().contains("plain-secret-value"),
            "`{pattern}` is shown in the clear: {shown}"
        );
    }
}

#[test]
fn a_password_in_a_urls_userinfo_is_redacted_and_the_rest_kept() {
    let mut settings = serde_json::json!({
        "telemetry": {"otlp_endpoint": "https://agent:hunter2@collector.example/v1?x=1"},
        "sandbox": {"network": {"proxy": "http://u:pw-1234@proxy:3128"}},
        "plain": "https://user@host/path",
        "args": ["--url=https://a:b-secret@h/p"]
    });
    redact_settings_secrets(&mut settings);
    assert_eq!(
        settings["telemetry"]["otlp_endpoint"],
        "https://agent:***@collector.example/v1?x=1"
    );
    assert_eq!(
        settings["sandbox"]["network"]["proxy"],
        "http://u:***@proxy:3128"
    );
    assert_eq!(settings["plain"], "https://user@host/path");
    assert!(!settings.to_string().contains("b-secret"), "{settings}");
}

#[test]
fn the_sandbox_env_names_variables_and_is_shown_as_it_is() {
    let mut settings = serde_json::json!({
        "sandbox": {"env": {"mode": "allowlist", "allow": ["AWS_REGION"]}},
        "mcp": {"servers": {"s": {"command": "srv", "env": {"mode": "secret-1234"}}}}
    });
    redact_settings_secrets(&mut settings);
    assert_eq!(settings["sandbox"]["env"]["mode"], "allowlist");
    assert_eq!(
        settings["sandbox"]["env"]["allow"],
        serde_json::json!(["AWS_REGION"])
    );
    assert_ne!(
        settings["mcp"]["servers"]["s"]["env"]["mode"],
        "secret-1234"
    );
}
