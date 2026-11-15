//! 宿主连接提供的工具与 hook 的登记处 `HostConnections`：哪条连接给了哪些工具、
//! 注册了哪些 hook，以及把只有它能答的帧交到它手上的那个订阅者。工具在会话建引擎时
//! 拷进去；hook 每次触发时现读，所以对已经建好的会话也立刻生效。
//!
//! 锁序里它是叶子：持着它不去拿任何别的锁，不调订阅者。`SessionHub` 可以在自己的锁
//! 下查它；断开时先从这里摘掉连接、放开锁，再去各个 hub 结束它的在途调用——之间
//! 发出的调用在 hub 锁下查不到它，当场结束。已关闭的连接（`SessionWatcher::is_open`）
//! 不再登记进来：断开时还在途的 `setHostTools` / `setHooks` 晚于清扫完成也不算数。

use crate::session_pool::api::SessionWatcher;
use base::interface::host_call::{HostId, HostToolDecl};
use hooks::{HookEvent, HostHook};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// How many tools one connection may provide.
pub(in crate::session_pool) const MAX_TOOLS_PER_HOST: usize = 64;
/// How many hooks one connection may register.
pub(in crate::session_pool) const MAX_HOOKS_PER_HOST: usize = 16;
/// How many connections may have hooks registered at once.
pub(in crate::session_pool) const MAX_HOOK_HOSTS: usize = 8;
/// How many hook calls one connection may have open at once.
pub(in crate::session_pool) const MAX_HOOK_CALLS_PER_HOST: usize = 256;

/// Why a connection's registration was not taken.
pub(in crate::session_pool) const CLOSED: &str = "this connection is closed";

#[derive(Clone, Default)]
pub(in crate::session_pool) struct HostConnections {
    inner: Arc<Mutex<BTreeMap<HostId, Provider>>>,
}

struct Provider {
    watcher: Arc<dyn SessionWatcher>,
    tools: Vec<HostToolDecl>,
    hooks: Vec<(Vec<HookEvent>, HostHook)>,
    slots: Arc<tokio::sync::Semaphore>,
}

impl Provider {
    fn new(watcher: Arc<dyn SessionWatcher>) -> Self {
        Self {
            watcher,
            tools: Vec::new(),
            hooks: Vec::new(),
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_HOOK_CALLS_PER_HOST)),
        }
    }

    fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.hooks.is_empty()
    }
}

/// A hook to register, with the events it fires on.
pub(in crate::session_pool) struct HookEntry {
    pub(in crate::session_pool) index: usize,
    pub(in crate::session_pool) events: Vec<HookEvent>,
    pub(in crate::session_pool) tools: Vec<String>,
    pub(in crate::session_pool) timeout: std::time::Duration,
    pub(in crate::session_pool) on_error: hooks::OnError,
}

impl HostConnections {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<HostId, Provider>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The watcher whose answer counts for `host`'s calls, while it is
    /// connected and provides something.
    pub(in crate::session_pool) fn provider(
        &self,
        host: HostId,
    ) -> Option<Arc<dyn SessionWatcher>> {
        self.lock().get(&host).map(|p| p.watcher.clone())
    }

    /// Replace what `watcher` provides with `tools`, keeping those whose
    /// name no other connection provides; returns the names of those taken
    /// by another, which stay that one's. A closed connection registers
    /// nothing.
    pub(in crate::session_pool) fn replace_tools(
        &self,
        watcher: Arc<dyn SessionWatcher>,
        tools: Vec<HostToolDecl>,
    ) -> Result<Vec<String>, &'static str> {
        let host = HostId(watcher.id().0);
        let mut all = self.lock();
        if !watcher.is_open() {
            return Err(CLOSED);
        }
        let taken: std::collections::HashSet<String> = all
            .iter()
            .filter(|(id, _)| **id != host)
            .flat_map(|(_, p)| p.tools.iter().map(|t| t.name.clone()))
            .collect();
        let (kept, refused): (Vec<_>, Vec<_>) =
            tools.into_iter().partition(|t| !taken.contains(&t.name));
        let provider = all
            .entry(host)
            .or_insert_with(|| Provider::new(watcher.clone()));
        provider.watcher = watcher;
        provider.tools = kept.into_iter().map(|t| t.host(host)).collect();
        if provider.is_empty() {
            all.remove(&host);
        }
        Ok(refused.into_iter().map(|t| t.name).collect())
    }

    /// Replace the hooks `watcher` registered with `hooks`; refused whole
    /// when it would be one more connection with hooks than allowed, or when
    /// the connection is closed.
    pub(in crate::session_pool) fn replace_hooks(
        &self,
        watcher: Arc<dyn SessionWatcher>,
        hooks: Vec<HookEntry>,
    ) -> Result<(), String> {
        let host = HostId(watcher.id().0);
        let mut all = self.lock();
        if !watcher.is_open() {
            return Err(CLOSED.into());
        }
        let others = all
            .iter()
            .filter(|(id, p)| **id != host && !p.hooks.is_empty())
            .count();
        if !hooks.is_empty() && others >= MAX_HOOK_HOSTS {
            return Err(format!(
                "{MAX_HOOK_HOSTS} connections already have hooks registered with this daemon"
            ));
        }
        let provider = all
            .entry(host)
            .or_insert_with(|| Provider::new(watcher.clone()));
        provider.watcher = watcher;
        let slots = provider.slots.clone();
        provider.hooks = hooks
            .into_iter()
            .map(|h| {
                (
                    h.events,
                    HostHook {
                        host,
                        index: h.index,
                        tools: h.tools,
                        timeout: h.timeout,
                        on_error: h.on_error,
                        slots: slots.clone(),
                    },
                )
            })
            .collect();
        if provider.is_empty() {
            all.remove(&host);
        }
        Ok(())
    }

    /// Every tool the connections provide now, by connection.
    pub(in crate::session_pool) fn tools(&self) -> Vec<HostToolDecl> {
        self.lock()
            .values()
            .flat_map(|p| p.tools.iter().cloned())
            .collect()
    }

    /// Forget `host`; whether it provided anything.
    pub(in crate::session_pool) fn remove(&self, host: HostId) -> bool {
        self.lock().remove(&host).is_some()
    }
}

impl hooks::HostHookSource for HostConnections {
    fn has_hooks_for(&self, event: HookEvent) -> bool {
        self.lock()
            .values()
            .any(|p| p.hooks.iter().any(|(events, _)| events.contains(&event)))
    }

    fn hooks_for(&self, event: HookEvent) -> Vec<HostHook> {
        self.lock()
            .values()
            .flat_map(|p| p.hooks.iter())
            .filter(|(events, _)| events.contains(&event))
            .map(|(_, hook)| hook.clone())
            .collect()
    }
}
