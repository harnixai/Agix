//! Which file reads a compaction or trim took out of the model's view, so the
//! session stops answering a repeat of one with "unchanged": the runtime
//! compares the transcript before and after every step that rewrites it.

use base::interface::model::{ModelContentBlock, ModelMessage};
use std::collections::HashMap;

/// The `Read` calls whose result `before` held and `after` no longer does:
/// dropped, summarized, or cleared or cut in place. A result whose text is
/// still in `after` word for word — re-attached whole after a compaction —
/// still counts as held.
pub fn reads_lost_from_view(before: &[ModelMessage], after: &[ModelMessage]) -> Vec<String> {
    let read_ids: Vec<&str> = blocks(before)
        .filter_map(|block| match block {
            ModelContentBlock::ToolUse { id, name, .. }
                if crate::compact::FILE_READ_TOOLS.contains(&name.as_str()) =>
            {
                Some(id.as_str())
            }
            _ => None,
        })
        .collect();
    if read_ids.is_empty() {
        return Vec::new();
    }
    let results_before = results_of(before, &read_ids);
    let results_after = results_of(after, &read_ids);
    let texts_after: Vec<&str> = blocks(after)
        .filter_map(|block| match block {
            ModelContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    results_before
        .into_iter()
        .filter(|(id, content)| {
            results_after.get(id) != Some(content)
                && !texts_after.iter().any(|text| text.contains(content))
        })
        .map(|(id, _)| id.to_string())
        .collect()
}

fn blocks(messages: &[ModelMessage]) -> impl Iterator<Item = &ModelContentBlock> {
    messages.iter().flat_map(|m| &m.content)
}

fn results_of<'a>(messages: &'a [ModelMessage], ids: &[&str]) -> HashMap<&'a str, &'a str> {
    blocks(messages)
        .filter_map(|block| match block {
            ModelContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if ids.contains(&tool_use_id.as_str()) => {
                Some((tool_use_id.as_str(), content.as_str()))
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::MessageRole;

    fn call(id: &str, name: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input: serde_json::json!({ "file_path": format!("/p/{id}") }),
            }],
        }
    }

    fn result(id: &str, content: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: content.into(),
                is_error: Some(false),
            }],
        }
    }

    fn text(t: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: t.into() }],
        }
    }

    #[test]
    fn a_read_dropped_cleared_or_cut_is_lost_and_one_kept_or_reattached_is_not() {
        let before = vec![
            call("dropped", "Read"),
            result("dropped", "     1\tgone\n"),
            call("reattached", "Read"),
            result("reattached", "     1\tback\n"),
            call("cleared", "Read"),
            result("cleared", "     1\tblanked\n"),
            call("cut", "Read"),
            result("cut", "     1\thead\n     2\ttail\n"),
            call("kept", "Read"),
            result("kept", "     1\tstill here\n"),
            call("bash", "Bash"),
            result("bash", "output"),
        ];
        let after = vec![
            text("<system-reminder>\n## /p/reattached\n\n     1\tback\n\n</system-reminder>"),
            call("cleared", "Read"),
            result("cleared", "[Old tool result content cleared]"),
            call("cut", "Read"),
            result("cut", "[Tool result truncated]\n     1\thead\n..."),
            call("kept", "Read"),
            result("kept", "     1\tstill here\n"),
        ];
        let mut lost = reads_lost_from_view(&before, &after);
        lost.sort();
        assert_eq!(lost, ["cleared", "cut", "dropped"]);
        assert!(reads_lost_from_view(&before, &before).is_empty());
    }
}
