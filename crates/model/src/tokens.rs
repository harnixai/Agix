//! 整个请求（system、tools、messages）的输入 token 估算，按 `MessagesRequest` 的形状求和；
//! 单段文本的计数器在 `base::interface::token_counter`。

use crate::types::{MessagesRequest, SystemBlock};
use base::interface::model::{document_token_estimate, IMAGE_TOKEN_ESTIMATE};
use base::interface::token_counter::count_text;
use base::message::{ContentBlock, ImageSource, ToolResultContent};

/// 估算一段 `MessagesRequest` 的输入 token 数（不含模型输出）。
///
/// 覆盖：
/// - system blocks 文本
/// - tools[] 的 description + input_schema（schema 序列化成 JSON 字符串）
/// - messages[] 的所有 ContentBlock
///
/// 不覆盖（轻微低估）：
/// - role / 字段名等 API 框架开销（实际占 ~5-15 token）
/// - JSON 结构本身（`{` `}` `,` 等）
pub fn estimate_input_tokens(req: &MessagesRequest) -> usize {
    let mut total = 0usize;

    // system
    for block in &req.system {
        let SystemBlock::Text { text, .. } = block;
        total += count_text(text);
    }

    // tools
    for tool in &req.tools {
        total += count_text(&tool.description);
        total += count_text(&tool.input_schema.to_string());
        total += count_text(&tool.name);
    }

    // messages
    for msg in &req.messages {
        for block in &msg.content {
            total += estimate_block_tokens(block);
        }
    }

    total
}

fn estimate_block_tokens(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text, .. } => count_text(text),
        ContentBlock::ToolUse { name, input, id } => {
            count_text(name) + count_text(id) + count_text(&input.to_string())
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            let id_tokens = count_text(tool_use_id);
            let body_tokens = match content {
                ToolResultContent::Text(t) => count_text(t),
                ToolResultContent::Blocks(blocks) => blocks.iter().map(estimate_block_tokens).sum(),
            };
            id_tokens + body_tokens
        }
        ContentBlock::Thinking { thinking, .. } => count_text(thinking),
        ContentBlock::RedactedThinking { data, .. } => count_text(data),
        // Flat per-image estimate, shared with the interface-level estimator so
        // the wire-format and context-budget views of the same image agree.
        ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
        ContentBlock::Document {
            source: ImageSource::Base64 { data, .. },
            ..
        } => document_token_estimate(data.len()),
        ContentBlock::Document { .. } => IMAGE_TOKEN_ESTIMATE,
        // Cache edits are metadata-only (delete-tool-result operations), negligible token cost
        ContentBlock::CacheEdits { .. } => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessageParam, ToolDef};
    use base::message::Role;
    use serde_json::json;

    fn empty_request() -> MessagesRequest {
        MessagesRequest::minimal("test", "")
    }

    #[test]
    fn empty_messages_zero_tokens_in_user() {
        let mut req = empty_request();
        req.messages = vec![]; // 完全空
        assert_eq!(estimate_input_tokens(&req), 0);
    }

    #[test]
    fn user_message_text_counted() {
        let req = MessagesRequest::minimal("test", "hello world");
        let n = estimate_input_tokens(&req);
        assert!((2..=5).contains(&n), "expected 2-5 tokens, got {n}");
    }

    #[test]
    fn longer_text_more_tokens() {
        let short = MessagesRequest::minimal("test", "hi");
        let long = MessagesRequest::minimal(
            "test",
            "hello world this is a longer message with many tokens",
        );
        assert!(estimate_input_tokens(&long) > estimate_input_tokens(&short));
    }

    #[test]
    fn system_block_counted() {
        let mut req = empty_request();
        req.system = vec![SystemBlock::text("system instructions go here")];
        let n_with = estimate_input_tokens(&req);
        req.system.clear();
        let n_without = estimate_input_tokens(&req);
        assert!(n_with > n_without, "system block should increase count");
    }

    #[test]
    fn tools_contribute_tokens() {
        let mut req = empty_request();
        req.tools = vec![ToolDef {
            name: "Bash".into(),
            description: "Run a shell command. Use sparingly.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            }),
            cache_control: None,
            defer_loading: None,
            strict: None,
        }];
        let n = estimate_input_tokens(&req);
        // tools 至少应当 > 5 token
        assert!(n > 5, "expected >5 tokens for one ToolDef, got {n}");
    }

    #[test]
    fn tool_use_block_counted() {
        let mut req = empty_request();
        req.messages = vec![MessageParam {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "toolu_01".into(),
                name: "Bash".into(),
                input: json!({"command": "ls -la"}),
            }],
        }];
        let n = estimate_input_tokens(&req);
        assert!(n > 3, "expected >3 tokens for tool_use, got {n}");
    }

    #[test]
    fn tool_result_text_counted() {
        let mut req = empty_request();
        req.messages = vec![MessageParam {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_01".into(),
                content: ToolResultContent::Text("a\nb\nc".into()),
                is_error: false,
            }],
        }];
        let n = estimate_input_tokens(&req);
        assert!(n > 3);
    }

    #[test]
    fn image_block_costs_1500() {
        let mut req = empty_request();
        req.messages = vec![MessageParam {
            role: Role::User,
            content: vec![ContentBlock::Image {
                source: base::message::ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "tiny".into(), // 数据本身不再额外计 —— 我们用固定估算
                },
            }],
        }];
        let n = estimate_input_tokens(&req);
        assert!(n >= 1500, "image should cost ≈1500 tokens; got {n}");
    }
}
