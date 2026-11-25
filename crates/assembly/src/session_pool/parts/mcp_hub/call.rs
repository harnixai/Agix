//! 宿主不经会话调 MCP 工具：`mcp.tools` 列出一对 (项目, 场景) 或一个会话能调的工具，
//! `mcp.callTool` 用它们已有的连接调一个。调用作为一次 `mcp__<server>__<tool>` 走宿主调用的
//! 那条工具管线（权限、hook、`tool.around`、遥测）：带会话时在会话的引擎上，不带会话时在
//! 只拿到这一个 server 的临时引擎上，不占脚本名额。结果是 server 答的原样；每次有结果的调用
//! 发一条 `mcp_tool_called`，不带参数和结果。

use super::connect::McpServersOf;
use super::*;
use crate::session_pool::facade::host_call::TransientEngine;
use crate::session_pool::parts::specs::SessionSpec;
use crate::session_pool::*;
use api::{
    HostCallOrigin, McpCallOrigin, McpServerState, McpServerTools, McpToolCall, McpToolCallOutcome,
    McpToolCallResult, McpToolCallTarget, McpToolInfo, McpToolList,
};
use mcp::adapter::{McpCallOutcome, McpToolAdapter};
use mcp::client::{McpCallResult, McpContent};

/// Where a host's call or listing finds its servers.
struct McpPlace {
    project_root: Option<PathBuf>,
    scene: Arc<SceneSpec>,
    session_id: Option<String>,
    kept: McpServersOf,
}

