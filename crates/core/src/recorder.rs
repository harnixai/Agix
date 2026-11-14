//! Record/replay configuration: the settings a model wrapper reads to record
//! a session, replay it, or re-issue its requests.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Recorder (record/replay) configuration. `None` on [`Settings`] leaves the
/// model wrapper a pass-through.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecorderConfig {
    pub mode: RecorderMode,
    /// Directory name for this recording under `root`. `None` uses the session
    /// id, which is what production recording wants; tests name theirs so a
    /// replay can find it again.
    #[serde(default)]
    pub name: Option<String>,
    /// Where recordings live. Each one is a self-contained directory beneath
    /// it, so a host retires a recording by deleting that directory.
    pub root: PathBuf,
    #[serde(default)]
    pub on_divergence: Divergence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecorderMode {
    Record,
    Replay,
    /// Re-issue a recording's requests against a real provider.
    ///
    /// The third mode exists because the other two cannot answer "what would
    /// the model have said if the prompt were different". `Replay` never reads
    /// a request's content — it compares blob *ids* and returns the recorded
    /// response — so editing a blob changes nothing it observes. `Rerun` loads
    /// the request back from the blobs (picking up any edit), sends it, and
    /// reports how the new response differs from the recorded one.
    Rerun,
}

/// What replay does when a live request differs from the one recorded at the
/// same position.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Divergence {
    /// Fail the call, naming the fields that differ. The default wherever a
    /// divergence means a broken test.
    #[default]
    Strict,
    /// Report the difference and play the recorded response anyway.
    Warn,
}
