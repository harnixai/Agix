//! 宿主不经会话调 MCP 工具：`mcp.tools` 列出一对 (项目, 场景) 或一个会话能调的工具，
//! `mcp.callTool` 用它们已有的连接调一个。调用作为一次 `mcp__<server>__<tool>` 走宿主调用的
//! 那条工具管线（权限、hook、`tool.around`、`tool.result`、遥测）：带会话时在会话的引擎上，
//! 不带会话时在只拿到这一个 server 的临时引擎上，不占脚本名额。交给宿主的只有 server 的原样
//! 答复，管线里有谁替它作答或改了它，就答 `answered_by_middleware`；每次有结果的调用发一条
//! `mcp_tool_called`，不带参数和结果。

use super::connect::McpServersOf;
use super::*;
use crate::session_pool::facade::host_call::TransientEngine;
use crate::session_pool::parts::specs::SessionSpec;
use crate::session_pool::*;
use api::{
    HostCallOrigin, McpCallOrigin, McpServerState, McpServerTools, McpToolCall, McpToolCallResult,
    McpToolCallTarget, McpToolInfo, McpToolList,
};
use base::interface::plugin_call::{HostCallOutcome, HostToolRunner};
use mcp::adapter::{McpCallOutcome, McpCallRecord, McpToolAdapter};
use mcp::client::McpCallResult;
use std::sync::OnceLock;

const REFUSED: &str = "refused";
const ANSWERED_BY_MIDDLEWARE: &str = "answered_by_middleware";
const TIMEOUT: &str = "timeout";
const CANCELLED: &str = "cancelled";
const FAILED: &str = "failed";

/// Where a host's call or listing finds its servers.
struct McpPlace {
    project_root: Option<PathBuf>,
    scene: Arc<SceneSpec>,
    session_id: Option<String>,
    kept: McpServersOf,
}

/// One call on its way to the pipeline: the server's tools are looked up
/// inside the call's time, where `mcp.cancel` reaches it too.
struct McpCallRun {
    place: McpPlace,
    one: McpManager,
    server: String,
    tool: String,
    input: serde_json::Value,
    origin: Arc<HostCallOrigin>,
    record: McpCallRecord,
    named: Arc<OnceLock<String>>,
}

impl SessionPool {
    /// The servers of the pair or session and the tools each offers there,
    /// connecting the pair as its first session would. `server` narrows the
    /// list to that one, which must be configured. A session's list is what
    /// that session could call.
    pub async fn mcp_tools(
        &self,
        server: Option<&str>,
        target: &McpToolCallTarget,
    ) -> Result<McpToolList, PoolError> {
        let place = self.mcp_place(target).await?;
        let central = self
            .mcp_for_project(place.project_root.as_deref(), &place.scene.id)
            .await;
        let statuses = self.statuses_at(&place, &central).await;
        if let Some(name) = server {
            self.listed_server(&place, &statuses, name)?;
        }
        let runner = match &place.session_id {
            Some(id) => Some(self.session_runner(id).await?.0),
            None => None,
        };
        let offered = |adapter: &McpToolAdapter| match &runner {
            Some(runner) => runner.tool(adapter.full_name()).is_some(),
            None => runtime::scene_permits(&place.scene, adapter),
        };
        let wanted = |name: &str| place.kept.gets(name) && server.is_none_or(|n| n == name);
        let mut view = central.share_where(wanted);
        view.refresh_tools().await;
        let mut servers: Vec<McpServerTools> = statuses
            .into_iter()
            .filter(|status| wanted(&status.name))
            .map(|status| {
                let tools = match status.state {
                    McpServerState::Connected | McpServerState::Disconnected => view
                        .adapters()
                        .iter()
                        .filter(|a| a.server_name() == status.name && offered(a))
                        .map(|a| tool_info(a))
                        .collect(),
                    _ => Vec::new(),
                };
                McpServerTools {
                    name: status.name,
                    state: status.state,
                    tools,
                    error: status.error,
                }
            })
            .collect();
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(McpToolList { servers })
    }

