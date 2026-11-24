//! 压缩首尾两帧（`CompactStarted` / `CompactEnded`）上的两个取值：为什么压、为什么没压成。

use serde::{Deserialize, Serialize};

/// What set a compaction off (`AgentEvent::CompactStarted`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompactCause {
    /// The context crossed the compaction threshold.
    Threshold,
    /// `/compact` or `session.compact`.
    Manual,
    /// The provider refused a request for its size; it is sent again after.
    Overflow,
}

/// Why a compaction left the history as it was (`AgentEvent::CompactEnded`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompactOutcome {
    /// A `PreCompact` hook declined it.
    Declined,
    NothingToCompact,
    /// The compactor or the summary failed.
    Failed,
    /// The turn was interrupted while it ran.
    Cancelled,
}
