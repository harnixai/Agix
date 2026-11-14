//! Durable memory data — what one memory is and how storing it can fail. The
//! store, the secret scan, the prompt and the selection call live in the
//! `memory` crate.

use serde::{Deserialize, Serialize};

/// A single durable memory entry — stored as a .md file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurableMemory {
    /// Short kebab-case slug (used as filename: `memory-dir/<slug>.md`).
    pub name: String,
    /// One-line summary — used to decide relevance during recall.
    pub description: String,
    /// Memory type.
    #[serde(default)]
    pub memory_type: MemoryType,
    /// The memory content (body after frontmatter).
    pub content: String,
    /// Session that produced this memory.
    #[serde(default)]
    pub source_session_id: String,
    /// Confidence score 0.0–1.0. Entries below 0.3 are not saved.
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    /// ISO-8601 timestamp of last write.
    #[serde(default)]
    pub last_seen: String,
    /// Number of times this memory has been surfaced to the model (recall count).
    #[serde(default)]
    pub recall_count: u32,
}

fn default_confidence() -> f64 {
    0.8
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MemoryType {
    /// Who the user is (role, expertise, preferences).
    #[default]
    User,
    /// Guidance the user has given on how you should work.
    Feedback,
    /// Ongoing work, goals, or constraints not derivable from code/git.
    Project,
    /// Pointers to external resources (URLs, dashboards, tickets).
    Reference,
}

impl MemoryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryType::User => "user",
            MemoryType::Feedback => "feedback",
            MemoryType::Project => "project",
            MemoryType::Reference => "reference",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MemoryError {
    #[error("IO error: {0}")]
    Io(String),
    #[error("serialization error: {0}")]
    Serde(String),
}
