//! 每个会话自己的事件出口 `SessionHub`：谁在看它、它还在等谁回答、它最近一次
//! 有动静是什么时候。宿主工具调用发出时没人在看、或看的人走光了，由它告诉引擎
//! 不必再等。事件经它同步交给订阅者，不经过池级的 `sessions` 锁。
//!
//! 由某条宿主连接提供的调用（`PendingAsk::host`）另走一条路：工具调用交给订阅者
//! 和提供它的连接，hook 调用只交给提供它的连接；订阅者走光不结束它们，提供它的
//! 连接不在了（`HostConnections` 里查不到、投递失败、断开）才结束；投递时发现它
//! 已关闭，也把它从 `HostConnections` 里摘掉。
//!
//! 锁序里它只在 `HostConnections` 之上：持着它不 `.await`，只会再去查那张叶子表。帧在锁下定好收件人、排进
//! 发件队列，放开锁再交给 `SessionWatcher::deliver`。投递由另一把锁串行：同一时刻
//! 只有一个线程在投递，发帧的调用返回时它的帧已经交出去了，每个订阅者收到的帧仍是
//! 发出的先后。`deliver` 里再往这个 hub 发帧不会卡住：同一线程上再发的帧排在队尾，
//! 由正在投递的这一层接着交出去。回合的终止帧是在池的 `sessions` 锁下发出的，所以
//! `deliver` 不能等池；要用池的工作交给它另起的任务。

use crate::session_pool::api::{AskKind, PendingAsk, SessionFrame, SessionWatcher, WatcherId};
use crate::session_pool::parts::hosts::HostConnections;
use base::interface::host_call::{HostCallEnd, HostId};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

pub(in crate::session_pool) struct SessionHub {
    inner: Mutex<Inner>,
    delivery: Mutex<()>,
    epoch: Instant,
    active_ms: AtomicU64,
}

#[derive(Default)]
struct Inner {
    watchers: Vec<Arc<dyn SessionWatcher>>,
    pending: HashMap<String, PendingAsk>,
    outbox: VecDeque<Outgoing>,
    delivering_on: Option<ThreadId>,
    /// The running engine's input, where a host call nobody can answer is
    /// ended.
    engine: Option<runtime::agent::InputSender>,
    /// Who provides the calls a host connection answers.
    hosts: Option<HostConnections>,
}

impl Inner {
    fn end_call(&self, call_id: &str, end: HostCallEnd) {
        if let Some(engine) = &self.engine {
            let _ = engine.send(runtime::agent::InputMessage::HostToolResponse {
                call_id: call_id.to_string(),
                end,
            });
        }
    }

    /// End every open host call any watcher may answer with `end` once
    /// nobody is left to answer it.
    fn end_host_calls_if_unwatched(&self, end: fn() -> HostCallEnd) {
        if !self.watchers.is_empty() {
            return;
        }
        for ask in self.pending.values() {
            if ask.kind == AskKind::HostToolCall && ask.host.is_none() {
                self.end_call(&ask.prompt_id, end());
            }
        }
    }

    /// End every open call `host` was to answer.
    fn end_calls_of(&self, host: HostId) {
        for ask in self.pending.values() {
            if ask.host == Some(host) {
                self.end_call(&ask.prompt_id, HostCallEnd::HostGone);
            }
        }
    }

    fn provider(&self, host: HostId) -> Option<Arc<dyn SessionWatcher>> {
        self.hosts.as_ref().and_then(|hosts| hosts.provider(host))
    }

    /// Who is handed a frame of the ask `kind` that `host` provides: a hook
    /// call's only by its provider, a tool call's by the watchers and its
    /// provider, any other's by the watchers.
    fn recipients(
        &self,
        kind: AskKind,
        host: Option<HostId>,
        except: Option<WatcherId>,
    ) -> Vec<Arc<dyn SessionWatcher>> {
        let provider = host.and_then(|host| self.provider(host));
        let mut to: Vec<Arc<dyn SessionWatcher>> = if kind == AskKind::HookCall {
            Vec::new()
        } else {
            self.watchers
                .iter()
                .filter(|w| Some(w.id()) != except)
                .cloned()
                .collect()
        };
        if let Some(provider) = provider {
            if !to.iter().any(|w| w.id() == provider.id()) {
                to.push(provider);
            }
        }
        to
    }
}

