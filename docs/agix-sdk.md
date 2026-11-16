# 把 Agix 嵌进自己的进程

这份指南给把引擎直接嵌进自己进程的 Rust 宿主看：依赖 `agix` 一个 crate，装出一个会话池，建会话、跑一轮、收事件、回答引擎的询问、关停。只想在自己进程里起引擎、然后经协议说话的宿主直接看 §10 的 `agix::embed`：它是 agixd 本身的启动路径，0.x 内单独承诺稳定。通过 RPC 使用引擎的看 `daemon_rpc_protocol.md`；每个扩展点的契约与触发时机看 `extension_points.md`；引擎怎么组织的看 `ARCHITECTURE.md`。

本文里每个 Rust 片段都是 `agix` crate 的 doctest（`cargo test -p agix --doc`），以 `# ` 开头的行是 doctest 的准备代码（临时目录、运行时、模型的脚本），GitHub 上也会显示出来，读正文代码即可。模型这一端用进程内的 mock 客户端，不联网、不花 token；生产里把 `model_client` 留成 `None`。

## 1. 选 crate 与 feature

只依赖 `agix` 一个 crate。同时直接依赖 `assembly`、`runtime` 等会让两套路径并存，feature 合并出意外的档位，不要这样做。`agix` 的导出按主题分模块，每个模块的文档第一句说它是什么、什么时候需要；跑起一个池、一个会话、一轮的最小集合在 `agix::prelude`。

```toml
# 缺省：coding + scripts + plugin-packages + sandbox，与 agixd 的缺省相同，只少 WebSocket 监听
[dependencies]
agix  = { path = "agix/crates/agix" }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal"] }

# 只要内核，再自选模块
# agix = { path = "agix/crates/agix", default-features = false, features = ["mcp", "scripts"] }
```

| feature | 含义 |
|---|---|
| 缺省 | `coding` + `scripts` + `plugin-packages` + `sandbox` |
| `coding` | `mcp` `agents` `skills` `web` `structured-output` `ptc` |
| `--no-default-features` | minimal：只有内核 |
| `otel` | 另开：把遥测发给 OTLP 收集器 |

没有 `ws` 与 `full`：WebSocket 监听是 agixd 的。设置里写了未编入模块的段（如 minimal 下的 `mcp_servers`）启动时警告并忽略；`scripts` 段在没编 `scripts` 时记 error（`ARCHITECTURE.md` §1.2）。`agix::process::identity().modules` 报出这个构建编入了哪些模块。

两条路：**会话池**（本指南；agixd 用的就是它，设置分层、provider 路由、历史布局、重建、驱逐、恢复都在）与**单个 Agent**（`agix::modules::Builder` 直接 `build()`，例子在 `crates/runtime/examples/minimal.rs`，以上这些都得自己管）。下游现在都走会话池。

依赖名提醒：宿主自己若已有名为 `agix` 的 feature 或 `mod agix`（Harnix-ext 的 `crates/engine` 与 IPDrun 的 `crates/docs` 就有），feature 写成 `agix = ["dep:agix", …]`；与本地 `mod agix` 同在一个 crate 时用 `::agix::`，或改依赖名：`agix-sdk = { package = "agix", … }`。

## 2. 装配一个池

设置从哪来：`load_daemon_config(model, max_tokens, socket, scope, &paths)` 按全局（`global_root/settings.json`）、场景（`config_root/settings.json`）、项目（`project_root/.agix/settings.json`）三层合并；`model` 与 `max_tokens` 是这些层都没写时的兜底；`socket` 只在挂 Unix 监听时才用，传 `None`。托管设置：`config.managed_settings_path` 指定的文件必须存在，否则读平台路径（macOS 是 `/Library/Application Support/Agix/managed-settings.json`）。

状态目录：`DefaultDaemonPaths::from_env(scope)` 是 `$AGIX_CONFIG_HOME` 或 `$HOME/.agix`，场景目录 `scenes/<scope>/`；嵌入方自带目录用 `StaticDaemonPaths::with_project(config_root, project_root).with_global(global_root)`。会话日志放哪由 `Assembly::transcripts` 定：`Transcripts::UnderGlobalRoot`（缺省）、`In(store)`、`Nowhere`。

provider：`Assembly::model_client` 为 `None` 时按 `ANTHROPIC_AUTH_TOKEN` 或 `ANTHROPIC_API_KEY`（加 `ANTHROPIC_BASE_URL`）建 Anthropic 客户端，环境里没有就向 `Assembly::credentials` 要 provider `"default"` 的密钥。设置里的 `providers`、`default_provider`、`task_models` 在 `assemble` 时校验，配错直接失败，不静默回退。

模块清单不由嵌入方传：由编入的 feature 决定。要加 Rust 能力用 `Assembly::plugins`（§6）。`Assembly` 其余字段：

- `cwd`：缺省项目；`scenes` 与 `extra_scenes`：自定义场景与要额外启用的场景名。
- `permission_prompt_timeout`：缺省读设置 `execution.permission_prompt_timeout_secs`，再缺省 300 秒，超时拒绝。
- `host_policy`：它的 deny 在任何模式下都成立。
- `customize_session`：每次建或重建会话都应用；`mcp`：MCP 的登录存储、OAuth 客户端与策略。

`assemble` 返回的池已经加载插件、在后台连 MCP、起了 janitor、配置热重载与定时任务，不要再调 `start_*`。一个进程可以有多个池；例外是 `otel` 的导出器是进程级的。

