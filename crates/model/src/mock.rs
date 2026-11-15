//! 测试用 mock client。
//!
//! 预先 push 多个 turn 的事件序列；每次 `stream_messages` 弹出一组事件流。
//! 不上 feature gate —— 体量很小、跨 crate 的 engine 测试都需要它。

use crate::client::{AnthropicClient, CountFuture, EventStream};
use crate::error::AnthropicError;
use crate::stream::{BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage};
use crate::types::MessagesRequest;
use futures::stream;
use std::collections::VecDeque;
use std::sync::Mutex;

/// 把每次 `stream_messages` 收到的请求与回放的事件流脚本化。
pub struct MockAnthropicClient {
    turns: Mutex<VecDeque<Vec<Result<StreamEvent, AnthropicError>>>>,
    captured: Mutex<Vec<MessagesRequest>>,
}

impl MockAnthropicClient {
    pub fn new() -> Self {
        Self {
            turns: Mutex::new(VecDeque::new()),
            captured: Mutex::new(Vec::new()),
        }
    }

    /// 队尾追加一个 turn 的事件序列。所有事件都标记 Ok。
    pub fn push_turn(&self, events: Vec<StreamEvent>) {
        self.turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(events.into_iter().map(Ok).collect());
    }

    /// 队尾追加一个 turn：一个 text 块，以 `end_turn` 结束。
    pub fn push_text(&self, text: &str) {
        self.push_turn(vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::TextDelta {
                    text: text.to_string(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            Self::message_end(base::message::StopReason::EndTurn),
        ]);
    }

    /// 队尾追加一个 turn：一个 tool_use 块，以 `tool_use` 结束。
    pub fn push_tool_use(&self, id: &str, name: &str, input: serde_json::Value) {
        self.push_turn(vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: serde_json::Value::Null,
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::InputJsonDelta {
                    partial_json: input.to_string(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            Self::message_end(base::message::StopReason::ToolUse),
        ]);
    }

    fn message_end(stop_reason: base::message::StopReason) -> StreamEvent {
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(stop_reason),
                stop_sequence: None,
            },
            usage: Some(Usage::default()),
        }
    }

    /// 队尾追加一个 turn，允许混入 Err（用来测错误传播）。
    pub fn push_turn_with_errors(&self, events: Vec<Result<StreamEvent, AnthropicError>>) {
        self.turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(events);
    }

    /// 还剩多少个 turn 没回放。
    pub fn turns_remaining(&self) -> usize {
        self.turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// 已经接收了多少次 `stream_messages` 调用。
    pub fn calls(&self) -> usize {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// 取走第 N 次调用的请求 clone。
    pub fn nth_request(&self, idx: usize) -> Option<MessagesRequest> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(idx)
            .cloned()
    }
}

impl Default for MockAnthropicClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicClient for MockAnthropicClient {
    fn stream_messages(&self, req: MessagesRequest) -> EventStream {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(req);
        let events = self
            .turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_default();
        Box::pin(stream::iter(events))
    }

    fn count_tokens<'a>(&'a self, _: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{MessageStartPayload, Usage};
    use futures::StreamExt;

    #[tokio::test]
    async fn replays_pushed_events_in_order() {
        let m = MockAnthropicClient::new();
        m.push_turn(vec![
            StreamEvent::MessageStart {
                message: MessageStartPayload {
                    id: "a".into(),
                    model: "x".into(),
                    role: "assistant".into(),
                    usage: Usage::default(),
                    stop_reason: None,
                },
            },
            StreamEvent::MessageStop,
        ]);
        m.push_turn(vec![StreamEvent::MessageStop]);
        assert_eq!(m.turns_remaining(), 2);

        let s = m.stream_messages(MessagesRequest::minimal("x", "u"));
        let evs: Vec<_> = s.collect().await;
        assert_eq!(evs.len(), 2);
        assert_eq!(m.turns_remaining(), 1);
        assert_eq!(m.calls(), 1);
    }

    #[tokio::test]
    async fn push_text_replays_one_text_block_ending_the_turn() {
        let m = MockAnthropicClient::new();
        m.push_text("你好");
        let evs: Vec<_> = m
            .stream_messages(MessagesRequest::minimal("x", "u"))
            .collect()
            .await;
        assert!(matches!(
            evs.first(),
            Some(Ok(StreamEvent::ContentBlockStart { .. }))
        ));
        assert!(matches!(
            evs.last(),
            Some(Ok(StreamEvent::MessageDelta { delta, .. }))
                if delta.stop_reason == Some(base::message::StopReason::EndTurn)
        ));
        assert!(evs.iter().any(|e| matches!(
            e,
            Ok(StreamEvent::ContentBlockDelta { delta: BlockDelta::TextDelta { text }, .. }) if text == "你好"
        )));
    }

    #[tokio::test]
    async fn push_tool_use_replays_one_call_ending_on_tool_use() {
        let m = MockAnthropicClient::new();
        m.push_tool_use("toolu_1", "Echo", serde_json::json!({"a": 1}));
        let evs: Vec<_> = m
            .stream_messages(MessagesRequest::minimal("x", "u"))
            .collect()
            .await;
        assert!(matches!(
            evs.first(),
            Some(Ok(StreamEvent::ContentBlockStart {
                content_block: ContentBlockStart::ToolUse { id, name, .. }, ..
            })) if id == "toolu_1" && name == "Echo"
        ));
        assert!(matches!(
            evs.last(),
            Some(Ok(StreamEvent::MessageDelta { delta, .. }))
                if delta.stop_reason == Some(base::message::StopReason::ToolUse)
        ));
        assert!(evs.iter().any(|e| matches!(
            e,
            Ok(StreamEvent::ContentBlockDelta { delta: BlockDelta::InputJsonDelta { partial_json }, .. })
                if partial_json == r#"{"a":1}"#
        )));
    }
}
