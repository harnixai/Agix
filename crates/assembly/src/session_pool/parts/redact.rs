//! settings 内容的脱敏：按键名遮住凭据，只留末四位。

/// Redact every credential-shaped field in a settings blob.
///
/// Walks by key name rather than by type, because the same secrets appear at
/// several depths (`providers.<id>.api_key`, an MCP server's `headers`) and a
/// settings shape that grows a new provider field should not silently start
/// returning it in the clear. Key names match case-insensitively, and every
/// string under a `headers` or `env` object is redacted whatever its key: an
/// MCP server's `Authorization` header and the variables a stdio server is
/// started with are credentials as often as not, under names nobody can list.
/// `sandbox.env` is not one of them: it names variables, it holds no values.
pub(in crate::session_pool) fn redact_settings_secrets(value: &mut serde_json::Value) {
    redact_beneath(value, None);
}

fn redact_beneath(value: &mut serde_json::Value, parent: Option<&str>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                let key = k.to_ascii_lowercase();
                let sandbox_env = key == "env" && parent == Some("sandbox");
                if matches!(key.as_str(), "headers" | "env") && v.is_object() && !sandbox_env {
                    redact_every_string(v);
                    continue;
                }
                match v {
                    serde_json::Value::String(s) if is_secret_key(&key) => {
                        if !s.is_empty() {
                            *v = serde_json::Value::String(redact_secret(s));
                        }
                    }
                    serde_json::Value::String(s) if s.contains("://") => {
                        *s = scrub_url(s);
                    }
                    other => redact_beneath(other, Some(&key)),
                }
            }
        }
        serde_json::Value::Array(items) => {
            let mut secret_follows = false;
            for item in items {
                match item {
                    serde_json::Value::String(s) if secret_follows => {
                        *s = redact_secret(s);
                        secret_follows = false;
                    }
                    serde_json::Value::String(s) if s.starts_with("--") => {
                        secret_follows =
                            !s.contains('=') && names_a_secret(&s.to_ascii_lowercase());
                        if s.contains('=') || s.contains("://") {
                            *s = scrub_url(s);
                        }
                    }
                    other => {
                        secret_follows = false;
                        redact_beneath(other, None);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Whether `value`, written at `path`, carries a credential the redaction
/// would mask: a secret-named key, a password or token in a URL, a token
/// after a `--token`-like argument.
pub(in crate::session_pool) fn holds_credential(path: &[&str], value: &serde_json::Value) -> bool {
    let original = path.iter().rev().fold(value.clone(), |inner, key| {
        serde_json::Value::Object(serde_json::Map::from_iter([(key.to_string(), inner)]))
    });
    let mut shown = original.clone();
    redact_settings_secrets(&mut shown);
    shown != original
}

pub(in crate::session_pool) use base::text::scrub_url;

fn names_a_secret(lowercase_flag: &str) -> bool {
    ["token", "key", "secret", "passw", "auth"]
        .iter()
        .any(|n| lowercase_flag.contains(n))
}

pub(in crate::session_pool) fn is_secret_key(lowercase_key: &str) -> bool {
    const SECRET_KEYS: &[&str] = &[
        "api_key",
        "auth_token",
        "token",
        "secret",
        "password",
        "authorization",
    ];
    SECRET_KEYS.contains(&lowercase_key)
        || ["_token", "_secret", "_password"]
            .iter()
            .any(|suffix| lowercase_key.ends_with(suffix))
}

pub(in crate::session_pool) fn redact_every_string(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => {
            *value = serde_json::Value::String(redact_secret(s));
        }
        serde_json::Value::Object(map) => map.values_mut().for_each(redact_every_string),
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_every_string),
        _ => {}
    }
}

/// Redact a secret to `***<last 4 chars>` (char-boundary safe — slices by
/// `char`, not byte, so this never panics on non-ASCII input); too-short
/// secrets (≤4 chars) redact to a bare `***` rather than revealing the
/// whole thing.
pub(in crate::session_pool) fn redact_secret(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 4 {
        return "***".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("***{tail}")
}