```rust
# use std::sync::Arc;
use agix::prelude::*;

# fn main() -> anyhow::Result<()> {
# tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
# let dir = tempfile::tempdir()?;
# let (home, work) = (dir.path().join("home"), dir.path().join("work"));
# std::fs::create_dir_all(&home)?;
# std::fs::create_dir_all(&work)?;
# let mock = Arc::new(model::mock::MockAnthropicClient::new());
let paths = StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
let mut config = load_daemon_config("claude-sonnet-4-6", 8192, None, "code", &paths);
// 记忆选取会另调一次模型，会吃掉 mock 脚本里的一条回答
# config.settings.values.memory_enabled = false;

let scene = Arc::new(builtin("code").expect("built-in scene"));
let pool = assemble(
    &config,
    scene,
    Assembly {
        cwd: Some(work.clone()),
        // 生产里留 None
        model_client: Some(mock.clone()),
        ..Default::default()
    },
)
.await?;

pool.shutdown_all().await;
# Ok::<(), anyhow::Error>(())
# })
# }
```

## 3. 一个会话，一轮

`create_session(scene, ProjectSelector, Option<&SessionOptions>)` 建会话。`ProjectSelector` 有三态：`Default`（缺省项目）、`NoProject`（无项目）、`Path(p)`（指定且必须存在）；要求项目的场景配 `NoProject` 会被拒。`SessionOptions` 在建会话时取下、随会话持久化，之后每次重建都照它，后续调用带的 options 被忽略。

`run_turn(TurnRequest, Option<Arc<dyn SessionWatcher>>)` 跑一轮：

- `session_id` 为 `None` 时顺带建一个会话。
- `on_busy` 是 `Reject`、`Queue`、`Steer` 之一，决定会话正忙时怎么办。
- `initiator` 写 `Initiator::Client`。`turn_id` 自己生成且唯一，**不要以 `goal-`、`schedule-`、`cron-` 开头**：那是目标轮与定时任务的形式，这一轮会被当成自动轮、能做的事受限；RPC 层会拒绝这种 id，池不替你查。
- 结局看返回值：`TurnOutcome` 是 `#[non_exhaustive]`（`Completed`、`Steered`、`Skipped`），`match` 要留 `_`；出错是 `PoolError`（`SessionBusy`、`Engine(TurnFailure)` 等）。传进来的 watcher 在这一轮开始时加入订阅，本轮事件照常收到，结局不另发 error 帧。

关一个会话用 `shutdown_session(id)`。下面的片段只用 `use agix::prelude::*;`，验证 prelude 够用。

```rust
# use std::sync::Arc;
use agix::prelude::*;

# fn main() -> anyhow::Result<()> {
# tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
# let dir = tempfile::tempdir()?;
# let (home, work) = (dir.path().join("home"), dir.path().join("work"));
# std::fs::create_dir_all(&home)?;
# std::fs::create_dir_all(&work)?;
# let mock = Arc::new(model::mock::MockAnthropicClient::new());
# mock.push_text("你好。");
# let paths = StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
# let mut config = load_daemon_config("claude-sonnet-4-6", 8192, None, "code", &paths);
# config.settings.values.memory_enabled = false;
# let scene = Arc::new(builtin("code").expect("built-in scene"));
# let pool = assemble(&config, scene, Assembly { cwd: Some(work.clone()), model_client: Some(mock.clone()), ..Default::default() }).await?;
let created = pool.create_session(None, ProjectSelector::Default, None).await?;
let outcome = pool
    .run_turn(
        TurnRequest {
            session_id: Some(created.session_id.clone()),
            message: "你好".into(),
            attachments: vec![],
            turn_id: "turn-1".into(),
            options: None,
            on_busy: OnBusy::Reject,
            initiator: Initiator::Client,
        },
        None,
    )
    .await?;
assert!(matches!(outcome, TurnOutcome::Completed { .. }));
# pool.shutdown_all().await;
# Ok::<(), anyhow::Error>(())
# })
# }
```

## 4. 事件与询问回答

事件通过实现 `SessionWatcher` 交给宿主。`deliver` 在发布事件的线程上同步调用，有时还持着池的锁：**不能阻塞，也不能等池**。标准做法是把帧丢进一个 channel，由宿主自己的任务读出来，再调池的方法；返回 `false` 即退订。

`WatcherId` 由宿主选，和同一个池上 `DaemonServer` 的连接 id 共用一个空间（连接 id 从小往上数）：同时用两者时从高位取，片段里用 `u64::MAX`。

`subscribe_session(id, watcher)` 回 `Subscribed { last_seq, pending, .. }`：**先订阅、再读历史到 `last_seq`**，反过来会丢中间的帧；`pending` 是还没回答的询问。`subscribe_resuming` 会先把不在内存的会话恢复起来。轮与轮之间只有询问、宿主工具调用、子 Agent 进度、通知这类事件会到（`emitted_outside_turns`）。

引擎向人提三种问，回答都走 `respond_to_prompt(session_id, prompt_id, PromptAnswer)`：

| 事件 | 回答 |
|---|---|
| `AgentEvent::PermissionPrompt` | `PromptAnswer::Permission(PermissionDecision::Permit` 或 `Deny { reason }` 或 `PermitAlways { scope })` |
| `AgentEvent::QuestionPrompt`（`AskUserQuestion`） | `PromptAnswer::Question(QuestionAnswer::Answer { text }` 或 `Decline { reason })` |
| `AgentEvent::McpElicitationPrompt` | `PromptAnswer::McpElicitation(McpElicitationAnswer::Accept { content }` 或 `Decline` 或 `Cancel)`；`Accept` 的内容按服务器的 `requested_schema` 校验，不合返回 `PoolError::AnswerOutsideSchema` |

