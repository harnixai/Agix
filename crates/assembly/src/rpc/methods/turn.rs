//! 跑一轮与上下文方法（`session.run_turn` / `compact` / `clear` /
//! `respondToPrompt` / `respondToolCall` / `inspectContext` / `dropMessages` /
//! `editMessage` / `rewind`）的参数与线上回执。

use super::lenient_vec;
use crate::rpc::RawOptions;
use crate::session_pool::api::{AskKind, TurnOutcome};
use crate::session_pool::OnBusy;
use runtime::agent::{Attachment, PermissionDecision};
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
    /// The answer to a `question` or an `mcp_elicitation`, read as the one
    /// `prompt_type` names.
    #[serde(default)]
    #[schemars(with = "Option<AnswerShape>")]
    pub answer: Option<serde_json::Value>,
}

#[derive(JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum AnswerShape {
    Question(runtime::agent::QuestionAnswer),
    McpElicitation(base::interface::elicitation::McpElicitationAnswer),
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

impl ImageSource {
    fn check(&self) -> Result<(), String> {
        const TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
        if !TYPES.contains(&self.media_type.as_str()) {
            return Err(format!(
                "an image's media_type must be one of {}, not `{}`",
                TYPES.join(", "),
                self.media_type
            ));
        }
        let bytes = self.data.len() / 4 * 3;
        if bytes as u64 > base::tool::MAX_IMAGE_BYTES {
            return Err(format!(
                "an image of about {bytes} bytes is over the {}-byte limit for one image",
                base::tool::MAX_IMAGE_BYTES
            ));
        }
        Ok(())
    }
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
                                    source.check()?;
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

/// The answer to a `hook_call` frame: what a hook response may say, but
/// approving, rewriting the input, ending the turn or asking to be run again.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RespondHookParams {
    /// The session the frame came in, a sub-agent's call's parent included.
    pub session_id: String,
    pub call_id: String,
    /// `block` or `proceed`; omitted: `proceed`.
    #[serde(default)]
    pub decision: Option<String>,
    /// With `block`: what the model reads, after the hook's prefix
    /// (`Blocked by PreToolUse hook: `).
    #[serde(default)]
    pub message: Option<String>,
    /// For the events that take it: text put in front of the model at its
    /// next step.
    #[serde(default)]
    pub additional_context: Option<String>,
}

impl RespondHookParams {
    pub fn answer(&self) -> Result<crate::session_pool::api::HookAnswer, String> {
        let block = match self.decision.as_deref() {
            None | Some("proceed") => false,
            Some("block") => true,
            Some("approve") => {
                return Err(
                    "a host hook cannot approve a call: it may block it or let it \
                            proceed to the permission check"
                        .into(),
                )
            }
            Some(other) => return Err(format!("decision `{other}` is not `block` or `proceed`")),
        };
        Ok(crate::session_pool::api::HookAnswer {
            block,
            message: self.message.clone(),
            additional_context: self.additional_context.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HookAnswered {
    pub call_id: String,
}

/// One progress report on an open host tool call; the keys of the
/// `tool_progress` frame it becomes.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportToolProgressParams {
    pub session_id: String,
    pub call_id: String,
    pub message: String,
    #[serde(default)]
    pub progress: Option<f64>,
    #[serde(default)]
    pub total: Option<f64>,
}

impl ReportToolProgressParams {
    pub fn update(&self) -> base::tool::ProgressUpdate {
        let mut update = base::tool::ProgressUpdate::text(&self.message);
        if let Some(progress) = self.progress {
            update = update.with_progress(progress);
        }
        if let Some(total) = self.total {
            update = update.with_total(total);
        }
        update
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProgressReported {
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
pub struct CompactParams {
    pub session_id: String,
    /// What the summary should keep or stress; at most 4000 characters.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Summarize up to here and keep what follows as it is.
    #[serde(default)]
    pub at: Option<CompactAtParam>,
}

/// Where a compaction stops: exactly one of the two.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct CompactAtParam {
    /// Up to and including this turn.
    #[serde(default)]
    pub to_turn: Option<String>,
    /// Up to and including the round that holds this tool call and its result.
    #[serde(default)]
    pub to_tool_use: Option<String>,
}

pub const COMPACT_INSTRUCTIONS_MAX_CHARS: usize = 4000;

impl CompactParams {
    pub fn command(&self) -> Result<String, String> {
        let mut command = String::from("/compact");
        if let Some(at) = &self.at {
            let (kind, id) = match (&at.to_turn, &at.to_tool_use) {
                (Some(id), None) => ("turn", id),
                (None, Some(id)) => ("tool_use", id),
                _ => return Err("`at` takes exactly one of to_turn and to_tool_use".into()),
            };
            if id.is_empty() || id.contains(char::is_whitespace) {
                return Err("`at` names an id with no whitespace".into());
            }
            command.push_str(&format!(" --at {kind}:{id}"));
        }
        if let Some(instructions) = self.instructions.as_deref().map(str::trim) {
            if instructions.is_empty() {
                return Err("`instructions` is empty".into());
            }
            if instructions.chars().count() > COMPACT_INSTRUCTIONS_MAX_CHARS {
                return Err(format!(
                    "`instructions` is longer than {COMPACT_INSTRUCTIONS_MAX_CHARS} characters"
                ));
            }
            command.push(' ');
            command.push_str(instructions);
        }
        Ok(command)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(extra: serde_json::Value) -> Result<RespondHookParams, serde_json::Error> {
        let mut params = json!({"session_id": "s", "call_id": "hk_1"});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(params)
    }

    #[test]
    fn respond_hook_takes_block_proceed_and_context() {
        let block = parse(json!({"decision": "block", "message": "m"})).unwrap();
        let answer = block.answer().unwrap();
        assert!(answer.block);
        assert_eq!(answer.message.as_deref(), Some("m"));

        assert!(!parse(json!({})).unwrap().answer().unwrap().block);
        assert!(
            !parse(json!({"decision": "proceed", "additional_context": "c"}))
                .unwrap()
                .answer()
                .unwrap()
                .block
        );
    }

    #[test]
    fn respond_hook_refuses_approve_and_unknown_decisions() {
        assert!(parse(json!({"decision": "approve"}))
            .unwrap()
            .answer()
            .is_err());
        assert!(parse(json!({"decision": "maybe"}))
            .unwrap()
            .answer()
            .is_err());
    }

    #[test]
    fn respond_hook_refuses_fields_a_host_hook_may_not_use() {
        for field in ["updated_input", "continue", "rewake"] {
            assert!(parse(json!({ field: true })).is_err(), "{field}");
        }
    }
}
