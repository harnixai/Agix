//! 跑一轮与上下文方法（`session.run_turn` / `compact` / `clear` /
//! `respondToPrompt` / `respondToolCall` / `inspectContext` / `dropMessages` /
//! `editMessage` / `rewind`）的参数与线上回执。

use super::lenient_vec;
use crate::rpc::RawOptions;
use crate::session_pool::api::{AskKind, TurnOutcome};
use crate::session_pool::OnBusy;
use runtime::agent::{Attachment, PermissionDecision, QuestionAnswer};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RunTurnParams {
    /// Omitted: a new session.
    #[serde(default)]
    pub session_id: Option<String>,
    pub message: String,
    /// Omitted: the daemon makes one up.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// An entry that does not read is dropped, not the turn.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub options: Option<RawOptions>,
    #[serde(default)]
    pub on_busy: Option<OnBusy>,
}

/// How a turn went, as `session.run_turn`, `session.compact` and
/// `session.clear` answer it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum TurnReply {
    Turn {
        session_id: String,
        turn_id: String,
        delivery: Delivery,
        /// The session's name after the turn.
        name: Option<String>,
        api_calls: u32,
    },
    Steer {
        session_id: String,
        turn_id: String,
        delivery: Delivery,
        into_turn_id: Option<String>,
    },
    Skipped {
        session_id: String,
        turn_id: String,
        skipped: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// The message ran a turn of its own.
    Turn,
    /// The message went to the turn already running.
    Steer,
}

impl From<TurnOutcome> for TurnReply {
    fn from(outcome: TurnOutcome) -> Self {
        match outcome {
            TurnOutcome::Completed {
                session_id,
                turn_id,
                name,
                api_calls,
            } => Self::Turn {
                session_id,
                turn_id,
                delivery: Delivery::Turn,
                name,
                api_calls,
            },
            TurnOutcome::Steered {
                session_id,
                turn_id,
                into_turn_id,
            } => Self::Steer {
                session_id,
                turn_id,
                delivery: Delivery::Steer,
                into_turn_id,
            },
            TurnOutcome::Skipped {
                session_id,
                turn_id,
            } => Self::Skipped {
                session_id,
                turn_id,
                skipped: true,
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RespondToPromptParams {
    pub session_id: String,
    pub prompt_id: String,
    /// The `prompt_type` of the frame answered; omitted: `permission`.
    #[serde(default)]
    pub prompt_type: Option<AskKind>,
    /// The answer to a `permission` prompt.
    #[serde(default)]
    pub decision: Option<PermissionDecision>,
    /// The answer to a `question`.
    #[serde(default)]
    pub answer: Option<QuestionAnswer>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PromptAnswered {
    pub prompt_id: String,
}

/// Exactly one of `result` and `error`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RespondToolCallParams {
    pub session_id: String,
    pub call_id: String,
    /// The host ran the call; `is_error` when it reports a failure.
    #[serde(default)]
    pub result: Option<HostToolResult>,
    /// The host could not run the call.
    #[serde(default)]
    pub error: Option<HostToolError>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HostToolResult {
    pub content: HostToolContent,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum HostToolContent {
    Text(String),
    Blocks(Vec<HostContentBlock>),
}

/// The `text` and `image` blocks of §4.1.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostContentBlock {
    Text { text: String },
    Image { source: ImageSource },
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ImageSource {
    /// Always `base64`.
    #[serde(rename = "type")]
    pub kind: String,
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HostToolError {
    pub message: String,
}

impl RespondToolCallParams {
    /// The answer the engine takes, or why there is none.
    pub fn reply(&self) -> Result<base::interface::host_call::HostReply, String> {
        use base::interface::host_call::HostReply;
        use base::tool::{ToolResult, ToolResultBlock};
        match (&self.result, &self.error) {
            (Some(result), None) => {
                let answer = match &result.content {
                    HostToolContent::Text(text) => ToolResult::text(text.clone()),
                    HostToolContent::Blocks(blocks) => {
                        let mut out = Vec::with_capacity(blocks.len());
                        for block in blocks {
                            out.push(match block {
                                HostContentBlock::Text { text } => ToolResultBlock::text(text),
                                HostContentBlock::Image { source } if source.kind == "base64" => {
                                    ToolResultBlock::image(&source.media_type, &source.data)
                                }
                                HostContentBlock::Image { .. } => {
                                    return Err("an image's source must be `base64`".into())
                                }
                            });
                        }
                        ToolResult::blocks(out)
                    }
                };
                Ok(HostReply::Result(answer.with_error(result.is_error)))
            }
            (None, Some(error)) => Ok(HostReply::Failed(error.message.clone())),
            _ => Err("give exactly one of `result` and `error`".into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ToolCallAnswered {
    pub call_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DropMessagesParams {
    pub session_id: String,
    /// Positions, in `session.history`'s coordinates.
    pub at: Vec<usize>,
    /// How many messages the caller saw; refused when the context has since
    /// changed.
    #[serde(default)]
    pub total: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Dropped {
    pub dropped: Vec<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EditMessageParams {
    pub session_id: String,
    pub at: usize,
    pub text: String,
    #[serde(default)]
    pub total: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Edited {
    pub edited: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RewindParams {
    pub session_id: String,
    #[serde(flatten)]
    pub cut: crate::session_pool::api::RewindCut,
    pub scope: crate::session_pool::api::RewindScope,
    /// Overwrite files changed since the session last wrote them.
    #[serde(default)]
    pub force: bool,
    /// Leave out files that cannot be restored instead of refusing.
    #[serde(default)]
    pub allow_partial: bool,
    /// Work out the plan and its problems; change nothing.
    #[serde(default)]
    pub dry_run: bool,
    /// Fork the whole session first, so the rewound part survives there.
    #[serde(default)]
    pub fork_first: bool,
}