权限询问超时（§2）按拒绝处理。回答的种类和询问不符被拒（`InvalidRequest`）；`prompt_id` 已失效（答过、超时）是静默无操作。

```rust
# use std::sync::Arc;
use agix::events::{AgentEvent, SessionEvent, SessionFrame, SessionWatcher, WatcherId};
use agix::prelude::*;
use agix::prompts::{McpElicitationAnswer, PermissionDecision, PromptAnswer, QuestionAnswer};
use tokio::sync::mpsc::UnboundedSender;

struct Frames(UnboundedSender<SessionFrame>);

impl SessionWatcher for Frames {
    fn id(&self) -> WatcherId {
        WatcherId(u64::MAX)
    }

    fn deliver(&self, frame: &SessionFrame) -> bool {
        self.0.send(frame.clone()).is_ok()
    }
}

fn answer(event: &AgentEvent) -> Option<(String, PromptAnswer)> {
    Some(match event {
        AgentEvent::PermissionPrompt { prompt_id, .. } => (
            prompt_id.clone(),
            PromptAnswer::Permission(PermissionDecision::Permit),
        ),
        AgentEvent::QuestionPrompt { prompt_id, .. } => (
            prompt_id.clone(),
            PromptAnswer::Question(QuestionAnswer::Decline {
                reason: "nobody to ask".into(),
            }),
        ),
        AgentEvent::McpElicitationPrompt { prompt_id, .. } => (
            prompt_id.clone(),
            PromptAnswer::McpElicitation(McpElicitationAnswer::Decline),
        ),
        _ => return None,
    })
}

# fn main() -> anyhow::Result<()> {
# tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
# let dir = tempfile::tempdir()?;
# let (home, work) = (dir.path().join("home"), dir.path().join("work"));
# std::fs::create_dir_all(&home)?;
# std::fs::create_dir_all(&work)?;
# let mock = Arc::new(model::mock::MockAnthropicClient::new());
# mock.push_text("你好。");
# let paths = StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
# let mut config = load_daemon_config("claude-sonnet-4-6", 8192, None, "code", &paths);
# config.settings.values.memory_enabled = false;
# let scene = Arc::new(builtin("code").expect("built-in scene"));
# let pool = assemble(&config, scene, Assembly { cwd: Some(work.clone()), model_client: Some(mock.clone()), ..Default::default() }).await?;
let created = pool.create_session(None, ProjectSelector::Default, None).await?;
let session_id = created.session_id;

let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
let turn = tokio::spawn({
    let pool = pool.clone();
    let session_id = session_id.clone();
    async move {
        pool.run_turn(
            TurnRequest {
                session_id: Some(session_id),
                message: "你好".into(),
                attachments: vec![],
                turn_id: "turn-1".into(),
                options: None,
                on_busy: OnBusy::Reject,
                initiator: Initiator::Client,
            },
            Some(Arc::new(Frames(tx))),
        )
        .await
    }
});

let mut said = String::new();
while let Some(frame) = rx.recv().await {
    let SessionEvent::Agent(event) = &frame.event else {
        continue;
    };
    match event {
        AgentEvent::TextDelta { text, .. } => said.push_str(text),
        AgentEvent::TurnComplete { .. } | AgentEvent::Error { .. } => break,
        other => {
            if let Some((prompt_id, reply)) = answer(other) {
                pool.respond_to_prompt(&frame.session_id, prompt_id, reply).await?;
            }
        }
    }
}
assert!(matches!(turn.await??, TurnOutcome::Completed { .. }));
assert_eq!(said, "你好。");
# pool.shutdown_all().await;
# Ok::<(), anyhow::Error>(())
# })
# }
```

## 5. 宿主工具

宿主自己执行、以会话工具的身份交给模型的工具，在 `SessionOptions::host_tools` 里声明，调用来了用 `respond_tool_call` 回答。声明的约束：

- `name` 匹配 `^[A-Za-z][A-Za-z0-9_-]{0,63}$`，不以 `mcp__` 开头；`description` 不超过 8 KiB；`input_schema` 是 `type: object`，不超过 64 KiB。
- `timeout_ms` 取 1000 到 600000，缺省 120000；另有 `read_only`、`destructive`、`concurrency_safe`、`deferred`、`subagents`。

调用到来是 `AgentEvent::HostToolCall { call_id, name, input, timeout_ms, .. }`；回答是 `HostReply::Result(ToolResult)`（结果本身可以带 `is_error`）或 `HostReply::Failed(message)`（宿主跑不了这次调用）。宿主工具和别的工具一样过权限门，所以非只读的工具先有一个 `PermissionPrompt`，答过才轮到 `HostToolCall`。没人回答的调用在超时、**所有 watcher 离开**或这一轮被取消时结束：回答期间 watcher 要一直挂着。

工具也可以挂在 watcher 上而不是会话上：`pool.set_host_tools(watcher, entries)` 把这个 watcher 提供的工具登记进去（每项同 `options.host_tools`，整个替换、`[]` 撤回），此后建起来的每个会话都带它们；它们不进会话选项。这样的调用的 `HostToolCall` 一定发给提供它的 watcher，答复只认它：用 `pool.respond_tool_call_from(Some(watcher_id), &session_id, call_id, reply)` 回答（`respond_tool_call` 等于 `from = None`，答不了这类调用）。报进度用 `pool.report_tool_progress(Some(watcher_id), &session_id, call_id, update)`。`pool.set_hooks(watcher, entries)` 是同一个思路的 hook 版本（只发给登记它的 watcher、对已建好的会话立刻生效），影响面和失败语义见 `daemon_rpc_protocol.md` §6.1 的 `daemon.setHooks`。watcher 离开，它的工具与 hook 随即消失。