struct Outgoing {
    frame: SessionFrame,
    to: Vec<Arc<dyn SessionWatcher>>,
}

impl SessionHub {
    pub(in crate::session_pool) fn active_at(at: Instant) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            delivery: Mutex::new(()),
            epoch: at,
            active_ms: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(in crate::session_pool) fn touch(&self) {
        let ms = Instant::now()
            .saturating_duration_since(self.epoch)
            .as_millis();
        self.active_ms
            .fetch_max(u64::try_from(ms).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    pub(in crate::session_pool) fn last_active(&self) -> Instant {
        self.epoch + Duration::from_millis(self.active_ms.load(Ordering::Relaxed))
    }

    pub(in crate::session_pool) fn publish(&self, frame: &SessionFrame) {
        self.publish_except(frame, None);
    }

    /// To every watcher but `except` — the one that hears the same thing
    /// from its own call. A watcher that cannot take the frame is dropped.
    pub(in crate::session_pool) fn publish_except(
        &self,
        frame: &SessionFrame,
        except: Option<WatcherId>,
    ) {
        self.touch();
        let inner = self.lock();
        let to = inner
            .watchers
            .iter()
            .filter(|w| Some(w.id()) != except)
            .cloned()
            .collect();
        self.send(inner, frame, to);
    }

    /// Where the calls a host connection provides find their provider.
    pub(in crate::session_pool) fn serve_hosts(&self, hosts: HostConnections) {
        self.lock().hosts = Some(hosts);
    }

    /// Where the session's host calls are ended when nobody can answer
    /// them: the engine now running it.
    pub(in crate::session_pool) fn answer_host_calls_with(
        &self,
        engine: runtime::agent::InputSender,
    ) {
        self.lock().engine = Some(engine);
    }

    /// Recorded before it goes out, under the same lock: a watcher that
    /// subscribes meanwhile is handed it either way. A host call made while
    /// nobody watches ends at once; one a host connection provides, when
    /// that connection is not there.
    pub(in crate::session_pool) fn publish_ask(
        &self,
        frame: Arc<SessionFrame>,
        prompt_id: String,
        task_id: Option<String>,
        kind: AskKind,
        host: Option<HostId>,
    ) {
        self.touch();
        let mut inner = self.lock();
        inner.pending.insert(
            prompt_id.clone(),
            PendingAsk {
                prompt_id: prompt_id.clone(),
                task_id,
                kind,
                host,
                frame: frame.clone(),
            },
        );
        match host {
            Some(host) if inner.provider(host).is_none() => {
                inner.end_call(&prompt_id, HostCallEnd::NoHost);
            }
            Some(_) => {}
            None if kind == AskKind::HostToolCall => {
                inner.end_host_calls_if_unwatched(|| HostCallEnd::NoHost);
            }
            None => {}
        }
        let to = inner.recipients(kind, host, None);
        self.send(inner, &frame, to);
    }

    /// The ask `prompt_id` is no longer open: forgotten and announced under
    /// the same lock, so a watcher that subscribes meanwhile is handed either
    /// the ask or the frame that ends it, never the ask alone. The end of a
    /// hook call goes where its call went, and nowhere once it is forgotten.
    pub(in crate::session_pool) fn publish_resolved(
        &self,
        frame: &SessionFrame,
        prompt_id: &str,
        kind: AskKind,
    ) {
        self.touch();
        let mut inner = self.lock();
        let host = inner.pending.remove(prompt_id).and_then(|ask| ask.host);
        let to = match (kind, host) {
            (AskKind::HookCall, None) => Vec::new(),
            (kind, host) => inner.recipients(kind, host, None),
        };
        self.send(inner, frame, to);
    }

    /// `host` went away: end every call it was to answer.
    pub(in crate::session_pool) fn end_calls_of(&self, host: HostId) {
        self.lock().end_calls_of(host);
    }

    /// Start watching, and what is still being asked as of that moment —
    /// but no hook call, which only its provider is handed, as it is made.
    pub(in crate::session_pool) fn subscribe(
        &self,
        watcher: Arc<dyn SessionWatcher>,
    ) -> Vec<PendingAsk> {
        let mut inner = self.lock();
        let id = watcher.id();
        inner.watchers.retain(|w| w.id() != id);
        inner.watchers.push(watcher);
        inner
            .pending
            .values()
            .filter(|ask| ask.kind != AskKind::HookCall)
            .cloned()
            .collect()
    }

    pub(in crate::session_pool) fn unsubscribe(&self, id: WatcherId) {
        let mut inner = self.lock();
        inner.watchers.retain(|w| w.id() != id);
        inner.end_host_calls_if_unwatched(|| HostCallEnd::HostGone);
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn is_watched_by(&self, id: WatcherId) -> bool {
        self.lock().watchers.iter().any(|w| w.id() == id)
    }

    pub(in crate::session_pool) fn ask_kind(&self, prompt_id: &str) -> Option<AskKind> {
        self.lock().pending.get(prompt_id).map(|ask| ask.kind)
    }

    /// The open ask `prompt_id`'s kind, and the connection that alone
    /// answers it when one does.
    pub(in crate::session_pool) fn ask_host(
        &self,
        prompt_id: &str,
    ) -> Option<(AskKind, Option<HostId>)> {
        self.lock()
            .pending
            .get(prompt_id)
            .map(|ask| (ask.kind, ask.host))
    }

    /// The frame the open ask `prompt_id` went out in.
    pub(in crate::session_pool) fn ask_frame(&self, prompt_id: &str) -> Option<Arc<SessionFrame>> {
        self.lock()
            .pending
            .get(prompt_id)
            .map(|ask| ask.frame.clone())
    }

    pub(in crate::session_pool) fn forget_asks_of_task(&self, task_id: &str) {
        self.lock()
            .pending
            .retain(|_, ask| ask.task_id.as_deref() != Some(task_id));
    }

    /// Forget the asks a turn's own engine raised; a background task's stay.
    pub(in crate::session_pool) fn retain_task_asks(&self) {
        self.lock().pending.retain(|_, ask| ask.task_id.is_some());
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn pending_prompt_ids(&self) -> Vec<String> {
        self.lock().pending.keys().cloned().collect()
    }

    /// Back to watching nothing and asking nothing.
    pub(in crate::session_pool) fn clear(&self) {
        let mut inner = self.lock();
        inner.watchers.clear();
        inner.pending.clear();
    }

    /// Queue `frame` for `to` and deliver the queue with the state lock
    /// released. Returns once `frame` has been handed over, except when
    /// called from inside a `deliver` on this hub: the delivery under way
    /// hands it over next. A watcher that cannot take a frame is dropped and
    /// gets no more, and the calls it was to answer end.
    fn send(
        &self,
        mut inner: MutexGuard<'_, Inner>,
        frame: &SessionFrame,
        to: Vec<Arc<dyn SessionWatcher>>,
    ) {
        inner.outbox.push_back(Outgoing {
            frame: frame.clone(),
            to,
        });
        let this_thread = std::thread::current().id();
        if inner.delivering_on == Some(this_thread) {
            return;
        }
        drop(inner);
        let _delivery = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
        let _unwind = StillDelivering(self);
        // By the subscription, not its id: one that subscribes again under
        // the same id while this delivery goes on is a new one, and stays.
        let mut dropped: Vec<Arc<dyn SessionWatcher>> = Vec::new();
        let is_dropped = |dropped: &[Arc<dyn SessionWatcher>], w: &Arc<dyn SessionWatcher>| {
            dropped.iter().any(|d| Arc::ptr_eq(d, w))
        };
        loop {
            let mut inner = self.lock();
            if !dropped.is_empty() {
                inner.watchers.retain(|w| !is_dropped(&dropped, w));
                inner.end_host_calls_if_unwatched(|| HostCallEnd::HostGone);
                for w in &dropped {
                    let host = HostId(w.id().0);
                    inner.end_calls_of(host);
                    if !w.is_open() {
                        if let Some(hosts) = &inner.hosts {
                            hosts.remove(host);
                        }
                    }
                }
            }
            let Some(Outgoing { frame, to }) = inner.outbox.pop_front() else {
                inner.delivering_on = None;
                return;
            };
            inner.delivering_on = Some(this_thread);
            drop(inner);
            for watcher in to {
                if !is_dropped(&dropped, &watcher) && !watcher.deliver(&frame) {
                    dropped.push(watcher);
                }
            }
        }
    }
}

/// A watcher that panics in `deliver` must not leave the hub believing a
/// delivery is still under way, which would hold every later frame back.
struct StillDelivering<'a>(&'a SessionHub);

impl Drop for StillDelivering<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.lock().delivering_on = None;
        }
    }
}