    /// Call one tool of a connected server and hand back what it answered,
    /// or why it did not. `Err` is a call that cannot be made: an unknown
    /// server or tool, a server in no state to be called, a time over the
    /// limit, no such scene, project or session.
    pub async fn call_mcp_tool(
        &self,
        call: McpToolCall,
        cancel: CancellationToken,
    ) -> Result<McpToolCallResult, PoolError> {
        let started = Instant::now();
        let time = mcp_call_time(call.timeout)?;
        let place = self.mcp_place(&call.target).await?;
        let central = self
            .mcp_for_project(place.project_root.as_deref(), &place.scene.id)
            .await;
        let statuses = self.statuses_at(&place, &central).await;
        callable(self.listed_server(&place, &statuses, &call.server)?)?;
        let audit = Audit {
            tool_name: Arc::default(),
            server: call.server.clone(),
            tool: call.tool.clone(),
            scene: place.scene.id.clone(),
            project_root: place.project_root.clone(),
            session_id: place.session_id.clone(),
            run_id: call.run_id.clone(),
        };
        let record = McpCallRecord::default();
        let run = McpCallRun {
            one: central.share_where(|name| name == call.server),
            place,
            input: serde_json::Value::Object(call.arguments),
            origin: Arc::new(HostCallOrigin::Mcp(McpCallOrigin {
                server: call.server.clone(),
                tool: call.tool.clone(),
                run_id: call.run_id,
            })),
            server: call.server,
            tool: call.tool,
            record: record.clone(),
            named: audit.tool_name.clone(),
        };
        let result = self
            .within_time(
                time,
                &cancel,
                move |pool: Arc<SessionPool>, stop| pool.run_mcp_call(run, stop),
                |ended| ran_out(time, ended, record.reached_server()),
            )
            .await?;
        self.emit_event(audit.event(&result, started.elapsed()));
        Ok(result)
    }

    async fn mcp_place(&self, target: &McpToolCallTarget) -> Result<McpPlace, PoolError> {
        match target {
            McpToolCallTarget::Pair {
                project_root,
                scene,
            } => {
                let scene = self.active_scene(scene)?;
                self.check_pair(project_root.as_deref(), &scene.id)?;
                let project_root = project_root.clone().unwrap_or_else(|| self.cwd().into());
                let plugins = self.session_plugins(&SessionSpec::default());
                Ok(McpPlace {
                    kept: self
                        .mcp_servers_of(Some(&project_root), &scene.id, &plugins)
                        .await,
                    project_root: Some(project_root),
                    scene,
                    session_id: None,
                })
            }
            McpToolCallTarget::Session(id) => {
                if !self.session_exists(id).await {
                    return Err(PoolError::no_session(id));
                }
                let (project_root, scene) = self.pair_of_session(id).await?;
                let plugins = self.plugins_of_session(id).await;
                Ok(McpPlace {
                    kept: self
                        .mcp_servers_of(project_root.as_deref(), &scene, &plugins)
                        .await,
                    project_root,
                    scene: self.scene_spec(&scene)?,
                    session_id: Some(id.clone()),
                })
            }
        }
    }

    /// `scene` among the scenes a session can be made in now, as
    /// `session.create` finds it.
    fn active_scene(&self, scene: &str) -> Result<Arc<SceneSpec>, PoolError> {
        self.scenes
            .resolve(Some(scene))
            .ok_or_else(|| PoolError::SceneNotActive {
                scene: scene.to_string(),
            })
    }

    fn scene_spec(&self, scene: &str) -> Result<Arc<SceneSpec>, PoolError> {
        self.scenes
            .registry()
            .resolve(scene)
            .ok_or_else(|| PoolError::SceneNotFound {
                scene: scene.to_string(),
            })
    }

    async fn statuses_at(
        &self,
        place: &McpPlace,
        central: &McpManager,
    ) -> Vec<api::McpServerStatus> {
        let settings = self
            .settings_for_project(place.project_root.as_deref(), &place.scene.id)
            .await;
        self.connected_mcp_entries(central, &settings)
    }

