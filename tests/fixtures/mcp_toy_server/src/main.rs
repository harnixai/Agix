//! 测试用的 MCP 服务 —— `tests/fixtures/template_project` 的 `mcp_servers.demo`
//! 与 `tests/blackbox` 的 MCP 用例连的就是它。不是产品代码，只服务于测试基础设施。
//!
//! 命令行：
//!
//! ```text
//! mcp-toy-server [--transport stdio|http|sse] [--port N] [--port-file PATH]
//!                [--calls PATH] [--name NAME] [--resources DIR] [--mute] [--liar]
//!                [--max-protocol VERSION] [--task]
//! ```
//!
//! - `http` / `sse` 在 127.0.0.1 上起 streamable HTTP，绑定后先在 stdout 第一行打出
//!   `http://127.0.0.1:<port>/mcp`（给了 `--port-file` 也写进那个文件）。rmcp 3 已经
//!   没有旧式 SSE 服务端，Agix 的 `type: "sse"` 客户端也是走 streamable HTTP，
//!   所以 `sse` 起的是同一个服务。
//! - `--calls`（或环境变量 `TOY_MCP_CALLS`）：每次工具调用往这个文件追加一行
//!   `<name>\t<tool>`，`slow` 被取消时另加一行 `<name>\tslow:cancelled`——测试据此证明
//!   调用真的到了这个进程。
//! - `--resources DIR`：目录里每个文件成为一个资源 `toy://files/<文件名>`，测试把
//!   运行时生成的令牌写进去，就能证明模型读到的是资源本身。
//! - `--mute`（只对 stdio）：读 stdin 但从不应答，握手永远完成不了。收到第一行时往
//!   `--calls` 记一行 `<name>\tmuted`，测试据此知道连接已经卡在它身上。
//! - `--liar`：任何没有列出的 URI 也读得出内容 `LIAR-<name>:<uri>`，用来证明不带 server
//!   的资源读取只会问列出了该 URI 的那个 server。
//! - `--max-protocol VERSION`：扮成只认到这个 MCP 版本的老 server，如 `2025-11-25`——
//!   `server/discover` 答"没有这个方法"，客户端回落到 `initialize`，询问改由 server 主动发
//!   `elicitation/create`。
//! - `--task`：server 声明 tasks 扩展，`work` 在客户端也声明了时以任务运行（`tasks/get`
//!   轮询、`tasks/update` 回答、`tasks/cancel` 协作式取消）；没声明或没开 `--task` 时
//!   同步跑完。任务建起记 `work:task <id>`，收到取消记 `work:cancel-requested`，真的停下
//!   记 `work:cancelled`，其余 `work:done` / `work:failed` / `work:sync`。
//!
//! 工具：`ping`、`echo`、`slow`（可取消）、`crash`（进程直接退出）、`big`（指定字节数）、
//! `ask`（向用户问一个问题：`mode` 为 `form` 问颜色、`url` 让人打开一个页面；协商到
//! 2026-07-28 时走 `input_required` 一轮，更早的版本由 server 发 `elicitation/create`，
//! `together` 让老版本 server 等这么多个 `ask` 都到了才问、都拿到回答了才答完）、`ask_twice`（同一轮问两个）、`ask_flood`（一次问 `count` 个；老版本 server 并发地发，`one_by_one` 时一个接一个）。
//! `work`（`steps` 步、每步 `step_ms`，`fail` 让它最后失败、`ask` 在第一步后问一个问题、
//! `code` 原样写进结果文本）。收到的每个回答原样记一行
//! `ask <json>`，并作为结果文本 `answer: <json>` 返回。
//! 资源：`toy://notes/readme.md`、`toy://unicode`、`toy://big`，以及 `--resources` 的。
//! 提示词：`greet`（`name` 必填、`style` 可选）、`mark`（无参数）。

use std::borrow::Cow;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::tool::{InputResponses, ToolCallContext};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams, ContentBlock,
    CreateTaskResult, DiscoverRequestMethod, DiscoverResult, ElicitRequest, ElicitRequestParams,
    GetPromptRequestParams, GetPromptResponse, GetPromptResult, GetTaskParams, GetTaskResult,
    InputRequest, InputRequiredResult, ListPromptsResult, ListResourcesResult,
    PaginatedRequestParams, Prompt, PromptArgument, PromptMessage, ProtocolVersion,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, Role, ServerCapabilities, ServerConfig, UpdateTaskParams,
};
use rmcp::service::RequestContext;
use rmcp::task_manager::{TaskExit, TaskManager, TaskOptions};
use rmcp::{tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler, ServiceExt};
use tokio_util::sync::CancellationToken;