impl SessionPool {
    /// The servers of the pair or session and the tools each offers there,
    /// connecting the pair as its first session would. `server` narrows the
    /// list to that one, which must be configured.
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
                        .filter(|a| a.server_name() == status.name)
                        .filter(|a| runtime::scene_permits(&place.scene, a.as_ref()))
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
    /// or why it was not called. `Err` is a call that cannot be made: an
    /// unknown server or tool, a server in no state to be called, a time
    /// over the limit, no such scene, project or session.
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
        let mut one = central.share_where(|name| name == call.server);
        one.refresh_tools().await;
        let adapter = tool_named(&one, &call.server, &call.tool)?;
        let audit = Audit {
            server: call.server.clone(),
            tool: call.tool.clone(),
            tool_name: adapter.full_name().to_string(),
            scene: place.scene.id.clone(),
            project_root: place.project_root.clone(),
            session_id: place.session_id.clone(),
            run_id: call.run_id.clone(),
        };
        let origin = Arc::new(HostCallOrigin::Mcp(McpCallOrigin {
            server: call.server,
            tool: call.tool,
            run_id: call.run_id,
        }));
        let input = serde_json::Value::Object(call.arguments);
        let run = move |pool: Arc<SessionPool>, stop: CancellationToken| async move {
            pool.run_mcp_call(place, one, adapter, input, origin, stop)
                .await
        };
        let result = self
            .within_time(time, &cancel, run, |ended| ran_out(time, ended))
            .await?;
        self.emit_event(audit.event(result.outcome(), started.elapsed()));
        Ok(result)
    }

    async fn mcp_place(&self, target: &McpToolCallTarget) -> Result<McpPlace, PoolError> {
        match target {
            McpToolCallTarget::Pair {
                project_root,
                scene,
            } => {
                self.check_pair(project_root.as_deref(), scene)?;
                let project_root = project_root.clone().unwrap_or_else(|| self.cwd().into());
                let plugins = self.session_plugins(&SessionSpec::default());
                Ok(McpPlace {
                    kept: self
                        .mcp_servers_of(Some(&project_root), scene, &plugins)
                        .await,
                    project_root: Some(project_root),
                    scene: self.scene_spec(scene)?,
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
        place: McpPlace,
        one: McpManager,
        adapter: Arc<McpToolAdapter>,
        input: serde_json::Value,
        origin: Arc<HostCallOrigin>,
        stop: CancellationToken,
    ) -> Result<McpToolCallResult, PoolError> {
        let name = adapter.full_name().to_string();
        let (recording, record) = adapter.recording();
        let tool: Arc<dyn base::tool::Tool> = Arc::new(recording);
        let outcome = match (&place.session_id, &place.project_root) {
            (Some(id), _) => {
                let (runner, _running) = self.session_runner(id).await?;
                hidden(runner.tool(&name).is_none(), &name, &place.scene.id)?;
                runner.run(tool, input, Vec::new(), stop.clone()).await
            }
            (None, Some(dir)) => {
                let engine = TransientEngine {
                    origin,
                    dir,
                    scene: place.scene.clone(),
                    mcp_module: Some(Arc::new(mcp::McpModule::new(one))),
                };
                let scene = place.scene.id.clone();
                let stopped = stop.clone();
                self.on_transient_engine(engine, stop.clone(), |runner| async move {
                    hidden(runner.tool(&name).is_none(), &name, &scene)?;
                    Ok::<_, PoolError>(runner.run(tool, input, Vec::new(), stopped).await)
                })
                .await??
            }
            (None, None) => {
                return Err(PoolError::Internal(
                    "a call in no session has a project".into(),
                ))
            }
        };
        Ok(result_of(outcome, record.take(), stop.is_cancelled()))
    }
}

/// What `mcp_tool_called` says of a call.
struct Audit {
    server: String,
    tool: String,
    tool_name: String,
    scene: String,
    project_root: Option<PathBuf>,
    session_id: Option<String>,
    run_id: Option<String>,
}

impl Audit {
    fn event(self, outcome: McpToolCallOutcome, took: Duration) -> api::DaemonEvent {
        api::DaemonEvent::McpToolCalled {
            server: self.server,
            tool: self.tool,
            tool_name: self.tool_name,
            scene: self.scene,
            project_root: self.project_root,
            session_id: self.session_id,
            run_id: self.run_id,
            outcome,
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
/// as the host reads them. A call that never reached the server was refused
/// on the way — by a rule, the mode, a hook, a `tool.around` that answered
/// in its place — unless it was stopped first.
fn result_of(
    outcome: base::interface::tool_middleware::ToolOutcome,
    recorded: Option<McpCallOutcome>,
    stopped: bool,
) -> McpToolCallResult {
    use McpToolCallOutcome as Code;
    match (recorded, outcome) {
        (Some(McpCallOutcome::Answered(answer)), Ok(_)) => answered(answer),
        (Some(McpCallOutcome::Answered(_)), Err(why)) => failure(Code::Refused, why),
        (Some(McpCallOutcome::TimedOut(why)), _) => failure(Code::Timeout, why),
        (Some(McpCallOutcome::Cancelled), _) => failure(Code::Cancelled, "the call was cancelled"),
        (Some(McpCallOutcome::Failed(why)), _) => failure(Code::Failed, why),
        (None, _) if stopped => failure(
            Code::Cancelled,
            "the call was cancelled before it reached the server",
        ),
        (None, Err(why)) => failure(Code::Refused, why),
        (None, Ok(_)) => failure(
            Code::Refused,
            "the server was not called: a tool.around script answered in its place",
        ),
    }
}

/// A call stopped at its time ran out of it, whatever stopped it.
fn ran_out(time: Duration, ended: Option<McpToolCallResult>) -> McpToolCallResult {
    match ended {
        Some(result) if result.ok => result,
        _ => failure(
            McpToolCallOutcome::Timeout,
            format!("the call ran past its {} ms", time.as_millis()),
        ),
    }
}

fn failure(outcome: McpToolCallOutcome, message: impl Into<String>) -> McpToolCallResult {
    McpToolCallResult {
        ok: false,
        is_error: None,
        content: None,
        text: None,
        structured: None,
        error: Some(api::McpToolCallError {
            code: outcome.as_str().to_string(),
            message: message.into(),
        }),
    }
}

fn answered(answer: McpCallResult) -> McpToolCallResult {
    let text = answer
        .content
        .iter()
        .filter_map(|block| match block {
            McpContent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    McpToolCallResult {
        ok: true,
        is_error: Some(answer.is_error),
        content: Some(answer.content.into_iter().map(block_of).collect()),
        text: Some(text),
        structured: answer.structured,
        error: None,
    }
}

/// A block of a tool result as a message carries it.
#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text { text: String },
    Image { source: Base64 },
}

#[derive(serde::Serialize)]
struct Base64 {
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: String,
    data: String,
}

fn block_of(content: McpContent) -> serde_json::Value {
    let block = match content {
        McpContent::Text(text) => Block::Text { text },
        McpContent::Image { data, media_type } => Block::Image {
            source: Base64 {
                kind: "base64",
                media_type,
                data,
            },
        },
        McpContent::Other(raw) => return raw,
    };
    serde_json::to_value(block).unwrap_or_default()
}
