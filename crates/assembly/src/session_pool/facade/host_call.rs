//! 宿主在一轮之外调一个工具（`plugin.call`、`mcp.callTool`）共用的部分：调用的时限与
//! 取消，不带会话时给这一次调用建的临时引擎，以及把临时引擎的 hook 调用带着来源标记
//! 交给登记 hook 的连接。临时引擎不写日志、不进会话表、不向订阅者发事件，调用结束就收掉。

use crate::session_pool::parts::factory::{Restore, SessionSeed};
use crate::session_pool::parts::live::{Naming, SessionCore};
use crate::session_pool::parts::specs::SessionSpec;
use crate::session_pool::*;
use api::{HostCallOrigin, SessionOptions};
use base::interface::plugin_call::HostToolRunner;
use std::future::Future;
use tokio_util::sync::DropGuard;

/// How long a call past its time is waited for once told to stop.
const WRAP_UP: Duration = Duration::from_secs(2);

/// A call's engine of its own, in `dir` on `scene`.
pub(in crate::session_pool) struct TransientEngine<'a> {
    pub origin: Arc<HostCallOrigin>,
    pub dir: &'a Path,
    pub scene: Arc<SceneSpec>,
    /// The pair's MCP servers the engine gets; `None` connects none.
    pub mcp_module: Option<Arc<dyn runtime::module::Module>>,
}

impl SessionPool {
    /// `call` in a task of its own, told to stop by `cancel` or once `time`
    /// is out. A call past its time is answered by `ran_out` with what it
    /// came to once it stopped, or `None` if it is still finishing two
    /// seconds later (its hooks, say); the rest of it then ends in the
    /// background, still holding what it holds.
    pub(in crate::session_pool) async fn within_time<T, F>(
        &self,
        time: Duration,
        cancel: &CancellationToken,
        call: impl FnOnce(Arc<SessionPool>, CancellationToken) -> F,
        ran_out: impl FnOnce(Option<T>) -> T,
    ) -> Result<T, PoolError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, PoolError>> + Send + 'static,
    {
        let pool = self
            .this
            .upgrade()
            .ok_or_else(|| PoolError::Internal("the pool is shutting down".into()))?;
        let stop = cancel.child_token();
        let _stop_if_dropped = stop.clone().drop_guard();
        let mut run = tokio::spawn(call(pool, stop.clone()));
        tokio::select! {
            joined = &mut run => joined_result(joined),
            _ = tokio::time::sleep(time) => {
                stop.cancel();
                let ended = match tokio::time::timeout(WRAP_UP, &mut run).await {
                    Ok(joined) => Some(joined_result(joined)?),
                    Err(_) => None,
                };
                Ok(ran_out(ended))
            }
        }
    }

    /// `call` on an engine of its own: the scene's tools and the directory's
    /// rules, attended so the call itself is the host's approval, with
    /// nobody to ask about the calls it makes.
    ///
    /// While the call runs, the engine is answerable under its session id:
    /// the hook calls it puts to host connections go to them as `origin`'s,
    /// and their answers come back to it.
    pub(in crate::session_pool) async fn on_transient_engine<T, F>(
        &self,
        engine: TransientEngine<'_>,
        cancel: CancellationToken,
        call: impl FnOnce(Arc<dyn HostToolRunner>) -> F,
    ) -> Result<T, PoolError>
    where
        F: Future<Output = T>,
    {
        let TransientEngine {
            origin,
            dir,
            scene,
            mcp_module,
        } = engine;
        let core = SessionCore::new(
            &Id::new().to_string(),
            scene.clone(),
            Some(dir.to_path_buf()).into(),
            Naming::default(),
            time::OffsetDateTime::now_utc(),
        );
        let kept = self
            .live
            .keep_one_call(&core, cancel)
            .map_err(PoolError::Internal)?;
        let mut built = self.build_transient(&core, scene, dir, mcp_module).await?;
        let events = std::mem::replace(
            &mut built.event_rx,
            tokio::sync::mpsc::unbounded_channel().1,
        );
        core.hub().serve_hosts(self.factory.hosts().clone());
        core.hub().answer_host_calls_with(built.input_tx.clone());
        kept.answer_through(built.input_tx.clone());
        built.agent.serve_host_answers();
        let relayed = relay_to_hosts(events, core.hub().clone(), core.id(), origin);
        let runner = built.agent.host_tool_runner();
        let outcome = call(runner.clone()).await;
        drop((runner, built, relayed));
        Ok(outcome)
    }

    /// The engine of a call in no session in `dir`, standing in `core`.
    async fn build_transient(
        &self,
        core: &Arc<SessionCore>,
        scene: Arc<SceneSpec>,
        dir: &Path,
        mcp_module: Option<Arc<dyn runtime::module::Module>>,
    ) -> Result<BuiltEngine, PoolError> {
        let binding = self.binding_for(Some(dir), &scene.id).await;
        let spec = SessionSpec {
            options: Some(SessionOptions {
                attended: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let plugins = Arc::new(self.session_plugins(&spec));
        let mut built = self.factory.build(BuildRequest {
            session_id: core.id(),
            seed: SessionSeed {
                state: core.state().clone(),
                steer: core.steer().clone(),
                generation: self.live.next_generation(),
            },
            restore: Restore::New,
            scene,
            binding: &binding,
            project_root: core.project(),
            worktree: None,
            script_root: dir,
            spec: &spec,
            scene_registry: self.scenes.registry(),
            cron_store: self.scheduler.store(),
            packages: self.plugins.for_session(&plugins),
            plugins,
            mcp_module,
            process_groups: Some(core.process_groups().clone()),
            skill_dirs: core.skill_dirs(),
            protected_dirs: self.bindings.host_dirs().0.protected().clone(),
            transient: true,
        })?;
        self.plugins
            .note_script_faults(std::mem::take(&mut built.script_faults));
        built.agent.attach_session();
        Ok(built)
    }
}

/// Hand what the engine of the call in no session `origin` says to its hub,
/// where only its hook calls find anyone — the connections that registered
/// the hooks. Once dropped, what the engine has said so far is still handed
/// over, and nothing after.
fn relay_to_hosts(
    mut events: EventReceiver,
    hub: Arc<SessionHub>,
    session_id: &str,
    origin: Arc<HostCallOrigin>,
) -> DropGuard {
    let done = CancellationToken::new();
    let session_id: Arc<str> = Arc::from(session_id);
    tokio::spawn({
        let done = done.clone();
        async move {
            loop {
                tokio::select! {
                    biased;
                    event = events.recv() => match event {
                        Some(event) => publish_host_call_event(&hub, &session_id, &origin, event),
                        None => return,
                    },
                    _ = done.cancelled() => break,
                }
            }
            while let Ok(event) = events.try_recv() {
                publish_host_call_event(&hub, &session_id, &origin, event);
            }
        }
    });
    done.drop_guard()
}

fn joined_result<T>(
    joined: Result<Result<T, PoolError>, tokio::task::JoinError>,
) -> Result<T, PoolError> {
    joined.map_err(|e| PoolError::Internal(format!("the host's call failed: {e}")))?
}