const BIG_RESOURCE_BYTES: usize = 200_000;
const BIG_RESOURCE_TAIL: &str = "TOY-BIG-RESOURCE-END";
const UNICODE_TEXT: &str =
    "统一码 ✓ — emoji 🦀🎉 — RTL مرحبا بالعالم — combining e\u{301} — zero-width a\u{200b}b";

struct Options {
    transport: String,
    port: u16,
    port_file: Option<PathBuf>,
    calls: Option<PathBuf>,
    name: String,
    resources: Option<PathBuf>,
    mute: bool,
    liar: bool,
    max_protocol: Option<ProtocolVersion>,
    task: bool,
}

fn parse_args() -> anyhow::Result<Options> {
    let mut opts = Options {
        transport: "stdio".into(),
        port: 0,
        port_file: None,
        calls: std::env::var_os("TOY_MCP_CALLS").map(PathBuf::from),
        name: "toy".into(),
        resources: None,
        mute: false,
        liar: false,
        max_protocol: None,
        task: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--transport" => opts.transport = value()?,
            "--port" => opts.port = value()?.parse()?,
            "--port-file" => opts.port_file = Some(value()?.into()),
            "--calls" => opts.calls = Some(value()?.into()),
            "--name" => opts.name = value()?,
            "--resources" => opts.resources = Some(value()?.into()),
            "--mute" => opts.mute = true,
            "--liar" => opts.liar = true,
            "--max-protocol" => {
                opts.max_protocol = Some(serde_json::from_value(serde_json::json!(value()?))?)
            }
            "--task" => opts.task = true,
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    anyhow::ensure!(
        matches!(opts.transport.as_str(), "stdio" | "http" | "sse"),
        "--transport is stdio, http or sse, not {}",
        opts.transport
    );
    Ok(opts)
}

struct State {
    name: String,
    calls: Option<PathBuf>,
    resources: Option<PathBuf>,
    liar: bool,
    max_protocol: Option<ProtocolVersion>,
    task: bool,
    tasks: TaskManager,
    asking: tokio::sync::watch::Sender<usize>,
    answered: tokio::sync::watch::Sender<usize>,
}

impl State {
    fn record(&self, what: &str) {
        let Some(path) = &self.calls else {
            return;
        };
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            // One write per line: calls in flight together append at the same
            // moment, and `writeln!` splits a line over writes that interleave.
            let _ = f.write_all(format!("{}\t{what}\n", self.name).as_bytes());
        }
    }

    fn file_resources(&self) -> Vec<(String, PathBuf)> {
        let Some(dir) = &self.resources else {
            return Vec::new();
        };
        let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| {
                (
                    format!("toy://files/{}", e.file_name().to_string_lossy()),
                    e.path(),
                )
            })
            .collect();
        out.sort();
        out
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PingRequest {
    #[schemars(description = "message to echo back")]
    message: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoRequest {
    #[schemars(description = "text returned exactly as given")]
    text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SlowRequest {
    #[schemars(description = "milliseconds to wait before answering")]
    ms: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BigRequest {
    #[schemars(description = "how many bytes of text to return")]
    bytes: usize,
    #[schemars(description = "text the output ends with (default BIG-END)")]
    tail: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AskRequest {
    #[schemars(
        description = "form (pick a color, the default), url (visit a page), long (a message over 4 KiB) or wide (a form of 40 fields)"
    )]
    mode: Option<String>,
    #[schemars(
        description = "for a server that asks itself: ask only once this many asks are in flight, and finish only once all of them are answered"
    )]
    together: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FloodRequest {
    #[schemars(description = "how many questions to put at once")]
    count: usize,
    #[schemars(description = "an older server: put them one after another instead")]
    #[serde(default)]
    one_by_one: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct WorkRequest {
    #[schemars(description = "how many steps (default 3)")]
    steps: Option<u32>,
    #[schemars(description = "milliseconds per step (default 200)")]
    step_ms: Option<u64>,
    #[schemars(description = "finish by failing")]
    fail: Option<bool>,
    #[schemars(description = "ask the user a question after the first step")]
    ask: Option<bool>,
    #[schemars(description = "text carried into the result")]
    code: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct Empty {}

// `#[tool_router]`/`#[tool_handler]` build `Self::tool_router()` per dispatch
// rather than reading a stored router, so the only field is the shared state.
#[derive(Clone)]
struct ToyServer {
    state: Arc<State>,
}

#[tool_router]
impl ToyServer {
    #[tool(description = "Echo the given message back, prefixed with 'toy: '.")]
    fn ping(&self, Parameters(PingRequest { message }): Parameters<PingRequest>) -> String {
        self.state.record("ping");
        format!("toy: {message}")
    }

    #[tool(description = "Return the given text unchanged.")]
    fn echo(&self, Parameters(EchoRequest { text }): Parameters<EchoRequest>) -> String {
        self.state.record("echo");
        text
    }

    #[tool(description = "Wait the given number of milliseconds, then answer. Cancellable.")]
    async fn slow(
        &self,
        Parameters(SlowRequest { ms }): Parameters<SlowRequest>,
        cancel: CancellationToken,
    ) -> String {
        self.state.record("slow");
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(ms)) => {
                format!("slow: done after {ms}ms")
            }
            _ = cancel.cancelled() => {
                self.state.record("slow:cancelled");
                "slow: cancelled".to_string()
            }
        }
    }

    #[tool(
        description = "Work for `steps` x `step_ms` milliseconds and report `work done`. A server \
                       started with --task runs it as a task the client can poll and cancel."
    )]
    async fn work(
        &self,
        Parameters(WorkRequest {
            steps,
            step_ms,
            code,
            ..
        }): Parameters<WorkRequest>,
    ) -> String {
        let (steps, step_ms) = (steps.unwrap_or(3), step_ms.unwrap_or(200));
        tokio::time::sleep(std::time::Duration::from_millis(u64::from(steps) * step_ms)).await;
        self.state.record("work:sync");
        format!("work done: {steps} steps {}", code.unwrap_or_default())
    }

    #[tool(description = "Exit the server process immediately, without answering.")]
    fn crash(&self, Parameters(Empty {}): Parameters<Empty>) -> String {
        self.state.record("crash");
        std::process::exit(3)
    }

    #[tool(description = "Return exactly `bytes` bytes of text ending with `tail`.")]
    fn big(&self, Parameters(BigRequest { bytes, tail }): Parameters<BigRequest>) -> String {
        self.state.record("big");
        sized_text(bytes, tail.as_deref().unwrap_or("BIG-END"))
    }

    #[tool(description = "Ask the user a question and report the answer.")]
    async fn ask(
        &self,
        Parameters(AskRequest { mode, together }): Parameters<AskRequest>,
        InputResponses(responses): InputResponses,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.state.record("ask");
        let asked = [("q", question(mode.as_deref()))];
        let together = together.filter(|_| !is_modern(&context));
        if let Some(n) = together {
            gather(&self.state.asking, n).await;
        }
        let answered = self.put_all(&asked, responses, &context, false).await;
        if let Some(n) = together {
            gather(&self.state.answered, n).await;
        }
        answered
    }

    #[tool(description = "Ask the user two questions at once and report both answers.")]
    async fn ask_twice(
        &self,
        Parameters(Empty {}): Parameters<Empty>,
        InputResponses(responses): InputResponses,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.state.record("ask_twice");
        let asked = [
            ("q1", question(Some("form"))),
            ("q2", question(Some("form"))),
        ];
        self.put_all(&asked, responses, &context, false).await
    }

    #[tool(
        description = "Ask the user `count` questions at once (an older server sends them all \
                       concurrently) and report every answer."
    )]
    async fn ask_flood(
        &self,
        Parameters(FloodRequest { count, one_by_one }): Parameters<FloodRequest>,
        InputResponses(responses): InputResponses,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.state.record("ask_flood");
        let keys: Vec<String> = (0..count).map(|i| format!("q{i}")).collect();
        let asked: Vec<(&str, ElicitRequestParams)> = keys
            .iter()
            .map(|key| (key.as_str(), question(Some("form"))))
            .collect();
        self.put_all(&asked, responses, &context, !one_by_one).await
    }
}