```rust
# use std::sync::Arc;
use agix::contracts::tools::ToolResult;
use agix::events::{AgentEvent, SessionEvent, SessionFrame, SessionWatcher, WatcherId};
use agix::host_tools::{HostReply, HostToolOption};
use agix::prelude::*;
use agix::prompts::{PermissionDecision, PromptAnswer};
use agix::session::SessionOptions;

# struct Frames(tokio::sync::mpsc::UnboundedSender<SessionFrame>);
# impl SessionWatcher for Frames {
#     fn id(&self) -> WatcherId { WatcherId(u64::MAX) }
#     fn deliver(&self, frame: &SessionFrame) -> bool { self.0.send(frame.clone()).is_ok() }
# }
# fn main() -> anyhow::Result<()> {
# tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
# let dir = tempfile::tempdir()?;
# let (home, work) = (dir.path().join("home"), dir.path().join("work"));
# std::fs::create_dir_all(&home)?;
# std::fs::create_dir_all(&work)?;
# let mock = Arc::new(model::mock::MockAnthropicClient::new());
# mock.push_tool_use("toolu_1", "lookup", serde_json::json!({ "key": "answer" }));
# mock.push_text("查到了");
# let paths = StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
# let mut config = load_daemon_config("claude-sonnet-4-6", 8192, None, "code", &paths);
# config.settings.values.memory_enabled = false;
# let scene = Arc::new(builtin("code").expect("built-in scene"));
# let pool = assemble(&config, scene, Assembly { cwd: Some(work.clone()), model_client: Some(mock.clone()), ..Default::default() }).await?;
let options = SessionOptions {
    host_tools: vec![HostToolOption {
        name: "lookup".into(),
        description: "Look a key up in the host's own store.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "key": { "type": "string" } },
            "required": ["key"]
        }),
        read_only: false,
        destructive: false,
        concurrency_safe: None,
        timeout_ms: 120_000,
        deferred: false,
        subagents: false,
    }],
    ..Default::default()
};
let created = pool
    .create_session(None, ProjectSelector::Default, Some(&options))
    .await?;

let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
let turn = tokio::spawn({
    let pool = pool.clone();
    let session_id = created.session_id.clone();
    async move {
        pool.run_turn(
            TurnRequest {
                session_id: Some(session_id),
                message: "查一下 answer".into(),
                attachments: vec![],
                turn_id: "turn-1".into(),
                options: None,
                on_busy: OnBusy::Reject,
                initiator: Initiator::Client,
            },
            Some(Arc::new(Frames(tx))),
        )
        .await
    }
});

let mut answered_the_call = false;
while let Some(frame) = rx.recv().await {
    let SessionEvent::Agent(event) = &frame.event else {
        continue;
    };
    match event {
        AgentEvent::PermissionPrompt { prompt_id, .. } => {
            let permit = PromptAnswer::Permission(PermissionDecision::Permit);
            pool.respond_to_prompt(&frame.session_id, prompt_id.clone(), permit).await?;
        }
        AgentEvent::HostToolCall { call_id, name, input, .. } => {
            assert_eq!(name, "lookup");
            assert_eq!(input["key"], "answer");
            let reply = HostReply::Result(ToolResult::text("42"));
            pool.respond_tool_call(&frame.session_id, call_id.clone(), reply).await?;
            answered_the_call = true;
        }
        AgentEvent::TurnComplete { .. } | AgentEvent::Error { .. } => break,
        _ => {}
    }
}
assert!(answered_the_call);
assert!(matches!(turn.await??, TurnOutcome::Completed { .. }));
# pool.shutdown_all().await;
# Ok::<(), anyhow::Error>(())
# })
# }
```

## 6. 模块、插件、契约替换

`Plugin`（`name` 加 `register(&PluginSession, &mut Registrar)`）经 `Assembly::plugins` 进入池建的**每一个**会话，子 Agent 也在内，每次重建再注册一次。`Registrar` 能加工具、agent 类型、工具与提示中间件、结果变换、模型拦截、提示块与变量、组装钩子、健康检查。Rust 插件和 QuickJS 脚本落在同一个点时 Rust 先（包裹时在内），与注册先后无关。

`SessionCustomizer`（`Arc<dyn Fn(Builder, &SessionCtx) -> Builder>`）是对每次建引擎的最后一句话：执行层（`agix::contracts::exec`）、`token_counter`、`prompt_assembler`、`rule_provider` 这些契约在这里换。**在池里换 `exec` 要求设置 `sandbox.reap_process_groups` 为 `false`**（缺省就是）：它开着时池给会话设了 `process_groups`，与 `exec` 同设会让 `build()` 报错（`ARCHITECTURE.md` §1.1 注入口表）。

`Module` 与 `SessionContext` 在 `agix::modules`：池的模块清单由编入的 feature 决定，嵌入方一般用不到；自己用 `Builder` 装单个会话时才写。`HostContracts` 是子 Agent 整份继承的那一套（`ARCHITECTURE.md` §3.4）。

