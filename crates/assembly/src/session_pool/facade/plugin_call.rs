//! 宿主调用插件函数（`[[function]]`）：调用作为一次 `PluginCall` 走会话的整条工具
//! 管线。带会话时用那个会话的引擎，不开新一轮、不进对话；不带会话时在给定目录下
//! 建一个临时引擎——不写日志、不进会话表、不连 MCP、不向订阅者发事件，调用结束
//! 就收掉。设计见 docs/runext.md §12。

use crate::session_pool::parts::factory::{Restore, SessionSeed};
use crate::session_pool::parts::live::{Naming, SessionCore};
use crate::session_pool::parts::specs::SessionSpec;
use crate::session_pool::*;
use api::{
    PluginCall, PluginCallError, PluginCallResult, PluginCallTarget, PluginFunctionInfo,
    SessionOptions,
};
use base::command::PluginFunction;
use base::interface::plugin_call::HostToolRunner;
use base::interface::tool_middleware::ToolOutcome;
use base::permission::PermissionRule;

impl SessionPool {
    /// The functions the enabled packages expose to the host. A `target`
    /// given must be there: the session, or the directory.
    pub async fn plugin_functions(
        &self,
        target: Option<&PluginCallTarget>,
    ) -> Result<Vec<PluginFunctionInfo>, PoolError> {
        self.require_plugin_functions()?;
        match target {
            Some(PluginCallTarget::Session(id)) if !self.session_exists(id).await => {
                return Err(PoolError::no_session(id));
            }
            Some(PluginCallTarget::Project(dir)) if !dir.is_dir() => {
                return Err(PoolError::ProjectNotFound { path: dir.clone() });
            }
            _ => {}
        }
        Ok(self.plugins.functions().iter().map(info_of).collect())
    }

    /// Run `call` and hand back what the function returned or why it did
    /// not. `Err` is a call that cannot be made at all: no such function,
    /// session or directory, a time over the limit.
    pub async fn call_plugin(
        &self,
        call: PluginCall,
        cancel: CancellationToken,
    ) -> Result<PluginCallResult, PoolError> {
        self.require_plugin_functions()?;
        let time = time_of(&call)?;
        let function = self.function_named(&call.function)?;
        let tool = runtime::plugin_call::tool(runtime::plugin_call::FunctionCall {
            function,
            writes: call.writes,
            time: Some(time),
            session_id: match &call.target {
                PluginCallTarget::Session(id) => Some(id.clone()),
                PluginCallTarget::Project(_) => None,
            },
            engine: self.layout().engine(),
            run_slots: self.run_slots(),
            host_js_libs: self.factory.host_js_libs().clone(),
        })
        .map_err(PoolError::InvalidRequest)?;
        let input = serde_json::Value::Object(serde_json::Map::from_iter([
            ("function".to_string(), call.function.clone().into()),
            ("input".to_string(), call.input),
        ]));
        let stop = cancel.child_token();
        let run = async {
            match &call.target {
                PluginCallTarget::Session(id) => {
                    let runner = self.session_runner(id).await?;
                    Ok(runner
                        .run(tool, input, call.permission_rules, stop.clone())
                        .await)
                }
                PluginCallTarget::Project(dir) => {
                    self.run_transient(dir, tool, input, call.permission_rules, stop.clone())
                        .await
                }
            }
        };
        tokio::pin!(run);
        tokio::select! {
            outcome = &mut run => Ok(result_of(outcome?, None)),
            _ = tokio::time::sleep(time) => {
                stop.cancel();
                Ok(result_of(run.await?, Some(time)))
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

    fn function_named(&self, name: &str) -> Result<PluginFunction, PoolError> {
        let functions = self.plugins.functions();
        if let Some(found) = functions.iter().find(|f| f.call_name() == name) {
            return Ok(found.clone());
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

    /// The engine `id` runs on, brought into memory if it is not.
    async fn session_runner(&self, id: &str) -> Result<Arc<dyn HostToolRunner>, PoolError> {
        if !self.session_exists(id).await {
            return Err(PoolError::no_session(id));
        }
        self.ensure_live(id, None).await?;
        self.live
            .with_live(id, |live| live.engine.host_tool_runner.clone())
            .await
            .flatten()
            .ok_or_else(|| PoolError::no_session(id))
    }

    /// One call on an engine of its own in `dir`: the default scene's tools
    /// and the directory's rules, attended so the call itself is the host's
    /// approval, with nobody to ask about the calls it makes.
    async fn run_transient(
        &self,
        dir: &Path,
        tool: Arc<dyn base::tool::Tool>,
        input: serde_json::Value,
        rules: Vec<PermissionRule>,
        cancel: CancellationToken,
    ) -> Result<ToolOutcome, PoolError> {
        if !dir.is_dir() {
            return Err(PoolError::ProjectNotFound {
                path: dir.to_path_buf(),
            });
        }
        let scene = self
            .scenes
            .resolve(None)
            .ok_or_else(|| PoolError::Internal("the pool has no default scene".into()))?;
        let binding = self.binding_for(Some(dir), &scene.id).await;
        let sid = Id::new().to_string();
        let core = SessionCore::new(
            &sid,
            scene.clone(),
            Some(dir.to_path_buf()).into(),
            Naming::default(),
            time::OffsetDateTime::now_utc(),
        );
        let _kept = self
            .live
            .keep_one_call(&core, cancel.clone())
            .map_err(PoolError::Internal)?;
        let spec = SessionSpec {
            options: Some(SessionOptions {
                attended: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut built = self.factory.build(BuildRequest {
            session_id: &sid,
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
            packages: self.plugins.for_session(),
            mcp_module: None,
            process_groups: super::create::groups_to_reap(&core, &binding),
            skill_dirs: core.skill_dirs(),
            protected_dirs: self.bindings.host_dirs().0.protected().clone(),
            transient: true,
        })?;
        self.plugins
            .note_script_faults(std::mem::take(&mut built.script_faults));
        built.agent.attach_session();
        let runner = built.agent.host_tool_runner();
        Ok(runner.run(tool, input, rules, cancel).await)
    }
}

fn time_of(call: &PluginCall) -> Result<Duration, PoolError> {
    let limit = runtime::plugin_call::time_limit().unwrap_or_default();
    match call.timeout {
        None => Ok(limit),
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

/// The pipeline's answer as the host reads it: the function's own result
/// when it ran, else why the pipeline did not run it. A call stopped at its
/// time (`timed_out`) failed by running out of it, whatever stopped it.
fn result_of(outcome: ToolOutcome, timed_out: Option<Duration>) -> PluginCallResult {
    let failed = |code: &str, message: String, logs: Vec<String>| PluginCallResult {
        ok: false,
        value: None,
        error: Some(PluginCallError {
            code: code.into(),
            message,
        }),
        logs,
    };
    let mut result = match outcome {
        Ok(answer) => match answer.structured {
            Some(structured) => parse_structured(structured),
            None if answer.reported_failure => failed("refused", answer.text, Vec::new()),
            None => PluginCallResult {
                ok: true,
                value: Some(serde_json::Value::String(answer.text)),
                error: None,
                logs: Vec::new(),
            },
        },
        Err(why) => failed("refused", why, Vec::new()),
    };
    if let (Some(time), false) = (timed_out, result.ok) {
        result = failed(
            "timeout",
            format!("the call ran past its {} ms", time.as_millis()),
            std::mem::take(&mut result.logs),
        );
    }
    result
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