    /// `name`'s entry among `statuses`, which a session must draw on.
    fn listed_server<'a>(
        &self,
        place: &McpPlace,
        statuses: &'a [api::McpServerStatus],
        name: &str,
    ) -> Result<&'a api::McpServerStatus, PoolError> {
        let Some(status) = statuses.iter().find(|s| s.name == name) else {
            let mut known: Vec<&str> = statuses
                .iter()
                .map(|s| s.name.as_str())
                .filter(|n| place.kept.gets(n))
                .collect();
            known.sort_unstable();
            return Err(PoolError::InvalidRequest(match known.is_empty() {
                true => format!("there is no MCP server `{name}`: none is configured here"),
                false => format!(
                    "there is no MCP server `{name}`; there are {}",
                    known.join(", ")
                ),
            }));
        };
        if let (Some(session), false) = (&place.session_id, place.kept.gets(name)) {
            return Err(PoolError::InvalidRequest(format!(
                "session `{session}` does not get MCP server `{name}`: its settings.plugins \
                 leave out the package that declares it"
            )));
        }
        Ok(status)
    }

    async fn run_mcp_call(
        self: Arc<Self>,
        run: McpCallRun,
        stop: CancellationToken,
    ) -> Result<McpToolCallResult, PoolError> {
        let McpCallRun {
            place,
            mut one,
            server,
            tool,
            input,
            origin,
            record,
            named,
        } = run;
        tokio::select! {
            biased;
            _ = stop.cancelled() => {
                return Ok(failure(CANCELLED, false, "the call was cancelled before it reached the server"));
            }
            _ = one.refresh_tools() => {}
        }
        let adapter = tool_named(&one, &server, &tool)?;
        let name = adapter.full_name().to_string();
        let _ = named.set(name.clone());
        let tool: Arc<dyn base::tool::Tool> = Arc::new(adapter.recording_into(record.clone()));
        let on = |runner: Arc<dyn HostToolRunner>, stop: CancellationToken| {
            let (name, scene) = (name.clone(), place.scene.id.clone());
            async move {
                hidden(runner.tool(&name).is_none(), &name, &scene)?;
                Ok::<_, PoolError>(runner.run(tool, input, Vec::new(), stop).await)
            }
        };
        let outcome = match (&place.session_id, &place.project_root) {
            (Some(id), _) => {
                let (runner, _running) = self.session_runner(id).await?;
                on(runner, stop.clone()).await?
            }
            (None, Some(dir)) => {
                let engine = TransientEngine {
                    origin,
                    dir,
                    scene: place.scene.clone(),
                    mcp_module: Some(Arc::new(mcp::McpModule::new(one))),
                };
                let stopped = stop.clone();
                self.on_transient_engine(engine, stop.clone(), |runner| on(runner, stopped))
                    .await??
            }
            (None, None) => {
                return Err(PoolError::Internal(
                    "a call in no session has a project".into(),
                ))
            }
        };
        Ok(result_of(outcome, &record, stop.is_cancelled()))
    }
}

/// What `mcp_tool_called` says of a call.
struct Audit {
    /// The adapter's name once the tool is found; `mcp__<server>__<tool>` as
    /// naming makes it, if the call ends before.
    tool_name: Arc<OnceLock<String>>,
    server: String,
    tool: String,
    scene: String,
    project_root: Option<PathBuf>,
    session_id: Option<String>,
    run_id: Option<String>,
}

impl Audit {
    fn event(self, result: &McpToolCallResult, took: Duration) -> api::DaemonEvent {
        let tool_name = match self.tool_name.get() {
            Some(name) => name.clone(),
            None => mcp::naming::tool_name(&self.server, &self.tool),
        };
        api::DaemonEvent::McpToolCalled {
            server: self.server,
            tool: self.tool,
            tool_name,
            scene: self.scene,
            project_root: self.project_root,
            session_id: self.session_id,
            run_id: self.run_id,
            outcome: result.outcome().to_string(),
            reached_server: result.reached_server(),
            duration_ms: took.as_millis() as u64,
        }
    }
}

fn mcp_call_time(asked: Option<Duration>) -> Result<Duration, PoolError> {
    let limit = api::MCP_CALL_TIME_LIMIT;
    match asked {
        None => Ok(api::MCP_CALL_DEFAULT_TIME),
        Some(t) if t.is_zero() => Err(PoolError::InvalidRequest(
            "an MCP call's timeout is more than 0 ms".into(),
        )),
        Some(t) if t > limit => Err(PoolError::InvalidRequest(format!(
            "an MCP call runs for at most {} ms, not {} ms",
            limit.as_millis(),
            t.as_millis()
        ))),
        Some(t) => Ok(t),
    }
}

/// `Err` unless `status` is a server a call can go to: one connected, or
/// one whose connection dropped, which the call reconnects.
fn callable(status: &api::McpServerStatus) -> Result<(), PoolError> {
    match status.state {
        McpServerState::Connected | McpServerState::Disconnected => Ok(()),
        state => Err(PoolError::McpServerUnavailable {
            server: status.name.clone(),
            state,
            error: status.error.clone(),
        }),
    }
}

