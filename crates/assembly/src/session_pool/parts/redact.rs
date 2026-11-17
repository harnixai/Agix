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
pub(in crate::session_pool) fn redact_settings_secrets(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                let key = k.to_ascii_lowercase();
                if matches!(key.as_str(), "headers" | "env") && v.is_object() {
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
                    other => redact_settings_secrets(other),
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
                        redact_settings_secrets(other);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Whether `value`, written under `key`, carries a credential the redaction
/// would mask: a secret-named key, a password or token in a URL, a token
/// after a `--token`-like argument.
pub(in crate::session_pool) fn holds_credential(key: &str, value: &serde_json::Value) -> bool {
    let original = serde_json::Value::Object(serde_json::Map::from_iter([(
        key.to_string(),
        value.clone(),
    )]));
    let mut shown = original.clone();
    redact_settings_secrets(&mut shown);
    shown != original
}

/// [`base::text::scrub_secrets`], and the password of each URL's userinfo
/// (`https://user:***@host`).
pub(in crate::session_pool) fn scrub_url(text: &str) -> String {
    let scrubbed = base::text::scrub_secrets(text);
    let mut out = String::with_capacity(scrubbed.len());
    let mut rest = scrubbed.as_str();
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
            .unwrap_or(tail.len());
        let (authority, after) = tail.split_at(end);
        match authority
            .rsplit_once('@')
            .and_then(|(userinfo, host)| Some((userinfo.split_once(':')?.0, host)))
        {
            Some((user, host)) => out.push_str(&format!("{user}:***@{host}")),
            None => out.push_str(authority),
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

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
