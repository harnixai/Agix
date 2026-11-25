//! 宿主调用插件函数时池要记着的东西：一个会话里正在跑几个宿主调用（`HostCalls`，
//! 有就不算空闲，不被重建、回滚、驱逐），以及不带会话的调用各自建的临时引擎
//! （`OneCallEngines`）。临时引擎按它的会话 id 登记，调用在跑时它向宿主连接发出的
//! hook 调用经这张表找得到、答得了；整池关闭时由它停掉这些调用、等它们收尾、回收
//! 它们的进程组。

use crate::session_pool::parts::hub::SessionHub;
use crate::session_pool::parts::live::SessionCore;
use runtime::agent::InputSender;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::{CancellationToken, DropGuard};

/// The host calls running in one session, from before its engine is handed
/// them until they have ended.
#[derive(Default)]
pub(in crate::session_pool) struct HostCalls(Arc<AtomicUsize>);

impl HostCalls {
    pub(in crate::session_pool) fn hold(&self) -> HostCallHold {
        self.0.fetch_add(1, Ordering::SeqCst);
        HostCallHold(self.0.clone())
    }

    pub(in crate::session_pool) fn any(&self) -> bool {
        self.0.load(Ordering::SeqCst) > 0
    }
}

/// One running host call, counted until dropped.
pub(in crate::session_pool) struct HostCallHold(Arc<AtomicUsize>);

impl Drop for HostCallHold {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct OneCallEngine {
    stop: CancellationToken,
    ended: CancellationToken,
    core: Arc<SessionCore>,
    /// Where the host's answers to the engine's calls go, once it is built.
    input: Option<InputSender>,
}

/// The engines built for one host call each, outside the session table, by
/// the id of the session each stands in.
#[derive(Default)]
pub(in crate::session_pool) struct OneCallEngines {
    engines: Arc<Mutex<HashMap<String, OneCallEngine>>>,
}

impl OneCallEngines {
    /// Keep `core`'s engine, whose call `stop` ends, until the guard is
    /// dropped, unless `refuse` says no — asked under the table's lock, so a
    /// shutdown that has marked itself before listing the engines sees every
    /// one admitted.
    pub(in crate::session_pool) fn admit(
        &self,
        core: &Arc<SessionCore>,
        stop: CancellationToken,
        refuse: impl FnOnce() -> Result<(), String>,
    ) -> Result<OneCallGuard, String> {
        let mut engines = self.engines.lock().unwrap_or_else(|e| e.into_inner());
        refuse()?;
        let ended = CancellationToken::new();
        engines.insert(
            core.id().to_string(),
            OneCallEngine {
                stop,
                ended: ended.clone(),
                core: core.clone(),
                input: None,
            },
        );
        Ok(OneCallGuard {
            core: core.clone(),
            engines: self.engines.clone(),
            _ended: ended.drop_guard(),
        })
    }

    /// The input and hub of the engine standing in session `id`, once it
    /// takes answers.
    pub(in crate::session_pool) fn answerer(
        &self,
        id: &str,
    ) -> Option<(InputSender, Arc<SessionHub>)> {
        let engines = self.engines.lock().unwrap_or_else(|e| e.into_inner());
        let engine = engines.get(id)?;
        Some((engine.input.clone()?, engine.core.hub().clone()))
    }

    pub(in crate::session_pool) fn hubs(&self) -> Vec<Arc<SessionHub>> {
        let engines = self.engines.lock().unwrap_or_else(|e| e.into_inner());
        engines.values().map(|e| e.core.hub().clone()).collect()
    }

    /// Stop every call still running on an engine of its own, handing back
    /// what says each has ended and each one's session.
    pub(in crate::session_pool) fn stop_all(&self) -> Vec<(CancellationToken, Arc<SessionCore>)> {
        let engines = self.engines.lock().unwrap_or_else(|e| e.into_inner());
        engines
            .values()
            .map(|engine| {
                engine.stop.cancel();
                (engine.ended.clone(), engine.core.clone())
            })
            .collect()
    }
}

/// One admitted engine. However its call ends, what the call left running
/// is stopped, and the engine is gone from the pool's keeping.
pub(in crate::session_pool) struct OneCallGuard {
    core: Arc<SessionCore>,
    engines: Arc<Mutex<HashMap<String, OneCallEngine>>>,
    _ended: DropGuard,
}

impl OneCallGuard {
    /// The host's answers to the engine's calls go to `input` from now on.
    pub(in crate::session_pool) fn answer_through(&self, input: InputSender) {
        let mut engines = self.engines.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(engine) = engines.get_mut(self.core.id()) {
            engine.input = Some(input);
        }
    }
}

impl Drop for OneCallGuard {
    fn drop(&mut self) {
        for task in self.core.state().running_background_tasks() {
            task.cancel.cancel();
        }
        self.core.reap_process_groups_later();
        self.engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(self.core.id());
    }
}
