//! `/compact` 的手动参数：给摘要的指令，和压到哪一点为止。

use crate::agent::Agent;
use base::interface::model::ModelContentBlock;

/// Where `/compact --at` cuts: the end of a turn, or the round holding a tool
/// call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompactAt {
    Turn(String),
    ToolUse(String),
}

/// The arguments of `/compact`: `[--at turn:<id>|tool_use:<id>] [instructions]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CompactArgs {
    pub(crate) at: Option<CompactAt>,
    pub(crate) instructions: Option<String>,
}

impl CompactArgs {
    pub(crate) fn parse(args: &str) -> Result<Self, String> {
        let args = args.trim();
        let (at, rest) = match args.strip_prefix("--at") {
            Some(rest) if rest.starts_with(char::is_whitespace) => {
                let rest = rest.trim_start();
                let (reference, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                let at = match reference.split_once(':') {
                    Some(("turn", id)) if !id.is_empty() => CompactAt::Turn(id.to_string()),
                    Some(("tool_use", id)) if !id.is_empty() => CompactAt::ToolUse(id.to_string()),
                    _ => {
                        return Err(format!(
                            "`--at` takes turn:<id> or tool_use:<id>, got `{reference}`"
                        ))
                    }
                };
                (Some(at), rest.trim())
            }
            _ => (None, args),
        };
        Ok(Self {
            at,
            instructions: (!rest.is_empty()).then(|| rest.to_string()),
        })
    }
}

impl Agent {
    /// How many leading messages a compaction up to `at` summarizes.
    pub(crate) async fn messages_through(&mut self, at: &CompactAt) -> Result<usize, String> {
        match at {
            CompactAt::ToolUse(id) => {
                let messages = self.session.messages();
                let call = messages
                    .iter()
                    .position(|m| {
                        m.content.iter().any(
                            |b| matches!(b, ModelContentBlock::ToolUse { id: u, .. } if u == id),
                        )
                    })
                    .ok_or_else(|| format!("the conversation has no tool call `{id}`"))?;
                let answered = messages[call + 1..].iter().position(|m| {
                    m.content.iter().any(|b| {
                        matches!(b, ModelContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id)
                    })
                });
                Ok(match answered {
                    Some(offset) => call + 1 + offset + 1,
                    None => call + 1,
                })
            }
            CompactAt::Turn(turn) => {
                let store = self
                    .session
                    .history_store()
                    .cloned()
                    .ok_or("this session keeps no log")?;
                self.flush_log().await?;
                let sid = base::session::SessionId::parse(&self.session.session_id)
                    .map_err(|e| e.to_string())?;
                let entries = store.load(sid).await.map_err(|e| e.to_string())?;
                let voided = history::transcript::voided(&entries);
                let keep = history::checkpoint::plan::cut(
                    &entries,
                    &voided,
                    &history::checkpoint::RewindTarget::ToTurn(turn.clone()),
                )
                .map_err(|e| e.to_string())?;
                let last = history::checkpoint::plan::cut_after(&entries, keep)
                    .ok_or("the turn has no messages")?;
                history::transcript::projected_message_count_through_entry_id(&entries, last)
                    .ok_or_else(|| "the turn is not in the log".to_string())
            }
        }
    }
}

#[cfg(test)]
mod args_tests {
    use super::*;

    #[test]
    fn args_split_into_a_point_and_instructions() {
        assert_eq!(CompactArgs::parse("").unwrap(), CompactArgs::default());
        let parsed = CompactArgs::parse("--at turn:t2 keep the API names").unwrap();
        assert_eq!(parsed.at, Some(CompactAt::Turn("t2".into())));
        assert_eq!(parsed.instructions.as_deref(), Some("keep the API names"));
        let parsed = CompactArgs::parse("--at tool_use:abc").unwrap();
        assert_eq!(parsed.at, Some(CompactAt::ToolUse("abc".into())));
        assert_eq!(parsed.instructions, None);
        let plain = CompactArgs::parse("--atlas matters").unwrap();
        assert_eq!(plain.instructions.as_deref(), Some("--atlas matters"));
        assert!(CompactArgs::parse("--at nowhere").is_err());
    }
}