fn tool_named(
    manager: &McpManager,
    server: &str,
    tool: &str,
) -> Result<Arc<McpToolAdapter>, PoolError> {
    let offered: Vec<&Arc<McpToolAdapter>> = manager
        .adapters()
        .iter()
        .filter(|a| a.server_name() == server)
        .collect();
    if let Some(found) = offered.iter().find(|a| a.tool_name() == tool) {
        return Ok(Arc::clone(found));
    }
    let mut names: Vec<&str> = offered.iter().map(|a| a.tool_name()).collect();
    names.sort_unstable();
    Err(PoolError::InvalidRequest(match names.is_empty() {
        true => format!("MCP server `{server}` has no tool `{tool}`: it offers none"),
        false => format!(
            "MCP server `{server}` has no tool `{tool}`; it has {}",
            names.join(", ")
        ),
    }))
}

fn hidden(is_hidden: bool, name: &str, scene: &str) -> Result<(), PoolError> {
    if !is_hidden {
        return Ok(());
    }
    Err(PoolError::InvalidRequest(format!(
        "`{name}` is not a tool of scene `{scene}`: the scene's tools leave it out"
    )))
}

fn tool_info(adapter: &McpToolAdapter) -> McpToolInfo {
    let hints = adapter.hints();
    McpToolInfo {
        name: adapter.tool_name().to_string(),
        description: adapter.full_description().to_string(),
        input_schema: adapter.input_schema_value().clone(),
        output_schema: adapter.output_schema().cloned(),
        annotations: api::McpToolAnnotations {
            read_only: hints.read_only,
            destructive: hints.destructive,
            idempotent: hints.idempotent,
            open_world: hints.open_world,
        },
        tool_name: adapter.full_name().to_string(),
    }
}

/// The pipeline's answer and what the server's side of the call came to,
/// as the host reads them. Only the server's own answer, untouched on the
/// way, is a result; an answer anything else gave or changed is
/// `answered_by_middleware`, whatever the server said. A call that came to
/// no answer is the server's timeout or failure when the server's side
/// ended that way, `cancelled` when it was told to stop, and `refused`
/// otherwise — a rule, the mode or a hook stopped it, before the server
/// heard of it or after it answered.
fn result_of(host: HostCallOutcome, record: &McpCallRecord, stopped: bool) -> McpToolCallResult {
    let reached = record.reached_server();
    match (record.take(), host.outcome) {
        (Some(McpCallOutcome::Answered(answer)), Ok(_)) if !host.by_middleware => answered(answer),
        (_, Ok(_)) => failure(
            ANSWERED_BY_MIDDLEWARE,
            reached,
            "the server's answer did not come back as it gave it: a tool.around script answered \
             in its place or changed it, or a tool.result transformer rewrote it",
        ),
        (Some(McpCallOutcome::TimedOut(why)), Err(_)) => failure(TIMEOUT, reached, why),
        (Some(McpCallOutcome::Cancelled), Err(_)) => {
            failure(CANCELLED, reached, "the call was cancelled")
        }
        (Some(McpCallOutcome::Failed(why)), Err(_)) => failure(FAILED, reached, why),
        (None, Err(_)) if stopped || host.stopped => failure(
            CANCELLED,
            reached,
            "the call was cancelled before it reached the server",
        ),
        (_, Err(why)) => failure(REFUSED, reached, why),
    }
}

/// A call stopped at its time ran out of it, whatever stopped it.
fn ran_out(time: Duration, ended: Option<McpToolCallResult>, reached: bool) -> McpToolCallResult {
    match ended {
        Some(result @ McpToolCallResult::Answered(_)) => result,
        _ => failure(
            TIMEOUT,
            reached,
            format!("the call ran past its {} ms", time.as_millis()),
        ),
    }
}

fn failure(code: &str, reached_server: bool, message: impl Into<String>) -> McpToolCallResult {
    McpToolCallResult::NotAnswered(api::McpToolCallFailure {
        ok: false,
        error: api::McpToolCallError {
            code: code.to_string(),
            message: message.into(),
        },
        reached_server,
    })
}

fn answered(answer: McpCallResult) -> McpToolCallResult {
    let text = answer
        .raw_content
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    McpToolCallResult::Answered(api::McpToolAnswer {
        ok: true,
        is_error: answer.is_error,
        content: answer.raw_content,
        text,
        structured: answer.structured,
    })
}