全部契约与每个点的触发时机、信任级别见 `docs/extension_points.md`（那张表由 `base::interface::catalog` 生成，有测试对账），这里不重抄；它们在门面里的路径是 `agix::contracts` 的各个子模块与 `agix::modules`。

```rust
# use std::sync::Arc;
use agix::contracts::exec::ExecProviders;
use agix::contracts::tools::{PathStyle, ProgressSender, Tool, ToolContext, ToolError, ToolResult, ToolSpec};
use agix::modules::{Plugin, PluginSession, Registrar};
use agix::prelude::*;
use serde_json::{json, Value};

struct Echo;

#[async_trait::async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "Echo"
    }

    fn description(&self) -> &str {
        "Return the text it was given."
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new().read_only(true).concurrency_safe(true)
    }

    fn input_schema(&self, _style: PathStyle) -> Value {
        json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        })
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text(input["text"].as_str().unwrap_or_default()))
    }
}

struct EchoPlugin;

impl Plugin for EchoPlugin {
    fn name(&self) -> &str {
        "echo"
    }

    fn register(&self, _session: &PluginSession<'_>, registrar: &mut Registrar) {
        registrar.tool(Arc::new(Echo));
    }
}

# fn main() -> anyhow::Result<()> {
# tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
# let dir = tempfile::tempdir()?;
# let (home, work) = (dir.path().join("home"), dir.path().join("work"));
# std::fs::create_dir_all(&home)?;
# std::fs::create_dir_all(&work)?;
# let mock = Arc::new(model::mock::MockAnthropicClient::new());
# mock.push_text("你好。");
# let paths = StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
# let mut config = load_daemon_config("claude-sonnet-4-6", 8192, None, "code", &paths);
# config.settings.values.memory_enabled = false;
# let scene = Arc::new(builtin("code").expect("built-in scene"));
let pool = assemble(
    &config,
    scene,
    Assembly {
        cwd: Some(work.clone()),
        # model_client: Some(mock.clone()),
        plugins: vec![Arc::new(EchoPlugin)],
        customize_session: Some(Arc::new(|builder, _ctx| builder.exec(ExecProviders::in_process()))),
        ..Default::default()
    },
)
.await?;

let outcome = pool
    .run_turn(
        TurnRequest {
            session_id: None,
            message: "你好".into(),
            attachments: vec![],
            turn_id: "turn-1".into(),
            options: None,
            on_busy: OnBusy::Reject,
            initiator: Initiator::Client,
        },
        None,
    )
    .await?;
assert!(matches!(outcome, TurnOutcome::Completed { .. }));
# let first = mock.nth_request(0).expect("the model was asked");
# assert!(format!("{:?}", first.tools).contains("Echo"));
# pool.shutdown_all().await;
# Ok::<(), anyhow::Error>(())
# })
# }
```

## 7. 关停、崩溃与进程级职责

agixd 替自己做的进程级的事，嵌入方哪些也要做、哪些不用，对照如下：

| 职责 | agixd 怎么做（`daemon/src/main.rs` 与它下面的 `agix::embed`） | 嵌入方 | 用什么 |
|---|---|---|---|
| 异步运行时 | 多线程，`--worker-threads` | **必须**多线程 | 你的 `tokio::runtime::Builder::new_multi_thread()` |
| 日志过滤 | `tracing_subscriber` 加 `log_filter(RUST_LOG)` | **必须**（只要装了 tracing subscriber）：rmcp 在 `debug` 下会写出 OAuth 授权码、令牌和表单答复 | `agix::process::log_filter` |
| panic 钩子 | `crash::install(~/.agix/daemon/crashes, "agixd", 版本)` | **建议**：`panic = "abort"` 的构建里它先杀掉命令的进程组再写崩溃记录，不装则 abort 时命令留在机器上；一个进程装一次 | `agix::process::crash::install(dir, 程序名, 版本)` |
| 进程组回收 | `Embedded::start` 拿到实例锁后 `reap_previous(groups/<实例>.json, 500 ms)`，池建好后 `persist_to` 同一路径；`shutdown` 删掉该文件 | **建议**（会跑 Bash 或 MCP stdio 的部署）：路径每个实例唯一；`reap_previous` 会跳过拥有者仍活着的记录；它会阻塞到宽限期，放在运行时之外或 `spawn_blocking` | `agix::process::orphans::{reap_previous, persist_to}`，或 `agix::embed`（给 `instance`） |
| 信号 | SIGINT 或 SIGTERM 触发 `Embedded::shutdown()`；第二次直接退出 130 | **必须**：收到时 `pool.shutdown_all()`，套一个上限（agixd 是 10 秒） | 你的信号处理加 `SessionPool::shutdown_all` |
| 关停顺序 | 停止监听、删 token 文件；`shutdown_all`（至多 10 秒）；删 groups 文件、删实例文件、放锁；最后 otel 收尾（至多 10 秒） | 照前两步，再删 groups 文件 | `shutdown_all`；开了 `otel` 就必须调 `agix::process::shutdown_process_exporter`，否则最后一批 span 丢 |
| 配置热重载、janitor、定时任务、MCP 后台连接、插件加载 | `assemble::pool` 里起 | **不用做** | 无 |
| 会话日志的写者锁 | 引擎自己对 `sessions/<id>/writer.lock` 持 flock | **不用做** | 无 |
| 实例锁 `daemon.lock` | `Embedded::start` 拿，持有到 `shutdown` | 可不做：只在你也给 agix 客户端开 socket 时需要 | `agix::embed`（给 `instance`），门面不单独收 |
| 实例发现 `instances.d` | `serve_unix` 绑好 socket 后写，`shutdown` 删 | 可不做：只有想让 agix 的客户端按场景找到你才做 | `agix::embed`（给 `instance`），门面不单独收 |
| 监听与认证 | `Embedded::serve_unix`；TCP / WebSocket 与 token 在 `assembly::agixd` | 可不做：进程内用 `Embedded::serve_stream`（§10）、`agix::rpc`（§8）或直接调池 | `agix::embed`、`agix::rpc::DaemonServer` |
| 导入探测、启动计时 | `Embedded::start` 里做 | 可不做 | `agix::embed` |

