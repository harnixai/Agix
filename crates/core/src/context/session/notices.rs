//! 会话里尚未被 agent 读到的通知，按可撤回的 key 存放。

#[derive(Debug, Default)]
pub(super) struct Notices {
    pending: std::sync::Mutex<Vec<(Option<String>, String)>>,
}

impl Notices {
    pub(super) fn post(&self, key: Option<String>, text: impl Into<String>) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((key, text.into()));
    }

    pub(super) fn retract(&self, key: &str) -> bool {
        let mut notices = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let before = notices.len();
        notices.retain(|(k, _)| k.as_deref() != Some(key));
        notices.len() != before
    }

    pub(super) fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()))
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }
}
