//! 宿主调用插件函数（`[[function]]`）：调用作为一次 `PluginCall` 走会话的整条工具
//! 管线。带会话时用那个会话的引擎，不开新一轮、不进对话；不带会话时在给定目录下
//! 建一个临时引擎——不写日志、不进会话表、不连 MCP、不向订阅者发事件，调用结束
//! 就收掉。临时引擎照样问宿主连接登记的 hook：它的 hook 调用带着 `plugin_call`
//! 标记交给登记者，调用在跑时按临时会话 id 答得了。设计见 docs/runext.md §12。

use crate::session_pool::parts::factory::{Restore, SessionSeed};
use crate::session_pool::parts::host_calls::HostCallHold;
use crate::session_pool::parts::live::{Naming, SessionCore};
use crate::session_pool::parts::phase::{SessionOp, SessionPhase};
use crate::session_pool::parts::specs::SessionSpec;
use crate::session_pool::*;
use api::{
    PluginCall, PluginCallError, PluginCallOrigin, PluginCallResult, PluginCallTarget,
    PluginFunctionInfo, SessionOptions,
};
use base::command::PluginFunction;
use base::interface::plugin_call::HostToolRunner;
use base::interface::tool_middleware::ToolOutcome;
use base::permission::PermissionRule;
use tokio_util::sync::DropGuard;

impl SessionPool {
    /// The functions the enabled packages expose to the host; for a session
    /// `target`, those of the packages it draws on. A `target` given must be
    /// there: the session, or the directory.
    pub async fn plugin_functions(
        &self,
        target: Option<&PluginCallTarget>,
    ) -> Result<Vec<PluginFunctionInfo>, PoolError> {
        self.require_plugin_functions()?;
        let functions = self.plugins.functions();
        let functions = match target {
            Some(PluginCallTarget::Session(id)) if !self.session_exists(id).await => {
                return Err(PoolError::no_session(id));
            }
            Some(PluginCallTarget::Project(dir)) if !dir.is_dir() => {
                return Err(PoolError::ProjectNotFound { path: dir.clone() });
            }
            Some(PluginCallTarget::Session(id)) => self
                .plugins_of_session(id)
                .await
                .retain(functions, |f| &f.plugin),
            _ => functions,
        };
        Ok(functions.iter().map(info_of).collect())
    }

    /// Run `call` and hand back what the function returned or why it did
    /// not. `Err` is a call that cannot be made at all: no such function,
    /// session or directory, a time over the limit.
    ///
    /// A call past its time is told to stop and answers `timeout` once it
    /// has, or two seconds later if it is still finishing (its hooks, say);
    /// the rest of it then ends in the background, still holding its session
    /// or its engine.
    pub async fn call_plugin(
        &self,
        call: PluginCall,
        cancel: CancellationToken,
    ) -> Result<PluginCallResult, PoolError> {
        self.require_plugin_functions()?;
        let time = time_of(&call)?;
        let function = self.function_named(&call.function, &call.target).await?;
        let plugin = function.plugin.clone();
        let tool = runtime::plugin_call::tool(runtime::plugin_call::FunctionCall {
            function,
            writes: call.writes,
            time: Some(time),
            session_id: match &call.target {
                PluginCallTarget::Session(id) => Some(id.clone()),
                PluginCallTarget::Project(_) => None,
            },
            engine: self.layout().engine(),
            run_slots: match &call.target {
                PluginCallTarget::Session(_) => Some(self.run_slots()),
                PluginCallTarget::Project(_) => None,
            },
            host_js_libs: self.factory.host_js_libs().clone(),
        })
        .map_err(PoolError::InvalidRequest)?;
        let PluginCall {
            function: name,
            input,
            target,
            permission_rules: rules,
            run_id,
            ..
        } = call;
        let origin = Arc::new(PluginCallOrigin {
            function: name.clone(),
            run_id,
        });
        let input = serde_json::Value::Object(serde_json::Map::from_iter([
            ("function".to_string(), name.clone().into()),
            ("input".to_string(), input),
        ]));
        let pool = self
            .this
            .upgrade()
            .ok_or_else(|| PoolError::Internal("the pool is shutting down".into()))?;
        let stop = cancel.child_token();
        let _stop_if_dropped = stop.clone().drop_guard();
        let mut run = tokio::spawn({
            let stop = stop.clone();
            async move {
                match &target {
                    PluginCallTarget::Session(id) => {
                        let (runner, _running) = pool.session_runner(id).await?;
                        pool.refuse_left_out(id, &plugin).await?;
                        Ok(result_of(&name, runner.run(tool, input, rules, stop).await))
                    }
                    PluginCallTarget::Project(dir) => {
                        pool.run_transient(origin, dir, tool, input, rules, stop)
                            .await
                    }
                }
            }
        });
        tokio::select! {
            joined = &mut run => joined_result(joined),
            _ = tokio::time::sleep(time) => {
                stop.cancel();
                let ended = match tokio::time::timeout(WRAP_UP, &mut run).await {
                    Ok(joined) => joined_result(joined)?,
                    Err(_) => failure("timeout", String::new()),
                };
                Ok(ran_out_of(time, ended))
            }
        }
    }

