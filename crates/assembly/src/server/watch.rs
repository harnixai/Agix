//! 一条连接作为会话的订阅者：池交来的事件投影成 `session.event` 帧，非阻塞地
//! 放进这条连接的出站队列。响应走同一个队列，所以 `turn_complete` 一定排在
//! `run_turn` 的响应前面。

use crate::rpc::events::SessionEventNotification;
use crate::rpc::methods::session::SubscribeResult;
use crate::rpc::Client;
use crate::session_pool::api::{AskKind, SessionFrame, SessionWatcher, Subscribed, WatcherId};

impl SessionWatcher for Client {
    fn id(&self) -> WatcherId {
        WatcherId(Client::id(self).0)
    }

    fn deliver(&self, frame: &SessionFrame) -> bool {
        let Some(notification) = SessionEventNotification::of(frame) else {
            return true;
        };
        match serde_json::to_string(&notification) {
            Ok(json) => self.try_send_for(&frame.session_id, json),
            Err(e) => {
                tracing::warn!(error = %e, "could not serialize a session frame; dropping it");
                true
            }
        }
    }

    fn is_open(&self) -> bool {
        Client::is_open(self)
    }
}

/// `session.subscribe`'s result: each open ask as the frame that raised it,
/// the host calls apart from the prompts a person answers.
pub(super) fn subscribe_result(subscribed: Subscribed) -> SubscribeResult {
    let frames = |host_calls: bool| -> Vec<serde_json::Value> {
        subscribed
            .pending
            .iter()
            .filter(|ask| (ask.kind == AskKind::HostToolCall) == host_calls)
            .filter_map(|ask| SessionEventNotification::of(&ask.frame))
            .filter_map(|frame| serde_json::to_value(frame).ok())
            .collect()
    };
    SubscribeResult {
        pending_prompts: frames(false),
        pending_host_calls: frames(true),
        session_id: subscribed.session_id,
        last_seq: subscribed.last_seq,
        resumed: subscribed.resumed,
    }
}