门面不收实例锁与实例发现：做这两件事等于在重写 agixd 的监听面。要它们的宿主用 §10 的 `agix::embed`：`EmbedConfig.instance` 给了名字，实例锁、实例发现、进程组的回收与登记、导入探测、启动计时都由它照 agixd 的做法做掉，表里剩给嵌入方的只有运行时、日志、panic 钩子、信号与 otel 收尾。

`release` profile 是嵌入方自己的。`panic = "abort"` 时一个工具 panic 会带走整个进程（`ARCHITECTURE.md` §1.3），`crash::install` 的行为也随之不同：只有 abort 构建才杀进程组。

下面这个 `main` 不运行（`no_run`），只守住它能编译。`otel` 那一步写在这里的文字里：开了 `otel` 的构建，在 `shutdown_all` 之后用 `spawn_blocking` 调 `agix::process::shutdown_process_exporter`。

```rust,no_run
# use std::sync::Arc;
use std::time::Duration;

use agix::prelude::*;
use agix::process::log_filter;
use agix::process::orphans::{persist_to, reap_previous};

# #[allow(unused_variables)]
fn main() -> anyhow::Result<()> {
    let state = std::env::temp_dir().join("my-host");
    agix::process::crash::install(state.join("crashes"), "my-host", env!("CARGO_PKG_VERSION"));

    let groups = state.join("groups").join("default.json");
    reap_previous(&groups, Duration::from_millis(500));
    persist_to(groups.clone());

    tracing_subscriber::fmt()
        .with_env_filter(log_filter(std::env::var("RUST_LOG").ok().as_deref()))
        .init();

    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
# let paths = StaticDaemonPaths::new(state.clone());
# let config = load_daemon_config("claude-sonnet-4-6", 8192, None, "code", &paths);
# let scene = Arc::new(builtin("code").expect("built-in scene"));
# let pool = assemble(&config, scene, Assembly::default()).await?;
        tokio::signal::ctrl_c().await?;
        let _ = tokio::time::timeout(Duration::from_secs(10), pool.shutdown_all()).await;
        let _ = std::fs::remove_file(&groups);
        Ok(())
    })
}
```

## 8. 与 RPC 用法对照

同一个池也有 JSON-RPC 面（`daemon_rpc_protocol.md`）。同一件事两边各怎么写：

| 事 | RPC | Rust |
|---|---|---|
| 建会话 | `session.create` | `pool.create_session(scene, ProjectSelector, Option<&SessionOptions>)` |
| 跑一轮 | `session.run_turn` | `pool.run_turn(TurnRequest { initiator: Initiator::Client, .. }, watcher)` |
| 收事件 | `session.subscribe` 加 `session.event` 通知 | `pool.subscribe_session(id, watcher)`，帧是 `SessionFrame`（线上形状是它的投影） |
| 回答询问 | `session.respondToPrompt` | `pool.respond_to_prompt(id, prompt_id, PromptAnswer)` |
| 回答宿主工具 | `session.respondToolCall` | `pool.respond_tool_call(id, call_id, HostReply)`；连接提供的工具用 `respond_tool_call_from(Some(watcher_id), ..)` |
| 压缩 | `session.compact { instructions?, at? }` | `run_turn` 发 `CompactParams { .. }.command()?` 生成的 `/compact …` |
| 关会话 | `session.close` | `pool.shutdown_session(id)` |
| 关停 | `daemon.shutdown` | `pool.shutdown_all()` 加 §7 |

RPC 层多做、池不替你做的检查：认证；`turn_id` 的形式（§3）；消息与附件都为空时拒绝；`options` 里未知字段的告警；宿主工具回答里图片的 `media_type` 与大小（`ImageSource::check`）：直接给 `ToolResultBlock::image` 不经这道检查。其余方法在 `SessionPool` 上多半同名（见 rustdoc）；它们的参数类型门面没收的，需要时到 `openatta/Agix` 提 issue。

进程内说 RPC：`DaemonServer::new(pool, CancellationToken)`，`accept_connection(sink)`（`sink` 是实现 `FrameSink` 的帧出口），`dispatch_public(RpcRequest, client)`，用完 `drop_connection(client.id())`。进程内分派不走认证，只给与池同进程的代码用。不必亲手装池、只要一条进程内协议连接的，用 §10 的 `Embedded::serve_stream`，回复与事件走同一条有序的流。

