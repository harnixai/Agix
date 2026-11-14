//! The secret scan a memory goes through before it is written.

use base::memory::DurableMemory;
use std::sync::OnceLock;

/// A detected secret pattern in a memory.
#[derive(Debug, Clone)]
pub struct SecretPattern {
    /// The kind of secret detected (e.g., "API key", "token").
    pub kind: &'static str,
    /// A truncated snippet of the matched content (first 20 chars).
    pub snippet: String,
    /// The name of the memory file containing the match.
    pub filename: Option<String>,
}

#[allow(clippy::unwrap_used)]
fn secret_patterns() -> &'static [(&'static str, regex::Regex)] {
    static PATTERNS: OnceLock<Vec<(&'static str, regex::Regex)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            // OpenAI / Anthropic / generic API keys starting with sk-
            (
                "API key (sk-...)",
                regex::Regex::new(r"(?i)\bsk-[A-Za-z0-9]{20,}\b").unwrap(),
            ),
            // Generic api_key / apikey assignment
            (
                "API key (generic)",
                regex::Regex::new(r#"(?i)(api[_-]?key|apikey)\s*[:=]\s*['"]?[A-Za-z0-9_\-]{16,}"#)
                    .unwrap(),
            ),
            // Bearer / auth tokens
            (
                "Bearer token",
                regex::Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9\-._~+/]{20,}").unwrap(),
            ),
            // GitHub tokens (ghp_, gho_, ghu_, ghs_, ghf_)
            (
                "GitHub token",
                regex::Regex::new(r"(?i)\bgh[pousf]_[A-Za-z0-9_]{30,}\b").unwrap(),
            ),
            // AWS access keys (AKIA...)
            (
                "AWS access key",
                regex::Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap(),
            ),
            // JWT tokens
            (
                "JWT token",
                regex::Regex::new(
                    r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b",
                )
                .unwrap(),
            ),
            // Generic token/password/secret variable assignment
            (
                "Token/secret variable",
                regex::Regex::new(
                    r#"(?i)(token|secret|password)\s*[:=]\s*['"][A-Za-z0-9_\-\.]{8,}['"]"#,
                )
                .unwrap(),
            ),
            // SSH private key marker
            (
                "SSH private key",
                regex::Regex::new(r"-----BEGIN\s+(RSA|DSA|EC|OPENSSH)\s+PRIVATE\s+KEY-----")
                    .unwrap(),
            ),
            // Slack tokens (xoxb-, xoxp-, xapp-)
            (
                "Slack token",
                regex::Regex::new(r"\b[xX][oO][xX][baprs]-[A-Za-z0-9_-]{20,}\b").unwrap(),
            ),
        ]
    })
}

/// Scan a single memory for sensitive data patterns.
///
/// Checks the name, description, and content fields against a set of regex
/// patterns for API keys, tokens, and other secrets.
///
/// Returns a list of [`SecretPattern`] matches. **Does not block** — warns and
/// proceeds.
pub fn scan_for_secrets(memory: &DurableMemory) -> Vec<SecretPattern> {
    let mut findings = Vec::new();

    let search_fields: Vec<(&str, &str)> = vec![
        ("name", &memory.name),
        ("description", &memory.description),
        ("content", &memory.content),
    ];

    let patterns = secret_patterns();

    for (_field_name, text) in &search_fields {
        if text.is_empty() {
            continue;
        }
        for (kind, re) in patterns {
            if re.is_match(text) {
                // Extract a short snippet for reporting
                let snippet = if let Some(cap) = re.find(text) {
                    let matched = cap.as_str();
                    if matched.len() > 40 {
                        // Character-boundary aware: `matched` is a regex hit
                        // against arbitrary conversation text, so byte 40 can
                        // land mid-character.
                        format!("{}...", base::text::truncate_at_char_boundary(matched, 40))
                    } else {
                        matched.to_string()
                    }
                } else {
                    continue;
                };

                findings.push(SecretPattern {
                    kind,
                    snippet,
                    filename: Some(memory.name.clone()),
                });
                // Report each pattern once per field per memory to avoid noise
                break;
            }
        }
    }

    findings
}