/// What `ask` puts to the user in each mode.
fn question(mode: Option<&str>) -> ElicitRequestParams {
    let wire = match mode {
        Some("url") => serde_json::json!({
            "mode": "url",
            "message": "Sign in to the toy service",
            "url": "https://auth.example.com/toy/sign-in",
            "elicitationId": "toy-sign-in",
        }),
        Some("long") => serde_json::json!({
            "mode": "form",
            "message": "Pick a color. ".repeat(1000),
            "requestedSchema": {
                "type": "object",
                "properties": { "color": { "type": "string", "enum": ["red", "blue"] } },
            },
        }),
        Some("wide") => serde_json::json!({
            "mode": "form",
            "message": "Fill in everything",
            "requestedSchema": {
                "type": "object",
                "properties": (0..40)
                    .map(|i| (format!("field{i}"), serde_json::json!({ "type": "string" })))
                    .collect::<serde_json::Map<_, _>>(),
            },
        }),
        _ => serde_json::json!({
            "mode": "form",
            "message": "Pick a color",
            "requestedSchema": {
                "type": "object",
                "properties": { "color": { "type": "string", "enum": ["red", "blue"] } },
                "required": ["color"],
            },
        }),
    };
    serde_json::from_value(wire).unwrap_or_else(|e| panic!("toy question: {e}"))
}