```rust
# use std::sync::Arc;
use agix::prelude::*;
use agix::rpc::{DaemonServer, FrameSink, RpcRequest};
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

struct Frames(UnboundedSender<String>);

#[async_trait::async_trait]
impl FrameSink for Frames {
    async fn send_json(&self, json: String) -> bool {
        self.0.send(json).is_ok()
    }
}

# fn main() -> anyhow::Result<()> {
# tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
# let dir = tempfile::tempdir()?;
# let (home, work) = (dir.path().join("home"), dir.path().join("work"));
# std::fs::create_dir_all(&home)?;
# std::fs::create_dir_all(&work)?;
# let mock = Arc::new(model::mock::MockAnthropicClient::new());
# mock.push_text("经 RPC。");
# mock.push_text("经 Rust。");
# let paths = StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
# let mut config = load_daemon_config("claude-sonnet-4-6", 8192, None, "code", &paths);
# config.settings.values.memory_enabled = false;
# let scene = Arc::new(builtin("code").expect("built-in scene"));
# let pool = assemble(&config, scene, Assembly { cwd: Some(work.clone()), model_client: Some(mock.clone()), ..Default::default() }).await?;
let server = DaemonServer::new(pool.clone(), CancellationToken::new());
let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
let client = server.accept_connection(Arc::new(Frames(tx)));
let request = |id: i64, method: &str, params| RpcRequest {
    jsonrpc: "2.0".into(),
    method: method.into(),
    params,
    id: Some(json!(id)),
};

let created = server
    .dispatch_public(request(1, "session.create", json!({})), client.clone())
    .await;
let session_id = created.result.expect("session.create answers")["session_id"]
    .as_str()
    .expect("a session id")
    .to_string();
let ran = server
    .dispatch_public(
        request(2, "session.run_turn", json!({
            "session_id": session_id, "message": "你好", "turn_id": "rpc-1"
        })),
        client.clone(),
    )
    .await;
assert!(ran.error.is_none(), "{:?}", ran.error);
while let Some(frame) = rx.recv().await {
    if frame.contains("turn_complete") {
        break;
    }
}
server.drop_connection(client.id()).await;

let created = pool.create_session(None, ProjectSelector::Default, None).await?;
let outcome = pool
    .run_turn(
        TurnRequest {
            session_id: Some(created.session_id),
            message: "你好".into(),
            attachments: vec![],
            turn_id: "turn-1".into(),
            options: None,
            on_busy: OnBusy::Reject,
            initiator: Initiator::Client,
        },
        None,
    )
    .await?;
assert!(matches!(outcome, TurnOutcome::Completed { .. }));
# pool.shutdown_all().await;
# Ok::<(), anyhow::Error>(())
# })
# }
```

## 9. 版本与兼容

版本号是 `x.y.z-YYYYMMDDNN`（`Cargo.toml` 的 `[workspace.package] version`）；每次推送到远程就是一次发布，带附注标签 `Agix-v<x.y.z>-<YYYYMMDDNN>`。`agix::process::identity()` 报出版本与编入的模块。

**不做兼容。** 没有 `#[deprecated]` 的过渡期（`api_surface` 守卫 A8），改名与删除在同一个版本里改掉仓库内的调用方。门面让这件事对下游可见：门面导出的东西被删或改名，升级时就是编译错误；行为变化写在标签消息里。唯一的例外是 §10 的 `agix::embed`：0.x 内只加不改，改动写进 §10 的迁移说明。

升级：把子模块挪到新标签，编译，读两个标签之间每条标签消息（门面有变化的发布在消息里单列「嵌入面」）。

门面里没有的东西：到 `openatta/Agix` 提 issue（标签是项目名加 `enhancement`，标题 `[<项目> <编号>] …`），说明哪段代码要它。门面按需增长，不预先收：进门面的每一条都对应一个下游用法或本指南的一个片段，导出总数钉在 `tests/guards/tests/facade.rs`，加一条删一条都要在提交里写依据。

## 10. 嵌入入口：`agix::embed`

前九节把池当库用：装配的每一步都看得见，`Assembly` 的字段随版本变。只想要「agixd 的参数进、协议连接出」的宿主用 `agix::embed`（也在 `daemon::embed`）：agixd 自己就从这里起，出现的只有 `EmbedConfig`、`EmbedError`、`Embedded` 三个类型，宿主之后只经协议（`daemon_rpc_protocol.md`）用引擎。

```rust
use agix::embed::{EmbedConfig, Embedded};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

# fn main() -> anyhow::Result<()> {
# tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
# let dir = tempfile::tempdir()?;
# let work = dir.path().join("work");
# std::fs::create_dir_all(&work)?;
let mut config = EmbedConfig::new(dir.path().join("state"));
config.cwd = Some(work);
let engine = Embedded::start(config).await?;

let (ours, theirs) = tokio::io::duplex(1 << 20);
engine.serve_stream(theirs);
let (read, mut write) = tokio::io::split(ours);
write
    .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"daemon.ping\"}\n")
    .await?;
let mut line = String::new();
BufReader::new(read).read_line(&mut line).await?;
let pong: serde_json::Value = serde_json::from_str(&line)?;
assert!(pong["result"]["capabilities"].is_array());

engine.shutdown().await;
# Ok::<(), anyhow::Error>(())
# })
# }
```

**`EmbedConfig`** 就是 agixd 的命令行：`EmbedConfig::new(data_root)` 给出与 agixd 相同的缺省，再改字段（`#[non_exhaustive]`，不能用结构体字面量建）。

| 字段 | agixd 的参数 | 缺省 |
|---|---|---|
| `data_root` | `AGIX_CONFIG_HOME`（否则 `~/.agix`） | 必给 |
| `cwd` | 进程的工作目录 | `None`：本进程的工作目录 |
| `scene` / `scenes` | `--scene` / `--scenes` | `code` / 无 |
| `model` / `max_tokens` | `--model` / `--max-tokens` | 与 agixd 相同 |
| `session_cap` / `session_idle_timeout` | `--session-cap` / `--session-idle-timeout` | 32 / 3600 秒 |
| `permission_prompt_timeout` | `--permission-prompt-timeout` | `None`：按设置，再缺省 300 秒 |
| `instance` | `--instance`（agixd 总有一个） | `None` |
| `socket` | `--socket` | `<data_root>/scenes/<scene>/daemon.sock` |
| `mcp_policy` / `managed_settings` | `--mcp-policy` / `--managed-settings` | 平台的路径；给了就必须存在 |