    fn require_plugin_functions(&self) -> Result<(), PoolError> {
        let status = self.plugins.status();
        if status != crate::plugins::PluginStatus::Enabled {
            return Err(PoolError::PluginsDisabled {
                status: status.as_str(),
            });
        }
        if runtime::plugin_call::time_limit().is_none() {
            return Err(PoolError::ModuleNotCompiled {
                method: "plugin.call".into(),
                module: "ptc",
            });
        }
        Ok(())
    }

    /// `Err` when `session_id` does not draw on the package `plugin`.
    async fn refuse_left_out(&self, session_id: &str, plugin: &str) -> Result<(), PoolError> {
        if self.plugins_of_session(session_id).await.permits(plugin) {
            return Ok(());
        }
        Err(PoolError::InvalidRequest(format!(
            "session `{session_id}` does not draw on plugin `{plugin}`: its \
             settings.plugins leave the package out, so none of its functions run there"
        )))
    }

    /// `Err` naming the functions there are — for a session, those of the
    /// packages it draws on — when none is `name`.
    async fn function_named(
        &self,
        name: &str,
        target: &PluginCallTarget,
    ) -> Result<PluginFunction, PoolError> {
        let mut functions = self.plugins.functions();
        if let Some(found) = functions.iter().find(|f| f.call_name() == name) {
            return Ok(found.clone());
        }
        if let PluginCallTarget::Session(id) = target {
            functions = self
                .plugins_of_session(id)
                .await
                .retain(functions, |f| &f.plugin);
        }
        let known: Vec<String> = functions.iter().map(PluginFunction::call_name).collect();
        Err(PoolError::InvalidRequest(match known.is_empty() {
            true => format!("there is no plugin function `{name}`: no enabled package has one"),
            false => format!(
                "there is no plugin function `{name}`; there are {}",
                known.join(", ")
            ),
        }))
    }

    /// The engine `id` runs on, brought into memory if it is not, and the
    /// session counted busy with the call until the hold is dropped. A call
    /// runs beside a turn, but not while the session is being rebuilt or
    /// rewound.
    pub(in crate::session_pool) async fn session_runner(
        &self,
        id: &str,
    ) -> Result<(Arc<dyn HostToolRunner>, HostCallHold), PoolError> {
        if !self.session_exists(id).await {
            return Err(PoolError::no_session(id));
        }
        self.with_live_ensured(id, None, |live| {
            if let SessionPhase::Rebuilding | SessionPhase::Exclusive(SessionOp::Rewind) =
                live.turn.phase
            {
                return live.turn.busy(id).map(PoolError::SessionBusy).map(Err);
            }
            let runner = live.engine.host_tool_runner.clone()?;
            Some(Ok((runner, live.core.host_calls().hold())))
        })
        .await?
        .flatten()
        .ok_or_else(|| PoolError::no_session(id))?
    }

    /// One call on an engine of its own in `dir`: the default scene's tools
    /// and the directory's rules, attended so the call itself is the host's
    /// approval, with nobody to ask about the calls it makes. The engine is
    /// built only once the call has its script slot, which bounds how many
    /// of them there are at once; `tool` runs in that slot.
    ///
    /// While the call runs, the engine is answerable under its session id:
    /// the hook calls it puts to host connections go to them as `origin`'s,
    /// and their answers come back to it.
    async fn run_transient(
        &self,
        origin: Arc<PluginCallOrigin>,
        dir: &Path,
        tool: Arc<dyn base::tool::Tool>,
        input: serde_json::Value,
        rules: Vec<PermissionRule>,
        cancel: CancellationToken,
    ) -> Result<PluginCallResult, PoolError> {
        if !dir.is_dir() {
            return Err(PoolError::ProjectNotFound {
                path: dir.to_path_buf(),
            });
        }
        let _slot = tokio::select! {
            slot = self.run_slots().acquire_owned() => slot.map_err(|_| {
                PoolError::Internal("no script can run in this process any more".into())
            })?,
            _ = cancel.cancelled() => {
                return Ok(failure("cancelled", "the call was cancelled before it started".into()));
            }
        };
        let scene = self
            .scenes
            .resolve(None)
            .ok_or_else(|| PoolError::Internal("the pool has no default scene".into()))?;
        let core = SessionCore::new(
            &Id::new().to_string(),
            scene.clone(),
            Some(dir.to_path_buf()).into(),
            Naming::default(),
            time::OffsetDateTime::now_utc(),
        );
        let kept = self
            .live
            .keep_one_call(&core, cancel.clone())
            .map_err(PoolError::Internal)?;
        let mut built = self.build_transient(&core, scene, dir).await?;
        let events = std::mem::replace(
            &mut built.event_rx,
            tokio::sync::mpsc::unbounded_channel().1,
        );
        core.hub().serve_hosts(self.factory.hosts().clone());
        core.hub().answer_host_calls_with(built.input_tx.clone());
        kept.answer_through(built.input_tx.clone());
        built.agent.serve_host_answers();
        let relayed = relay_to_hosts(events, core.hub().clone(), core.id(), origin.clone());
        let runner = built.agent.host_tool_runner();
        let outcome = runner.run(tool, input, rules, cancel).await;
        drop((runner, built, relayed));
        Ok(result_of(&origin.function, outcome))
    }