impl ToyServer {
    /// `work` as a task the client polls: one status message per step.
    fn work_as_task(&self, args: WorkRequest) -> CreateTaskResult {
        let state = self.state.clone();
        let steps = args.steps.unwrap_or(3);
        let step_ms = args.step_ms.unwrap_or(200);
        let code = args.code.unwrap_or_default();
        let (fail, ask) = (args.fail.unwrap_or(false), args.ask.unwrap_or(false));
        let options = TaskOptions::new()
            .with_poll_interval_ms(100)
            .with_ttl_ms(120_000)
            .with_status_message(format!("step 0/{steps}"));
        let task = self.state.tasks.spawn(options, {
            let state = state.clone();
            move |cx| {
                Box::pin(async move {
                    for i in 1..=steps {
                        tokio::select! {
                            () = cx.cancelled() => {
                                state.record("work:cancelled");
                                return Err(TaskExit::Cancelled);
                            }
                            () = tokio::time::sleep(std::time::Duration::from_millis(step_ms)) => {}
                        }
                        cx.set_status_message(format!("step {i}/{steps}"));
                        if ask && i == 1 {
                            let request = ElicitRequest::new(question(Some("form")));
                            let answer = match cx
                                .request_input("color", InputRequest::Elicitation(request))
                                .await
                            {
                                Ok(answer) => answer,
                                Err(exit) => {
                                    state.record("work:cancelled");
                                    return Err(exit);
                                }
                            };
                            state.record(&format!("ask {answer}"));
                        }
                    }
                    if fail {
                        state.record("work:failed");
                        return Err(TaskExit::Error(ErrorData::internal_error(
                            "work failed on purpose",
                            None,
                        )));
                    }
                    state.record("work:done");
                    Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                        "work done: {steps} steps {code}"
                    ))]))
                })
            }
        });
        self.state.record(&format!("work:task {}", task.task_id));
        CreateTaskResult::new(task)
    }

    /// Ask `asked` the way the negotiated protocol allows, and answer with
    /// what came back.
    async fn put_all(
        &self,
        asked: &[(&str, ElicitRequestParams)],
        responses: Option<rmcp::model::InputResponses>,
        context: &RequestContext<RoleServer>,
        concurrent: bool,
    ) -> Result<CallToolResponse, ErrorData> {
        let modern = is_modern(context);
        let answers: Vec<serde_json::Value> = if modern {
            let Some(responses) = responses else {
                let requests = asked
                    .iter()
                    .map(|(key, params)| {
                        let request = ElicitRequest::new(params.clone());
                        (key.to_string(), InputRequest::Elicitation(request))
                    })
                    .collect();
                return Ok(InputRequiredResult::from_input_requests(requests).into());
            };
            asked
                .iter()
                .map(|(key, _)| responses.get(*key).cloned().unwrap_or_default())
                .collect()
        } else {
            let mut answers = Vec::new();
            if concurrent {
                let mut pending = tokio::task::JoinSet::new();
                for (i, (_, params)) in asked.iter().enumerate() {
                    let (peer, params) = (context.peer.clone(), params.clone());
                    pending.spawn(async move { (i, peer.create_elicitation(params).await) });
                }
                while let Some(joined) = pending.join_next().await {
                    let (i, result) =
                        joined.map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
                    let result = result
                        .map(|r| serde_json::to_value(r).unwrap_or_default())
                        .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
                    answers.push((i, result));
                }
                answers.sort_by_key(|(i, _)| *i);
            } else {
                for (i, (_, params)) in asked.iter().enumerate() {
                    let result = context
                        .peer
                        .create_elicitation(params.clone())
                        .await
                        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
                    answers.push((i, serde_json::to_value(result).unwrap_or_default()));
                }
            }
            answers.into_iter().map(|(_, a)| a).collect()
        };
        let text = answers
            .iter()
            .map(|answer| {
                self.state.record(&format!("ask {answer}"));
                format!("answer: {answer}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
    }
}

fn is_modern(context: &RequestContext<RoleServer>) -> bool {
    context
        .protocol_version()
        .is_some_and(|v| !v.has_initialize())
}

/// Count this call in at `point` and wait until `n` calls have been.
async fn gather(point: &tokio::sync::watch::Sender<usize>, n: usize) {
    point.send_modify(|arrived| *arrived += 1);
    let _ = point.subscribe().wait_for(|arrived| *arrived >= n).await;
}

#[tool_handler]
impl ServerHandler for ToyServer {
    fn get_info(&self) -> ServerConfig {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_prompts();
        let capabilities = if self.state.task {
            capabilities.enable_tasks()
        } else {
            capabilities
        };
        let info = ServerConfig::new(capabilities.build()).with_instructions(
            "Test-only MCP server: tools, resources and prompts for the harness.",
        );
        match &self.state.max_protocol {
            Some(max) => info.with_protocol_version(max.clone()),
            None => info,
        }
    }

    /// With `--max-protocol`, the newest version is listed too: rmcp turns a
    /// probe at a version not listed down before `discover` below sees it,
    /// with a version list rather than the "method not found" an old server
    /// answers.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        match &self.state.max_protocol {
            Some(max) => {
                let mut known = ProtocolVersion::known_up_to(max).to_vec();
                known.push(ProtocolVersion::LATEST);
                Cow::Owned(known)
            }
            None => Cow::Borrowed(ProtocolVersion::KNOWN_VERSIONS),
        }
    }

    /// A server from before 2026-07-28 has never heard of `server/discover`.
    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        match &self.state.max_protocol {
            Some(max) if max.has_initialize() => {
                Err(ErrorData::method_not_found::<DiscoverRequestMethod>())
            }
            _ => Ok(DiscoverResult::from_server_info(
                self.supported_protocol_versions().into_owned(),
                self.get_info(),
            )),
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let client_runs_tasks = context
            .client_capabilities()
            .is_some_and(|caps| caps.supports_tasks());
        if self.state.task && client_runs_tasks && request.name == "work" {
            let args = request
                .arguments
                .map(serde_json::Value::Object)
                .map(serde_json::from_value::<WorkRequest>)
                .transpose()
                .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?
                .unwrap_or(WorkRequest {
                    steps: None,
                    step_ms: None,
                    fail: None,
                    ask: None,
                    code: None,
                });
            return Ok(CallToolResponse::Task(self.work_as_task(args)));
        }
        Self::tool_router()
            .call(ToolCallContext::new(self, request, context))
            .await
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, ErrorData> {
        Ok(GetTaskResult::new(
            self.state.tasks.get_task(&request.task_id)?,
        ))
    }

    async fn update_task(
        &self,
        request: UpdateTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.state
            .tasks
            .update_task(&request.task_id, request.input_responses)
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.state.record("work:cancel-requested");
        self.state.tasks.cancel_task(&request.task_id)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let mut resources = vec![
            Resource::new("toy://notes/readme.md", "readme")
                .with_description("A short note")
                .with_mime_type("text/markdown"),
            Resource::new("toy://unicode", "unicode")
                .with_description("Non-ASCII text: CJK, emoji, right-to-left, combining marks")
                .with_mime_type("text/plain"),
            Resource::new("toy://big", "big")
                .with_description(format!("{BIG_RESOURCE_BYTES} bytes of text"))
                .with_mime_type("text/plain")
                .with_size(BIG_RESOURCE_BYTES as u64),
        ];
        for (uri, path) in self.state.file_resources() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            resources.push(Resource::new(uri, name).with_mime_type("text/plain"));
        }
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri;
        let text = match uri.as_str() {
            "toy://notes/readme.md" => "# Toy notes\n\nTOY-NOTES-MARK\n".to_string(),
            "toy://unicode" => UNICODE_TEXT.to_string(),
            "toy://big" => sized_text(BIG_RESOURCE_BYTES, BIG_RESOURCE_TAIL),
            other => match self
                .state
                .file_resources()
                .into_iter()
                .find(|(u, _)| u == other)
            {
                Some((_, path)) => std::fs::read_to_string(path)
                    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?,
                None if self.state.liar => format!("LIAR-{}:{other}", self.state.name),
                None => {
                    return Err(ErrorData::resource_not_found(
                        format!("no resource {other}"),
                        None,
                    ))
                }
            },
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, uri)]).into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(vec![
            Prompt::new(
                "greet",
                Some("Greet someone by name"),
                Some(vec![
                    PromptArgument::new("name")
                        .with_description("who to greet")
                        .with_required(true),
                    PromptArgument::new("style")
                        .with_description("how to greet them")
                        .with_required(false),
                ]),
            ),
            Prompt::new("mark", Some("A fixed instruction"), None),
        ]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        let arg = |key: &str| {
            request
                .arguments
                .as_ref()
                .and_then(|a| a.get(key))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let text = match request.name.as_str() {
            "greet" => {
                let name = arg("name").ok_or_else(|| {
                    ErrorData::invalid_params("greet needs the `name` argument", None)
                })?;
                match arg("style") {
                    Some(style) => format!("Greet {name} in a {style} style."),
                    None => format!("Greet {name}."),
                }
            }
            "mark" => "Reply with exactly TOY-PROMPT-MARK.".to_string(),
            other => {
                return Err(ErrorData::invalid_params(
                    format!("no prompt {other}"),
                    None,
                ))
            }
        };
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)]).into())
    }
}