数据根、套接字和两个策略文件只从这里来，不看本进程的 `AGIX_CONFIG_HOME` 或 `HOME`，所以一个进程里起两个不同 `data_root` 的 `Embedded`，会话互不可见。凭据照旧：`ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL`，以及设置里 `providers` 的 `api_key` / `api_key_env`。一个也没有时照样起来，记一条 warn；走缺省 provider 的那一轮以 `auth` 失败（agixd 同样）。

**`instance`。** 给了名字，`start` 拿 `<data_root>/scenes/<scene>/daemon.lock`（同一场景已有一个在跑 → `EmbedError::AlreadyRunning`），回收这个名字上一次运行留下的进程组并登记这次的（`<data_root>/daemon/groups/<名字>.json`），`serve_unix` 绑好 socket 后写 `<data_root>/daemon/instances.d/<名字>.json`；`shutdown` 把它们都收掉。不给就三样都不做，锁由宿主自己管。进程组的登记是进程级的，一个进程至多起一个带 `instance` 的 `Embedded`：已有一个在跑时再起一个 → `EmbedError::InstanceInProcess`（前一个 `shutdown` 或 drop 之后才能再起）。这次运行的进程组在池建好之后才开始登记，`start` 失败不留登记文件。

**`Embedded`**（`Send + Sync`，要在几个任务里用就放进 `Arc`）：

- `start(config)`：读场景（内建的与 `<data_root>/scenes/` 下声明式的）、设置、插件包、MCP、调度器，建池并让它跑起来（§2 的 `assemble` 做的全部），探测可导入的配置、记启动计时。场景不认识 → `EmbedError::UnknownScene`，其余失败 → `EmbedError::Startup`。
- `serve_stream(stream)`：在任意 `AsyncRead + AsyncWrite` 上服务一条连接，分帧、帧上限、在途上限、出站队列都与 unix socket 上的一条连接相同，不要 `daemon.auth`。回复与事件走同一条有序的流：等到一个回复，就已经收到了排在它前面的帧。进程内就用 `tokio::io::duplex`，不必再写一份进程内链路；对端关掉写端，任务结束。
- `serve_unix()`：在 `config.socket` 上监听，直到关停开始；权限与 agixd 的 socket 相同（`0600`，拒绝引擎自己起的进程）。路径上已有一个还有人在听的 socket（另一个 agixd 或 `Embedded`）时不替换它，返回 `AddrInUse`；没人听的旧文件照常换掉。没有 `instance` 时没有锁，这是唯一挡住两个引擎抢同一个 socket 的地方。关停开始之后再调，什么都不绑、不写，立刻返回。
- `closed()`：关停开始（有连接发了 `daemon.shutdown`，或调了 `shutdown`）就返回；宿主据此收尾自己的东西。
- `shutdown()`：不再接受连接，停掉所有会话（至多等 10 秒），放掉锁、删掉上面那些文件。可以重复调、并发调，后来的等第一次做完。

**不替宿主做的**，即 §7 表里剩下的：装 tracing subscriber（装了就用 `agix::process::log_filter`）、信号处理、panic 钩子、退出进程、建运行时（用调用方的，必须多线程），以及开了 `otel` 时最后的 `agix::process::shutdown_process_exporter`。

**仍是进程级的**，两个 `Embedded` 会共用：

- `network.use_system_proxy` 交给进程级的 HTTP 客户端：进程里第一次读到的设置生效，之后读到不同的只记一条 warn；两个数据根设得不同，后起的那个用不上自己的。代理变量（`HTTP(S)_PROXY`、`NO_PROXY`）也从进程环境读。
- OpenTelemetry 导出器：第一个起它的端点就是全进程的端点。
- 进程组登记：见上，至多一个带 `instance` 的（`InstanceInProcess`）。
- `OTEL_EXPORTER_OTLP_ENDPOINT`（设置没写端点时）与上面的凭据变量从进程环境读。
- `AGIX_RECORD` / `AGIX_REPLAY` / `AGIX_RECORDINGS_DIR` 只对自己拿 `RecorderModel` 包模型、又不给配置的代码起作用；池里会话的录制只看 `options.recorder`。

### 迁移说明

守卫 `tests/guards/tests/embed_surface.rs` 钉着这三个类型的字段、变体和方法签名；这里每条都对应其中的一次变化。

- **0.3.15** 新增 `agix::embed` / `daemon::embed`：`EmbedConfig` 十三个字段（上表）与 `EmbedConfig::new`，`EmbedError::{UnknownScene, AlreadyRunning, Startup, InstanceInProcess}`，`Embedded::{start, serve_stream, serve_unix, closed, shutdown}`。自己照抄 agixd 启动过程的宿主（`load_daemon_config` + `assemble::pool` + `DaemonServer::new`，进程内 `accept_connection` + `dispatch_public`，继承来的流自己写读循环）换成 `Embedded::start` 加 `serve_stream`；进程内连接的回复改走出站队列，往队列里塞屏障帧的做法可以删掉。同一版起 agixd 与 `assemble::pool` 没有凭据也能启动。