    /// The engine of a call in no session in `dir`, standing in `core`.
    async fn build_transient(
        &self,
        core: &Arc<SessionCore>,
        scene: Arc<base::interface::scene::SceneSpec>,
        dir: &Path,
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
            mcp_module: None,
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
    origin: Arc<PluginCallOrigin>,
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
                        Some(event) => publish_plugin_call_event(&hub, &session_id, &origin, event),
                        None => return,
                    },
                    _ = done.cancelled() => break,
                }
            }
            while let Ok(event) = events.try_recv() {
                publish_plugin_call_event(&hub, &session_id, &origin, event);
            }
        }
    });
    done.drop_guard()
}

/// How long a call past its time is waited for once told to stop.
const WRAP_UP: Duration = Duration::from_secs(2);

fn joined_result(
    joined: Result<Result<PluginCallResult, PoolError>, tokio::task::JoinError>,
) -> Result<PluginCallResult, PoolError> {
    joined.map_err(|e| PoolError::Internal(format!("the plugin call failed: {e}")))?
}

fn time_of(call: &PluginCall) -> Result<Duration, PoolError> {
    let limit = runtime::plugin_call::time_limit().unwrap_or_default();
    match call.timeout {
        None => Ok(runtime::plugin_call::default_time().unwrap_or_default()),
        Some(t) if t.is_zero() => Err(PoolError::InvalidRequest(
            "a plugin call's timeout is more than 0 ms".into(),
        )),
        Some(t) if t > limit => Err(PoolError::InvalidRequest(format!(
            "a plugin call runs for at most {} ms, not {} ms",
            limit.as_millis(),
            t.as_millis()
        ))),
        Some(t) => Ok(t),
    }
}

fn info_of(f: &PluginFunction) -> PluginFunctionInfo {
    PluginFunctionInfo {
        name: f.call_name(),
        plugin: f.plugin.clone(),
        plugin_version: f.plugin_version.clone(),
        description: f.description.clone(),
        writes: f.writes.clone(),
        input_schema: f.input_schema.clone(),
    }
}

fn failure(code: &str, message: String) -> PluginCallResult {
    PluginCallResult {
        ok: false,
        value: None,
        error: Some(PluginCallError {
            code: code.into(),
            message,
        }),
        logs: Vec::new(),
    }
}

/// The pipeline's answer as the host reads it: the function's own result
/// when it ran, else why the pipeline did not run it. Only the function's
/// tool gives a structured result, so an answer without one was made up or
/// rewritten on the way — by a `tool.around` that answered in its place or a
/// result transformer — and is a failure, never the function's success.
fn result_of(function: &str, outcome: ToolOutcome) -> PluginCallResult {
    match outcome {
        Ok(answer) => match answer.structured {
            Some(structured) => parse_structured(structured),
            None => failure(
                "answered_by_middleware",
                format!(
                    "`{function}`'s own result did not come back: a tool.around script \
                     answered in its place or a result transformer rewrote it"
                ),
            ),
        },
        Err(why) => failure("refused", why),
    }
}

/// A call stopped at its time failed by running out of it, whatever
/// stopped it.
fn ran_out_of(time: Duration, result: PluginCallResult) -> PluginCallResult {
    if result.ok {
        return result;
    }
    PluginCallResult {
        logs: result.logs,
        ..failure(
            "timeout",
            format!("the call ran past its {} ms", time.as_millis()),
        )
    }
}

fn parse_structured(structured: serde_json::Value) -> PluginCallResult {
    let logs = structured["logs"]
        .as_array()
        .map(|lines| {
            lines
                .iter()
                .filter_map(|l| l.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if structured["ok"] == true {
        return PluginCallResult {
            ok: true,
            value: Some(structured["value"].clone()),
            error: None,
            logs,
        };
    }
    let text = |key: &str| {
        structured["error"][key]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    PluginCallResult {
        ok: false,
        value: None,
        error: Some(PluginCallError {
            code: text("code"),
            message: text("message"),
        }),
        logs,
    }
}