/// ASCII text of exactly `bytes` bytes (at least the tail) ending with `tail`.
fn sized_text(bytes: usize, tail: &str) -> String {
    let fill = bytes.saturating_sub(tail.len());
    let mut out: String = "0123456789abcdef\n".chars().cycle().take(fill).collect();
    out.push_str(tail);
    out
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = parse_args()?;
    let state = Arc::new(State {
        name: opts.name.clone(),
        calls: opts.calls.clone(),
        resources: opts.resources.clone(),
        liar: opts.liar,
        max_protocol: opts.max_protocol.clone(),
        task: opts.task,
        tasks: TaskManager::new(),
        asking: tokio::sync::watch::Sender::new(0),
        answered: tokio::sync::watch::Sender::new(0),
    });
    if opts.mute {
        return stay_mute(&state).await;
    }
    if opts.transport == "stdio" {
        let service = ToyServer { state }.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        return Ok(());
    }
    serve_http(opts, state).await
}

async fn stay_mute(state: &State) -> anyhow::Result<()> {
    use tokio::io::AsyncBufReadExt as _;
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut told = false;
    while lines.next_line().await?.is_some() {
        if !told {
            state.record("muted");
            told = true;
        }
    }
    Ok(())
}

async fn serve_http(opts: Options, state: Arc<State>) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", opts.port)).await?;
    let url = format!("http://127.0.0.1:{}/mcp", listener.local_addr()?.port());
    if let Some(path) = &opts.port_file {
        std::fs::write(path, &url)?;
    }
    println!("{url}");
    std::io::stdout().flush()?;

    let service = StreamableHttpService::new(
        move || {
            Ok(ToyServer {
                state: state.clone(),
            })
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    loop {
        let (sock, _) = listener.accept().await?;
        let service = hyper_util::service::TowerToHyperService::new(service.clone());
        tokio::spawn(async move {
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(sock), service)
                .await;
        });
    }
}
