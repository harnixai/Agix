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
| `coding` | `mcp` `mcp-oauth-jwt` `agents` `skills` `web` `structured-output` `ptc` |
| `mcp-oauth-jwt` | MCP 的 `oauth.private_key_jwt`（RFC 7523）。rmcp 用 jsonwebtoken 签断言，带进第二套加密库 aws-lc，约 1.7 MB；关掉后依赖图里没有 `aws-lc-rs`，配了 `private_key_jwt` 的 server 连接时报错并点名这个 feature，能力位 `mcp_oauth_jwt` 不报 |
| `--no-default-features` | minimal：只有内核 |
| `otel` | 另开：把遥测发给 OTLP 收集器 |
| `rpc-schema` | 不在 `agix` 上：它是 `assembly` 的测试用 feature（生成 `docs/schemas/rpc.schema.json`），发布的 agixd 与嵌入方都不带 |

关掉 `mcp-oauth-jwt`：它在 `coding` 里，所以不用 `coding`，把成员逐个列出来：`default-features = false, features = ["mcp", "agents", "skills", "web", "structured-output", "ptc", "scripts", "plugin-packages", "sandbox"]`。

没有 `ws` 与 `full`：WebSocket 监听是 agixd 的。设置里写了未编入模块的段（如 minimal 下的 `mcp`）启动时警告并忽略；`scripts` 段在没编 `scripts` 时记 error（`ARCHITECTURE.md` §1.2）。`agix::process::identity().modules` 报出这个构建编入了哪些模块。

两条路：**会话池**（本指南；agixd 用的就是它，设置分层、provider 路由、历史布局、重建、驱逐、恢复都在）与**单个 Agent**（`agix::modules::Builder` 直接 `build()`，例子在 `crates/runtime/examples/minimal.rs`，以上这些都得自己管）。下游现在都走会话池。

依赖名提醒：宿主自己若已有名为 `agix` 的 feature 或 `mod agix`（Harnix-ext 的 `crates/engine` 与 IPDrun 的 `crates/docs` 就有），feature 写成 `agix = ["dep:agix", …]`；与本地 `mod agix` 同在一个 crate 时用 `::agix::`，或改依赖名：`agix-sdk = { package = "agix", … }`。

## 2. 装配一个池

设置从哪来：`load_daemon_config(model, cli_settings, socket, scope, &paths)` 按全局（`global_root/settings.json`）、场景（`config_root/settings.json`）、项目（`project_root/<项目层目录>/settings.json`，缺省 `.agix`）三层合并，`cli_settings`（`Option<serde_json::Value>`，agixd 的 `--set`）作为 `cli` 层叠在三层之上，池以后读每一对 `(项目, 场景)` 的设置都带上它（`DaemonConfig::cli_settings`）；它不做启动校验，放不进的值像文件里的一样丢掉并记进 `ignored`（层名 `cli`），要启动时就拒绝用 §10 的 `EmbedConfig.settings`；`model` 是内建 provider 三档的模型；`socket` 只在挂 Unix 监听时才用，传 `None`。托管设置：`config.managed_settings_path` 指定的文件必须存在，否则读平台路径（macOS 是 `/Library/Application Support/<产品名>/managed-settings.json`，缺省 `Agix`）。

状态目录：`DefaultDaemonPaths::from_env(scope)` 是 `$AGIX_CONFIG_HOME` 或 `$HOME/.agix`，场景目录 `scenes/<scope>/`；嵌入方自带目录用 `StaticDaemonPaths::with_project(config_root, project_root).with_global(global_root)`，引擎名、项目层目录或产品名不是缺省的时再 `.with_layout(base::paths::Layout::default().with_engine("codix")?.with_product_name("Codix")?)`（§10 的 `engine`、`project_dir` 与 `product`；只改目录用 `Layout::new(".codix")?`）。内建场景用 `builtin(id, &layout)` 取，`layout` 定它的展示名（`<产品名> Code`）：传池自己的 `config.paths.layout()`，名字才与池里其余地方一致。会话日志放哪由 `Assembly::transcripts` 定：`Transcripts::UnderGlobalRoot`（缺省）、`In(store)`、`Nowhere`。`UnderGlobalRoot` 按设置 `app.history`（或 `Assembly::history`，给了就以它为准）决定受信项目的会话是否放进项目（`ARCHITECTURE.md` §8.2）；自己建 `JsonlHistoryStore` 交给 `In` 的，要同样的行为就 `.with_project_history(policy)`，`policy` 实现 `agix::history::ProjectHistory`（项目的历史目录、新会话进不进项目、项目此刻是否受信），不给则一律放全局、也不读索引放进项目的会话。交给 `In` 的存储若是包装另一个存储（加观察、加缓存），`HistoryStore` 上有缺省实现的方法都要转发给里面那个，`history::store::ObservedHistoryStore` 是照着写的样例；漏了 `sidecar_locator`，会话就没有会话记忆与提醒；漏了 `history_dirs_for`，导入导出仍避开设置里的全局历史根，却挡不住项目里的 `state/history`；漏了 `checkpoint_objects`，`session.checkpoints`/`session.rewind` 无从恢复。转发要用的类型（`HistoryError`、`SidecarLocator`、`ProjectHistory` 等）都在 `agix::history` 下。

provider：`Assembly::model_client` 为 `None` 时按 `ANTHROPIC_AUTH_TOKEN` 或 `ANTHROPIC_API_KEY`（加 `ANTHROPIC_BASE_URL`）建 Anthropic 客户端，环境里没有就向 `Assembly::credentials` 要 provider `"default"` 的密钥。设置里的 `providers`、`model.order`、`model.tasks` 在 `assemble` 时校验，配错直接失败，不静默回退。一个 provider 找不到密钥不算配错：它的调用以 `no_credentials` 失败，交给 `model.order` 里的下一个。

模块清单不由嵌入方传：由编入的 feature 决定。要加 Rust 能力用 `Assembly::plugins`（§6）。`Assembly` 其余字段：

- `cwd`：缺省项目；`scenes` 与 `extra_scenes`：自定义场景与要额外启用的场景名。
- `host_policy`：它的 deny 在任何模式下都成立。
- `customize_session`：每次建或重建会话都应用；`mcp`：MCP 的登录存储、OAuth 客户端与策略。
- `js_libs`：给插件代码的宿主 JS 库，见 §10「宿主 JS 库」；不合规的装配失败。
- `history`：会话放在哪，压过设置的 `app.history`；`None` 按设置（缺省 `global`）。只对 `Transcripts::UnderGlobalRoot` 起作用。
- `host_plugins`：宿主自带的插件包，每项是一个解开的包目录（根上有 `plugin.toml`）。装配时逐个登记，
  校验同 `plugin.install`，有一个加载不了装配就失败；设置把插件关了（`plugins.enabled: false`）时一个也不登记，
  记一条 warn。运行中再登记用 `SessionPool::register_host_plugin(dir)`，返回它在 `plugin.list` 里的那一项。
  宿主包在 `plugin.list` 里标 `source: "host"`，`plugin.uninstall` / `plugin.disable` 拒绝它，同名的已装包让位
  （`shadowed_by: "host"`）。见 `plugin_packages.md`「宿主自带的包」。

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
let mut config = load_daemon_config("claude-sonnet-4-6", None, None, "code", &paths);
// 记忆选取会另调一次模型，会吃掉 mock 脚本里的一条回答
# config.settings.values.memory.enabled = false;

let scene = Arc::new(builtin("code", &config.paths.layout()).expect("built-in scene"));
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
# let mut config = load_daemon_config("claude-sonnet-4-6", None, None, "code", &paths);
# config.settings.values.memory.enabled = false;
# let scene = Arc::new(builtin("code", &config.paths.layout()).expect("built-in scene"));
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
# let mut config = load_daemon_config("claude-sonnet-4-6", None, None, "code", &paths);
# config.settings.values.memory.enabled = false;
# let scene = Arc::new(builtin("code", &config.paths.layout()).expect("built-in scene"));
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

调用到来是 `AgentEvent::HostToolCall { call_id, name, input, timeout_ms, cwd, depth, .. }`，`cwd` 与 `depth` 是发起调用的会话的工作目录与委派深度（根会话 0；在 worktree 里跑的子 Agent 的 `cwd` 是那个 worktree）；回答是 `HostReply::Result(ToolResult)`（结果本身可以带 `is_error`）或 `HostReply::Failed(message)`（宿主跑不了这次调用）。宿主工具和别的工具一样过权限门，所以非只读的工具先有一个 `PermissionPrompt`，答过才轮到 `HostToolCall`。没人回答的调用在超时、**所有 watcher 离开**或这一轮被取消时结束：回答期间 watcher 要一直挂着。

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
# let mut config = load_daemon_config("claude-sonnet-4-6", None, None, "code", &paths);
# config.settings.values.memory.enabled = false;
# let scene = Arc::new(builtin("code", &config.paths.layout()).expect("built-in scene"));
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

`SessionCustomizer`（`Arc<dyn Fn(Builder, &SessionCtx) -> Builder>`）是对每次建引擎的最后一句话：执行层（`agix::contracts::exec`）、`token_counter`、`prompt_assembler`、`rule_provider` 这些契约在这里换。**在池里换 `exec` 要求设置 `sandbox.reap_on_close` 为 `false`**（缺省就是）：它开着时池给会话设了 `process_groups`，与 `exec` 同设会让 `build()` 报错（`ARCHITECTURE.md` §1.1 注入口表）。

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
# let mut config = load_daemon_config("claude-sonnet-4-6", None, None, "code", &paths);
# config.settings.values.memory.enabled = false;
# let scene = Arc::new(builtin("code", &config.paths.layout()).expect("built-in scene"));
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
| 异步运行时 | 多线程，工作线程数按设置 `app.worker_threads`（`--worker-threads`），启动最先读一次 | **必须**多线程；`app.worker_threads` 不读 | 你的 `tokio::runtime::Builder::new_multi_thread()` |
| 日志过滤 | `tracing_subscriber` 加 `log_filter(RUST_LOG)` | **必须**（只要装了 tracing subscriber）：rmcp 在 `debug` 下会写出 OAuth 授权码、令牌和表单答复 | `agix::process::log_filter` |
| panic 钩子 | `crash::install(~/.agix/daemon/crashes, "agixd", 版本)` | **建议**：`panic = "abort"` 的构建里它先杀掉命令的进程组再写崩溃记录，不装则 abort 时命令留在机器上；一个进程装一次 | `agix::process::crash::install(dir, 程序名, 版本)` |
| 进程组回收 | `Embedded::start` 拿到实例锁后 `reap_previous(groups/<实例>.json, 500 ms)`，池建好后 `persist_to` 同一路径；`shutdown` 删掉该文件并不再登记 | **建议**（会跑 Bash 或 MCP stdio 的部署）：路径每个实例唯一；`reap_previous` 会跳过拥有者仍活着的记录，`live_owner` 告诉你这样的记录是谁的，该拒绝启动而不是 `persist_to` 盖掉它；收尾时调 `stop_persisting(路径)`，否则之后在本进程里结束的命令会把文件写回来；它会阻塞到宽限期，放在运行时之外或 `spawn_blocking` | `agix::process::orphans::{reap_previous, live_owner, persist_to, stop_persisting}`，或 `agix::embed`（给 `groups_file` 或 `instance`） |
| 信号 | SIGINT 或 SIGTERM 触发 `Embedded::shutdown()`；第二次直接退出 130 | **必须**：收到时 `pool.shutdown_all()`，套一个上限（agixd 是 10 秒） | 你的信号处理加 `SessionPool::shutdown_all` |
| 关停顺序 | 停止监听、删 token 文件；`shutdown_all`（至多 10 秒）；删 groups 文件、删实例文件、放锁；最后 otel 收尾（至多 10 秒） | 照前两步，再删 groups 文件 | `shutdown_all`；开了 `otel` 就必须调 `agix::process::shutdown_process_exporter`，否则最后一批 span 丢 |
| 配置热重载、janitor、定时任务、MCP 后台连接、插件加载 | `assemble::pool` 里起 | **不用做** | 无 |
| 会话日志的写者锁 | 引擎自己对 `sessions/<id>/writer.lock` 持 flock | **不用做** | 无 |
| 实例锁 `daemon.lock` | `Embedded::start` 拿，持有到 `shutdown` | 可不做：只在你也给 agix 客户端开 socket 时需要 | `agix::embed`（给 `instance`），门面不单独收 |
| 实例发现 `instances.d` | `serve_unix` 绑好 socket 后写，`shutdown` 删 | 可不做：只有想让 agix 的客户端按场景找到你才做 | `agix::embed`（给 `instance`），门面不单独收 |
| 监听与认证 | `Embedded::serve_unix`；TCP / WebSocket 与 token 在 `assembly::agixd` | 可不做：进程内用 `Embedded::serve_stream`（§10）、`agix::rpc`（§8）或直接调池 | `agix::embed`、`agix::rpc::DaemonServer` |
| 导入探测、启动计时 | `Embedded::start` 里做 | 可不做 | `agix::embed` |

门面不收实例锁与实例发现：做这两件事等于在重写 agixd 的监听面。要它们的宿主用 §10 的 `agix::embed`：`EmbedConfig.instance` 给了名字，实例锁、实例发现、进程组的回收与登记、导入探测、启动计时都由它照 agixd 的做法做掉，表里剩给嵌入方的只有运行时、日志、panic 钩子、信号与 otel 收尾。自己管数据根互斥、不给 `instance` 的宿主，给 `EmbedConfig.groups_file` 就照样有进程组的回收与登记。

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
# let config = load_daemon_config("claude-sonnet-4-6", None, None, "code", &paths);
# let scene = Arc::new(builtin("code", &config.paths.layout()).expect("built-in scene"));
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
| 换模型、推理强度、思考开关 | `session.setModel` | `pool.set_session_model(id, model, provider, ReasoningChange)`，结果 `ModelChoice`（含 `clamped_options`）；一轮在跑时 `PoolError::SessionBusy` |
| 改提示词 | `session.setPrompt` | `pool.set_session_prompt(id, PromptChange)`，结果 `PromptChoice` |
| 设置的值为什么是这个 | `config.explain` | `pool.explain_setting(key, ExplainTarget, include_secrets)`，结果 `settings::explain::SettingExplanation` |
| 调插件函数 | `plugin.call` / `plugin.functions`，取消用 `plugin.cancel {run_id}` | `pool.call_plugin(PluginCall::new(function, input, PluginCallTarget::Session(id) \| Project(cwd)), cancel)`，`pool.plugin_functions(Option<&PluginCallTarget>)`；取消就是 `cancel.cancel()`（`runext.md` §12） |
| 关停 | `daemon.shutdown` | `pool.shutdown_all()` 加 §7 |

RPC 层多做、池不替你做的检查：认证；`turn_id` 的形式（§3）；消息与附件都为空时拒绝；`options` 里读不出的字段（未知字段、值不对、不是按会话决定的设置键）以 `INVALID_PARAMS` 拒绝（池里直接建会话时，`settings` 里会话不能给的键被略去并告警，要先查调 `SessionOptions::settings_problems()`）；宿主工具回答里图片的 `media_type` 与大小（`ImageSource::check`）：直接给 `ToolResultBlock::image` 不经这道检查。其余方法在 `SessionPool` 上多半同名（见 rustdoc）；它们的参数类型门面没收的，需要时到 `harnixai/Agix` 提 issue。

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
# let mut config = load_daemon_config("claude-sonnet-4-6", None, None, "code", &paths);
# config.settings.values.memory.enabled = false;
# let scene = Arc::new(builtin("code", &config.paths.layout()).expect("built-in scene"));
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

升级：把子模块挪到新标签，编译，读两个标签之间每条标签消息（门面有变化的发布在消息里单列「嵌入面」）。从 0.3.x 升到 0.4.0 的应用先读 `upgrading-0.4.md`（设置键、协议、选项的对照），Rust 类型与函数的变化在 §10 的迁移说明。

门面里没有的东西：到 `harnixai/Agix` 提 issue（标签是项目名加 `enhancement`，标题 `[<项目> <编号>] …`），说明哪段代码要它。门面按需增长，不预先收：进门面的每一条都对应一个下游用法或本指南的一个片段，导出总数钉在 `tests/guards/tests/facade.rs`，加一条删一条都要在提交里写依据。

## 10. 嵌入入口：`agix::embed`

前九节把池当库用：装配的每一步都看得见，`Assembly` 的字段随版本变。只想要「agixd 的参数进、协议连接出」的宿主用 `agix::embed`（也在 `daemon::embed`）：agixd 自己就从这里起，出现的只有 `EmbedConfig`、`EmbedError`、`Embedded` 三个类型（另有 `js_libs` 用的 `HostJsLib`、`HostCallContext`、`HostJsError`、`PackageSource`），宿主之后经协议（`daemon_rpc_protocol.md`）用引擎；协议是 Rust 接口的子集，只有 Rust 才有的（`SessionPool::call_plugin` 的 `CancellationToken`、`register_host_plugin`……）经 `Embedded::pool()` 拿到同一个池去调。

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
| `model` | `--model` | `claude-sonnet-4-6`：内建 provider 三档的模型 |
| `settings` | `--set KEY=VALUE`（与它的简写 `--max-tokens`、`--permission-prompt-timeout`、`--session-cap` 等） | `None`：叠在每一对设置文件之上的 `cli` 层，见下 |
| `instance` | `--instance`（agixd 总有一个） | `None` |
| `socket` | `--socket` | `<data_root>/scenes/<scene>/daemon.sock` |
| `mcp_policy` / `managed_settings` | `--mcp-policy` / `--managed-settings` | 平台的路径；给了就必须存在 |
| `groups_file` | 无（agixd 用 `instance` 的） | `None`：有 `instance` 时是 `<data_root>/daemon/groups/<名字>.json`，否则不登记 |
| `project_dir` | 无（agixd 总是 `.agix`） | `None`：`.<engine>`，引擎在每个项目里的目录名 |
| `product` | 无（agixd 总是 Agix） | `Agix`：引擎自称的产品名 |
| `engine` | 无（agixd 总是 `agix`） | `agix`：引擎在脚本与文件名里的名字 |
| `plugins` | 无（agixd 不带宿主包） | 空：宿主自带的插件包目录，即 `Assembly::host_plugins`（§2）；有一个加载不了 `start` 返回 `EmbedError::Startup` |
| `js_libs` | 无（agixd 不带宿主库） | 空：给插件代码的宿主 JS 库，即 `Assembly::js_libs`；不合规时 `start` 返回 `EmbedError::InvalidHostLibrary`（见「宿主 JS 库」） |
| `credentials` | 无（agixd 只读环境变量） | `None`：给 `api_key_env` 指名的变量在环境里没有的 provider 提供密钥的来源，即 `Assembly::credentials`（`agix::embed::HostCredentials`，包一个 `Arc<dyn CredentialSource>`）；环境里有的变量照用，没有的才问它，宿主可以把密钥放在密钥库或钥匙串里 |

数据根、套接字和两个策略文件只从这里来，不看本进程的 `AGIX_CONFIG_HOME` 或 `HOME`，所以一个进程里起两个不同 `data_root` 的 `Embedded`，会话互不可见。凭据照旧：`ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL`，以及设置里 `providers` 的 `api_key_env` 指名的环境变量，或 `credentials` 给的密钥来源。一个也没有时照样起来，记一条 warn；走缺省 provider 的模型调用不发请求，那一轮以 `no_credentials` 失败（agixd 同样）。

**`project_dir`。** 不给就是 `.<engine>`（缺省 `.agix`）。引擎在项目里放的一切——项目层设置（`settings.json`、`settings.local.json`）、agents、hooks、rules、output-styles、团队、记忆、telemetry、`tool-results`、导入标记——都在 `<项目>/<project_dir>/` 下，其中只属于这台机器的（团队、记忆、scratchpad、telemetry、`tool-results`、导入标记）在它的 `state/` 里，`<project_dir>/.gitignore` 由引擎补上三行：`state/`、`settings.local.json`、`mcp.local.json`（git worktree 除外：它们在仓库根的 `.wkts/`，由用户设置的 `app.worktree_dir` 改名，不随 `project_dir` 变，仓库的 `.gitignore` 也只加这一行，`project_dir` 里的东西照常能提交），提示词、`daemon.doctor`、`config.list` 报的也是这个名字。只认这一个名字，不做“先找它、找不到再读 `.agix`”。名字是一个路径分量，以 `.` 开头，只含小写字母、数字、`.`、`_`、`-`，至多 64 个字符，不能是 `.`、`..`，也不能是别的工具的目录（`.git`、`.agents`、`.claude`、`.codex`、`.cursor`、`.github`、`.vscode`、`.idea`）。要以 `.` 开头，是因为引擎拒写其中的配置子目录，起成 `src`、`docs` 这类项目自己用的名字，项目自己的文件就成了配置；隐藏目录是工具放自己状态的惯例位置。也不能是 `.wkts`（worktree 目录的缺省名）。不合法时 `start` 返回 `EmbedError::InvalidProjectDir { name, reason }`。保护跟着名字走：模型改不了它的设置文件，改其中的配置子目录要问，沙箱里的命令只写得了其中的 `state/scratchpad/`、`state/memory/`（`ARCHITECTURE.md` §8.1）。名字不是 `.agix` 时，项目里的 `.agix` 与 `.claude` 一样作为别的代理的配置继续受保护（写工具拒写其中的设置文件、写别的要问，沙箱里整个写不了，其中的 `settings.local.json` / `mcp.local.json` 仍算凭据），但不再被读。`.agents/skills` 是跨工具的约定，不随名字变；worktree 的分支前缀跟的是 `product`，不是它。

**`product`。** 引擎自称的地方都用它：模型收到的提示词（`You are Codix, …`）、工具指南与报错里的产品名、Bash 指南里提交与 PR 的署名（缺省是带链接的 `[Agix](…)`，改名后只写名字）、桌面通知的标题、Cursor 规则导入时写的注释、内建场景在 `scene.list` / `scene.describe` 里的名字（`Codix Code`、`Codix Mini`，场景 id `code` / `mini` 不变）、OpenTelemetry 的 `service.name`（`<小写名>`）；MCP 动态注册的 `client_name`（授权页上显示的名字；库式用法给了 `McpOptions.oauth_client` 时用宿主的）；引擎发出的 HTTP 请求（模型、MCP OAuth、`WebFetch` 等工具、hook、插件包下载、`config.listModels`）的 `User-Agent: <小写名>/<版本>`，请求自己带了就不覆盖；worktree 的分支 `<小写名>/worktree-<名>` 与 `.gitignore` 里那行的注释、嵌套仓库被挪开的 `.git` 的新名字 `.git.<小写名>-quarantine-<时间>`；没给 `managed_settings` / `mcp_policy` 时读的平台路径（macOS `/Library/Application Support/<名>/`，Windows `%ProgramData%\<名>\`，其余 `/etc/<小写名>/`）。规则：ASCII 字母开头，其后至多 31 个 ASCII 字母、数字、`-`、`_`——小写形式要能当路径分量、git 分支名的一段和 HTTP token，也不能是引擎在项目 `state/` 下自己用的名字（`base::paths::STATE_RESERVED_NAMES`：`memory`、`scratchpad`、`teams`、`telemetry`、`tool-results`、`history`、`imported`、`sessions`、`cache`、`tmp`），因为 `state/<小写名>/` 归嵌入方；不合法时 `start` 返回 `EmbedError::InvalidProductName { name, reason }`。不跟着变的是接口的名字：线上协议的 `agix.*` 方法与命名空间、`AGIX_*` 环境变量、transcript 与导出包的 `agix.` 命名空间、OpenTelemetry 的 instrumentation scope 与 `agix.*` 指标名（它们标识库，不是服务）、协议 schema 的标题、场景 id、内部的临时目录与线程名，以及 agixd 自己。脚本里基础库的名字也不跟它，跟的是 `engine`。

**嵌入方自己的本机状态。** 项目里给宿主留的位置是 `base::paths::Layout::host_state_dir(project_root)`：`<项目>/<project_dir>/state/<product 的小写>/`（Codix 是 `.codix/state/codix/`）。宿主自己建、自己读写，引擎不读不写；它随 `state/` 那一行不进仓库，也和 `state/` 的其余部分一样受保护：模型的写工具写进去要问（没人可问的会话里拒），沙箱里的命令写不了，策略点名的可写根在 `state/` 里时也写不了它。整个 `state/` 是 `Layout::project_state_dir(project_root)`；会话的设置里有 `PathSettings::project_state_dir()`（无项目时 `None`）。

**`engine`。** 引擎在脚本与文件名里的名字：三处 JS 载体（`RunCode`、插件命令、`[[script]]`）里基础库唯一的全局（缺省 `agix`；改成 `foo` 后脚本里只有 `foo`，没有 `agix`），基础库错误的名字 `<首字母大写>Error`（`FooError`），`RunCode` 工具描述与指南里写的 `foo.yaml`、`foo.glob`……（`agix_js.md` §2）；`project_dir` 的缺省 `.<engine>`；沙箱与写工具当作本机另一个实例的状态目录来守的 `~/.<engine>`（`~/.agix` 照守，同机的 agixd 用它）。规则：`[a-z][a-z0-9]*`，至多 16 个字符，不能是 JavaScript 保留字，不能与脚本已有的全局重名（载体放的 `tools`、`console`、`fs`……，新上下文里的 `escape`、`atob`……），`.<engine>` 也要是合法的 `project_dir`（所以不能是 `git`、`claude`、`codex`、`wkts` 这类）；不合法时 `start` 返回 `EmbedError::InvalidEngineName { name, reason }`。用 `agix.*` 写的插件在改了引擎名的嵌入方里调不到基础库，`daemon.doctor` 的 `plugins.engine_name` 点名这些包（只看源码文本）。不跟着变的：`agix.*` RPC 方法与命名空间、`AGIX_*` 环境变量、transcript 命名空间、OpenTelemetry scope 与指标名、agixd、crate 名，以及 `data_root`（必给，不从引擎名推）。库式用法：`base::paths::Layout::default().with_engine("foo")?`（同时把项目层目录设成 `.foo`，要别的名字在它之后 `.with_project_dir(..)?`），`Layout::engine()`，`DEFAULT_ENGINE`，`EngineNameError`；脚本全局名的规则是 `js_lib::check_global_name`（即 `base::interface::script::check_global_name`）。

**`settings`。** 一个设置键组成的 JSON 对象（`json!({"permissions": {"mode": "plan"}, "turn": {"max_model_calls": 50}})`），与 agixd 的 `--set` 是同一个东西：作为 `cli` 层叠在每一对 `(项目, 场景)` 的设置文件之上，`config.reload` 之后照样在。它按全局层的规矩合并——`app` 照收，不受项目信任门禁——`permissions` 的规则列表与各层的并列生效，对每一对都加上。输出上限、权限询问的超时、会话放在哪都写在这里：`{"model": {"max_tokens": 4096}, "permissions": {"prompt_timeout_secs": 60}, "app": {"history": "project"}}`。会话上限与闲置超时也在这里写：`{"app": {"session_cap": 4, "session_idle_timeout_secs": 600}}`（缺省 32 与 3600，也可以写在 `<data_root>/settings.json`），`start` 时读一次，`config.reload` 不改（改了的键报在它的 `restart_required`）。`start` 时对守护进程自己那一对查一遍：有没有设置读的键、旧键（`reason` 里给出新键名）、放不进的值，或者它不是对象，就返回 `EmbedError::InvalidSetting { key, reason }`，`key` 是点分的键（不是对象时为空）。`config.get {tier:"cli"}` 返回这一层，`daemon.doctor` 的 `settings_tiers` 里有一项 `tier: "cli"`。

**会话放在哪（`app.history`）。** 会话的转录、会话记忆、metadata、checkpoint 副本与输入历史放在哪。`Global`（agixd 的缺省）放在 `<data_root>/projects/` 与 `<data_root>/sessions/<id>/`。`Project` 让**受信**项目的新会话整份放进 `<项目>/<project_dir>/state/history/<id>/`（git worktree 里的会话放进主仓库的这个目录），`<data_root>/sessions/<id>/` 只留一份索引（`index.json`：项目根、会话目录与谱系）和写者锁；不受信的项目、无项目的会话照旧放全局，子 Agent 与同一项目的分叉跟着父会话。写在 `settings` 或全局设置文件里（只认全局层、`cli` 层与 managed 层，场景层、项目层写了忽略并告警），启动时读一次。已经放在任一处的历史原地照读、不迁移，两种位置并存以索引为准，所以中途改这个值不会丢会话。只读索引登记过的项目内会话：克隆来的仓库自带的 `state/history/` 不会被当成谁的历史；项目被取消信任后，它里面的历史不读，`session.resume` 等报 `PROJECT_UNTRUSTED`。项目整个搬了家，在新位置打开它（`cwd` 是它，或有会话建在它上面）时索引跟过去。导出不能写进、导入不能读自项目的历史目录。细节在 `ARCHITECTURE.md` §8.2。

**`instance`。** 给了名字，`start` 拿 `<data_root>/scenes/<scene>/daemon.lock`（同一场景已有一个在跑 → `EmbedError::AlreadyRunning`），`serve_unix` 绑好 socket 后写 `<data_root>/daemon/instances.d/<名字>.json`；`shutdown` 把它们收掉。不给就两样都不做，锁由宿主自己管。

**`groups_file`。** 会话里的命令与 MCP stdio 服务器各领一个进程组，引擎被 `SIGKILL` 时它们不跟着走。给了这个文件，`start` 先停掉上一次用同一文件、没活到收尾的那次运行留下的进程组（最多等 500 ms 再 `SIGKILL`），池建好之后把这次的登记在这里；`shutdown` 删掉它，之后本进程里的命令不再往那里写。文件里的拥有者是另一个还活着（或查不出死活）的进程时，那是它的登记不是遗留：`start` 不动它、也不接管，返回 `EmbedError::GroupsHeld { owner, file }`，已拿的实例锁等随之放掉。不给而有 `instance` 时用 `<data_root>/daemon/groups/<名字>.json`（agixd 就是这样）；两个都不给就不登记。登记是进程级的，一个进程里同时只有一个 `Embedded` 能登记：已有一个在跑时再起一个要登记的 → 前一个是带 `instance` 的，`EmbedError::InstanceInProcess`，否则 `EmbedError::GroupsInProcess`（前一个 `shutdown` 或 drop 之后才能再起；drop 与 `shutdown` 一样不再登记并删文件）。这次运行的进程组在池建好之后才开始登记，`start` 失败不留登记文件。文件每个数据根（或实例）一个，别让两个同时在跑的进程用同一个。

**`Embedded`**（`Send + Sync`，要在几个任务里用就放进 `Arc`）：

- `start(config)`：读场景（内建的与 `<data_root>/scenes/` 下声明式的）、设置、插件包、MCP、调度器，建池并让它跑起来（§2 的 `assemble` 做的全部），探测可导入的配置、记启动计时。场景不认识 → `EmbedError::UnknownScene`；实例锁被占 → `AlreadyRunning`；进程组登记冲突 → `InstanceInProcess` / `GroupsInProcess`（本进程已有一个在登记），或 `GroupsHeld`（登记文件的拥有者是另一个活着的进程）；其余失败 → `Startup`。
- `serve_stream(stream)`：在任意 `AsyncRead + AsyncWrite` 上服务一条连接，分帧、帧上限、在途上限、出站队列都与 unix socket 上的一条连接相同，不要 `daemon.auth`。回复与事件走同一条有序的流：等到一个回复，就已经收到了排在它前面的帧。进程内就用 `tokio::io::duplex`，不必再写一份进程内链路；对端关掉写端，任务结束。
- `serve_unix()`：在 `config.socket` 上监听，直到关停开始；权限与 agixd 的 socket 相同（`0600`，拒绝引擎自己起的进程）。路径上已有一个还有人在听的 socket（另一个 agixd 或 `Embedded`）时不替换它，返回 `AddrInUse`；没人听的旧文件照常换掉。没有 `instance` 时没有锁，这是唯一挡住两个引擎抢同一个 socket 的地方。关停开始之后再调，什么都不绑、不写，立刻返回。
- `closed()`：关停开始（有连接发了 `daemon.shutdown`，或调了 `shutdown`）就返回；宿主据此收尾自己的东西。
- `pool()`：协议服务的那个 `SessionPool`（`&Arc<SessionPool>`），给协议没有的 Rust 用法，如 §8 表里的 `call_plugin`。与协议连接同时用没有问题：两边看到的是同一组会话。
- `shutdown()`：不再接受连接，停掉所有会话（至多等 10 秒），放掉锁、删掉上面那些文件。可以重复调、并发调，后来的等第一次做完。

**不替宿主做的**，即 §7 表里剩下的：装 tracing subscriber（装了就用 `agix::process::log_filter`）、信号处理、panic 钩子、退出进程、建运行时（用调用方的，必须多线程），以及开了 `otel` 时最后的 `agix::process::shutdown_process_exporter`。

**仍是进程级的**，两个 `Embedded` 会共用：

- `app.system_proxy` 交给进程级的 HTTP 客户端：进程里第一次读到的设置生效，之后读到不同的只记一条 warn；两个数据根设得不同，后起的那个用不上自己的。代理变量（`HTTP(S)_PROXY`、`NO_PROXY`）也从进程环境读。
- OpenTelemetry 导出器：第一个起它的端点就是全进程的端点，`service.name` 也是那个池的产品名。
- 进程组登记：见上，至多一个登记的（`InstanceInProcess` / `GroupsInProcess`）。
- `OTEL_EXPORTER_OTLP_ENDPOINT`（设置没写端点时）与上面的凭据变量从进程环境读。
- `AGIX_RECORD` / `AGIX_REPLAY` / `AGIX_RECORDINGS_DIR` 只对自己拿 `RecorderModel` 包模型、又不给配置的代码起作用；池里会话的录制只看 `options.recorder`。

### 宿主 JS 库

嵌入方给**插件代码**注册自己的 JavaScript 库：一个命名空间下的一组异步函数，JSON 进、JSON 出。插件包顶层声明
`uses = ["codix"]`（`plugin_packages.md`「宿主库」），它的插件命令（`RunExt` 与 `/name`，`runext.md`）和插件函数
（`[[function]]`，`runext.md` §12）的运行时里就有全局 `codix`；没声明的包、`RunCode` 的程序、`[[script]]` 的九个点
都没有。宿主拿不到 QuickJS 的任何类型：桩由引擎按函数表生成，每次新建运行时不跑宿主的 Rust 代码，Promise 的衔接、
取消与上限由引擎管。

```rust
use agix::embed::{EmbedConfig, Embedded, HostCallContext, HostJsError, HostJsLib, PackageSource};
use serde_json::{json, Value};

# fn main() -> anyhow::Result<()> {
# tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
# let dir = tempfile::tempdir()?;
let mut config = EmbedConfig::new(dir.path().join("state"));
# config.cwd = Some(dir.path().to_path_buf());
config.js_libs.push(
    HostJsLib::new("codix")
        .function("flow.start", |cx: HostCallContext, input: Value| async move {
            if cx.via == "model" && cx.source != PackageSource::Host {
                return Err(HostJsError::new("denied", "only the host's own packages start flows for the model"));
            }
            Ok(json!({ "run": "r-1", "change": input["change"] }))
        })
        .prelude("codix.flow.startAll = (changes) => Promise.all(changes.map((change) => codix.flow.start({ change })));")
        .description("Codix 的流程与作业"),
);
let engine = Embedded::start(config).await?;
engine.shutdown().await;
# Ok::<(), anyhow::Error>(())
# })
# }
```

插件里写 `const run = await codix.flow.start({ change: "c1" });`。

- **函数**：`function(path, f)`，`f` 是 `Fn(HostCallContext, Value) -> impl Future<Output = Result<Value, HostJsError>>`
  （`Send + Sync + 'static`）。`path` 是命名空间之下的点分路径，`flow.start` 在脚本里是 `codix.flow.start(input)`，
  返回 Promise。`input` 不传是 `null`；没有 JSON 形式的（函数、`BigInt`）在脚本里就抛 `TypeError`。函数在调用方的
  tokio 运行时上自己一个任务里跑。
- **`prelude(js)`**：纯 JavaScript，在桩之后、插件代码之前求值（基础库与 `tools.*` 已在），可以往命名空间上加便利
  封装。求值完命名空间整个冻结：全局不可写、不可删、不可枚举，与基础库一样。prelude 抛错时每次运行都失败，结果是
  ``the prelude of host library `codix` failed: …``。
- **`description(text)`**：进安装披露，``uses host library `codix` (<text>)``。
- **调用上下文** `HostCallContext`：

  | 字段 | 内容 |
  |---|---|
  | `plugin` / `plugin_version` | 代码所在的包，按包自己 `plugin.toml` 里写的 |
  | `source` | 包从哪来：`PackageSource::Host`（宿主经 `EmbedConfig.plugins` / `register_host_plugin` 登记的自带包）或 `PackageSource::Installed`（`plugin.install` 装的） |
  | `entry` | 命令名，或函数名 |
  | `session_id` | 所在会话；没有会话时 `None` |
  | `project_root` / `cwd` | 会话的工作目录（同脚本 `ctx` 的 `projectRoot` / `cwd`） |
  | `via` | 谁跑的这段插件代码：`model`（模型调 `RunExt`）、`user`（用户敲的 `/name`）、`skill`（skill 的内联命令）、`RunCode`（程序里调 `RunExt`）、`host`（宿主调插件函数） |
  | `agent_depth` | 子 Agent 层级，根会话是 0 |
  | `cancel` | 脚本被停时触发 |

  宿主库的调用不是工具调用：不经权限判定、`tool.around` 与 hooks，不进转录。放不放行由宿主看上下文自己定。
  `plugin` 是包自报的名字，谁都能装一个叫 `xflow` 的包；`uses` 也不是授权，任何包写上就拿得到这个全局。要认
  宿主自己的包，看 `source`，不看 `plugin`。
- **出错**：返回 `Err(HostJsError { code, message })` → Promise reject 一个 `Error`，带 `code` 与 `message`；命令没接住
  时模型读到的是 `message`（`runext.md` §4）。引擎自己的拒绝也带 `code`：`limit`（见下）、`stopped`（运行在停）、
  `failed`（函数 panic）。
- **上限与取消**：输入与返回值按 JSON 各至多 200,000 字符、嵌套至多 128 层（同一次工具调用交回脚本的上限），超了
  reject `limit`，函数不被调用或返回值丢掉。一次运行里同时在跑的宿主库调用至多 8 个（与工具子调用的
  并发上限同一个数，两者各算各的），多出的排队，排着时运行被停就以 `stopped` reject、函数不被调用。
  没有单独的时限：所在运行的时限（命令 2 分钟）管它。脚本被停（超时、
  `TaskStop` 宽限过后、这一轮被中断）时 `cancel` 触发、Promise 以 `stopped` reject，引擎不再等这个函数；函数该在
  `cancel` 上收手。
- **观测**：每次调用往所在 `ext` 任务的输出记一行，``codix.flow.start ok 12ms``、``codix.flow.fail error denied 3ms``
  （`session.tasks` / `TaskOutput` 看得到），并打一个 tracing span `host_library_call`（`function`、`plugin`、`entry`、
  `via`）。
- **命名**：命名空间 `[a-z][a-z0-9]*`，不能是引擎名（`engine`），也不能是 `agix`（包的 `uses` 写不了它），不能是
  JavaScript 保留字或脚本已有的全局（`tools`、`callTool`、`console`、`log`……，规则同 `engine`），库之间不重名；函数
  路径是点分的 JavaScript 标识符（`[A-Za-z_$][A-Za-z0-9_$]*`，不能是 `__proto__`），不重复，一个路径不能又是另一个的
  前缀（`flow` 与 `flow.start` 不并存）。不合规时 `start` 返回 `EmbedError::InvalidHostLibrary { namespace, reason }`。
- **安装**：库名单就是包的 `uses` 对照的名单：声明了没提供的库的包 `plugin.install` 拒装，已装的不加载，原因进
  `plugin.list` / `daemon.doctor`（`plugin_packages.md`「宿主库」）。给插件作者的 `.d.ts` 由宿主随自己的开发资料提供。
- **库式用法**：`Assembly::js_libs`（§2），校验相同，不合规时 `assemble` 失败，消息同上；`ptc::HostLibraries` 是
  `RunExtTool::with_host_libraries` 收的那一份，自己用 `Builder` 装会话时给 `Builder::host_js_libs`，子 Agent 经
  `HostContracts` 继承。

### 迁移说明

守卫 `tests/guards/tests/embed_surface.rs` 钉着这三个类型的字段、变体和方法签名；这里每条都对应其中的一次变化。

- **0.4.0**（相对 0.3.21；协议版本 5）
  - **设置从外面进来的口子。** `EmbedConfig` 删掉 `session_cap`、`session_idle_timeout`、`max_tokens`、`permission_prompt_timeout`、`history` 五个字段，新增 `settings: Option<serde_json::Value>`（缺省 `None`）与 `EmbedError::InvalidSetting { key, reason }`：`settings` 是叠在每一对 (项目, 场景) 设置文件之上的 `cli` 层，与 agixd 的 `--set` 同一条路径，有没有设置读的键、旧键、放不进的值让 `start` 返回 `InvalidSetting`。原来的 `config.max_tokens = n`、`config.permission_prompt_timeout = Some(d)`、`config.history = Some(HistoryLocation::Project)`、`config.session_cap = n` 写成 `config.settings = Some(json!({"model": {"max_tokens": n}, "permissions": {"prompt_timeout_secs": d.as_secs()}, "app": {"history": "project", "session_cap": n}}))`；`model.max_tokens` 写在 `cli` 层，压过设置文件里写的值，不再只是没写时的兜底。`agix::embed::HistoryLocation` 随之删掉（类型仍是 `settings::HistoryLocation`）。agixd 的 `--max-tokens`、`--permission-prompt-timeout`、`--session-cap`、`--session-idle-timeout`、`--worker-threads` 是 `--set` 的简写，`AGIX_WORKER_THREADS` 排在所有参数之前。新增设置块 `app` 的 `session_cap`（缺省 32）、`session_idle_timeout_secs`（缺省 3600）、`worker_threads`（agixd 用，嵌入时不读），`app` 只认全局层、`cli` 层与 managed 层。
  - **库式用法的签名。** `load_daemon_config` 的第二个参数由 `max_tokens: u32` 改为 `cli_settings: Option<serde_json::Value>`，`DaemonConfig::max_tokens_fallback`、`session_cap`、`session_idle_timeout_secs` 换成 `DaemonConfig::cli_settings`；`Assembly::permission_prompt_timeout` 删掉，写进 `cli_settings` 的 `permissions.prompt_timeout_secs`（或 `config.settings.values.permissions.prompt_timeout_secs`）；`assemble::pool` 从套好 managed 层的设置读 `app.session_cap` 与 `app.session_idle_timeout_secs`。新增 `config::process_settings(root, cli_settings, managed_settings_path: Option<&Path>)`（`None` 是平台路径），读全局层加 `cli` 层、managed 层压顶的 `app` 块。`settings::LoadedSettings::load_from` 的八个参数收进 `settings::LoadRequest { source, global_dir, scene_dir, local_dir, scope, default_model, sections, layout }`，调用处改成 `load_from(LoadRequest { .. })`；新增 `LoadedSettings::load_without_project_from` 与公有字段 `LoadedSettings::cli_layer`。`base::interface::config_source::Tier` 多 `Cli`（名字 `cli`，规则来源 `RuleSource::CliArg`）与 `Session`（名字 `session`，规则来源 `RuleSource::Session`），对它穷举 `match` 的要补；`config.get` 的 `tier` 多一个 `cli`（`ConfigTierName::Cli`）。`SettingsReloaded`（`config.reload`、`config.set`、`config.setProvider` 的结果）多 `restart_required: Vec<String>`：只在启动时读的键（`settings::keys::read_at_start`）里值变了的，managed 文件变了时另有 `managed`。
  - **设置的严格程度按写的人分。** 设置文件里坏的键与值告警并忽略；`EmbedConfig.settings` 与 `agixd --set` 里的让启动失败；`SessionOptions.settings` 经 RPC 的让请求以 `INVALID_PARAMS` 失败；managed 文件里的一个都不套、拒绝所有工具调用（`ARCHITECTURE.md` §3.2.1）。引擎自带的 `anthropic` provider 只在没有定义 `providers` 时存在，模型是 `--model`；要给它配额或档位，在 `providers` 里自己定义。provider id 只能由小写 ASCII 字母、数字、`-`、`_` 组成（`ProviderConfig::id_problem`）：别的 id 加载时整项忽略（`daemon.doctor` 的 `ignored_values` 里说明），`routing_order` 也拒绝；`quota/` 下的用量文件名就是 id 本身。
  - **会话选项与会话层（RPC 不兼容）。** `options.settings` 是这个会话（含子 Agent、团队成员）的设置覆盖层，形状同 settings.json，只收按会话决定的键（键表 `settings::keys` 的作用域），缺省只能收紧，被夹住的报在 `SessionCreated.clamped_options: Vec<ClampedOption>`（`ClampedOption { field, requested, applied }`，后两个是 `serde_json::Value`，`field` 如 `settings.turn.token_budget`），能力位 `session_settings`。`SessionOptions` 删掉 `permission_mode`、`permission_rules`、`turn`（与类型 `TurnOptions`）、`thinking_mode`、`effort`、`prompt`（与类型 `runtime::agent::SessionPrompt`、`Builder::session_prompt`），分别写成 `settings.permissions.mode`、`settings.permissions.{allow,allow_exact,ask,deny}` 的规则串（allow 在设置不许放宽时被扣下）、`settings.turn`、`settings.model.thinking`、`settings.model.effort`、`settings.prompt.{append,override}`，推理强度、思考开关与提示词因此也作用于子 Agent 与团队成员；新增 `settings: Option<serde_json::Map<String, Value>>`、`SessionOptions::requested_permission_mode()` 与 `settings_problems()`（`settings.model.thinking` 的 `on_budget` 小于 1024 也在其中）。不经 RPC 直接建会话时，会话不能给的键被略去并告警，要先查就调 `settings_problems()`。`SessionPool::set_session_model` 的 `ReasoningChange { effort, thinking }` 与 `set_session_prompt(session_id, PromptChange)`（`PromptChange { append, override_text }`；两者的字段 `None` 不动、`Some(None)` 去掉）写会话的这层设置，判法同 `options.settings`；`ModelChoice` 与 `PromptChoice` 报这层设置里的值，多 `clamped_options: Vec<ClampedOption>`。`session_state::ReasoningChoice` 与 `SessionState::{reasoning_choice, set_reasoning_choice}` 删掉；`runtime::agent::ReasoningReport::of(&ModelSettings)` 取代 `resolve`，两个来源都报 `settings`，会话自己的由池标成 `session`。会话日志里 0.3 记下的旧形状恢复时按新形状读。
  - **会话定下的值不再写进设置：`settings::SessionConfig`。** 它是为这一对 (项目, 场景) 读好的设置（`shared()`，`Arc<LoadedSettings>`，不按会话改）、会话设置层（`session_layer()`、被压住的 `held()`、`requested(key)` / `applied(key)`）与两者经 `LoadedSettings::overlay` 得出的 `settings()`，以及会话自己定的 `permission_mode()`、`model_name()`、`workspace()`、`worktree()`（`working_dir()`、`worktree_git()`、`engine_config()` 由它们得出）；构造用 `SessionConfig::of(settings)` 或 `with_session_layer(shared, layer, sections)`，再 `with_permission_mode` / `with_model_name` / `with_workspace` / `with_worktree`。`ModelSettings.model_name` 删掉：设置给的对话模型是 `LoadedSettings.main_model`，会话的是 `SessionConfig::model_name()`、`Agent::session_config().model_name()`；`Settings::defaults()` 是不带 provider 的缺省，`Settings::defaults_for(model)` 是缺省加上引擎自带的 provider（服务 `model`），`Settings::builtin_model()` 读回它。`base::paths::PathSettings` 的 `worktree` 字段与 `worktree_git()` 删掉，`working_dir()` 改名 `workspace_dir()`（只是项目或 scratch 目录；会话在 worktree 里时用 `SessionConfig::working_dir()`）。`Builder::settings(Arc<LoadedSettings>)` 照旧（即 `SessionConfig::of`），新增 `Builder::session_config(SessionConfig)`；`module::SessionContext` 多 `session_config` 字段，`Agent::session_config()`；`Agent::settings()` 是会话层叠过的设置，其中 `permissions.mode` 是设置说的值，会话按哪个模式跑看 `permission_mode()`。
  - **键表与逐键闸门。** 新增 `settings::overlay`（`LoadedSettings::overlay(layer, Subject, sections)`，宿主内存覆盖与会话层共用）与键表 `settings::keys`：每个设置键一行 `Key`（点分路径，`*` 代一个 map 键；每一列各自取设了它的最长前缀那行），列是作用域 `Scope { Process, Pair, Session }`（实现 `Serialize` / `Deserialize`，`process` / `pair` / `session`）、未受信项目能做什么 `Trust { Free, Tighten(Order), Withheld }`、是否只在启动时读（`read_at_start`）、哪些层的文件能写 `Writers { AnyTier, GlobalOnly }`；核心键在 `keys::KEYS`，另有 `keys::{scope_of, keys_outside, read_at_start, global_only_at, key_gaps}`。`Order` 在 `settings::keys`，含 `Higher`（下限，只能更大）。`settings::section::Section` 的 `const PROJECT_TIER` 换成 `const KEYS: &'static [Key]`，路径相对段名、`""` 是段本身，段的作用域写在那一行（`Key::at("").scope(Scope::Session)`、`Key::at("enabled").tighten(Order::FalseTighter)`），自己注册段落的要改；`SectionSpec.project_tier` 换成 `keys`。`settings::trust` 里的逐键闸门拆开：`ProjectKey` 换成 `settings::keys::Trust`，`PROJECT_TIER` 换成 `keys::KEYS`，`project_tier_gaps` 换成 `keys::key_gaps`（同时查作用域），`gate_layer` 与新增的 `tighten_layer` 在 `settings::tier_gate`；`settings::GLOBAL_ONLY_KEYS` 删掉，由 `app` 行的 `Writers::GlobalOnly` 定，查一个键用 `keys::global_only_at`。
  - **managed 层能写任何设置键**（`ARCHITECTURE.md` §6.1）：写了的键压过设置文件、`cli` 层、宿主覆盖与会话的 `options.settings`（有松紧次序的键下层更严时留下层的），读不了、有没有设置读的键或放不进的值时一个都不套、拒绝所有工具调用。`base::permission::ManagedSettings` 多公有字段 `values`（规则列表与 `disable_bypass` 之外它设的一切）并实现 `PartialEq`，自己构造它的补 `..Default::default()`；`ManagedSettings::followed_by` 移成 `settings::managed::followed_by`；`settings::managed::{read, from_value}` 多一个 `&Sections`；`MANAGED_KEYS` 与 `MANAGED_PERMISSION_KEYS` 删掉，新增 `managed::as_file` 与 `managed::DISABLE_BYPASS`；`LoadedSettings::with_managed` 套它的值，多一个 `&Sections`（段落键的松紧次序按登记的段落判）。`daemon.doctor` 的 `settings.managed` 的 `details.permissions` 换成 `details.settings`（文件的形状）。
  - **设置溯源**（协议上是 `config.explain`，能力位 `config_explain`）：新增 `SessionPool::explain_setting(key, ExplainTarget, include_secrets) -> Result<settings::explain::SettingExplanation, PoolError>`，`ExplainTarget::{Pair { project_root, scene }, Session(session_id)}`；结果的类型在 `settings::explain`（`SettingExplanation { key, keys, truncated }`、`KeyExplanation`、`LayerOutcome`、`ManagedOutcome`、`SessionOutcome`、`SettingOutcome`），自己有加载输入的嵌入方也可以直接调 `settings::explain::explain(&Sources { layers, host, settings, session }, key, sections)`。
  - **用量单点计量与预算链。** 一次调用的用量只由路由它的那一层计一次——`model::routing` 的路由，或没有路由时 `Builder` 在根上套的那一层——同时记 provider 配额和 `UsageTap` 所记的节点。`UsageTap` 多 `UsageTap::charging(node)`、`for_attempt()`、`charge()` 与 `metered(stream, also: Option<OnSpent>)`（自己写路由的宿主也用它计）；调用方（主循环、旁路调用）只往 tap 上说记到哪，自己不再累加。`TurnSpend` 改名 `SpendNode`，是带上限的节点链：`SpendNode::session(limit)`、`SpendNode::goal(limit, session)`、`SpendNode::turn(limit, above)`、`counting_into(own_session)`、`remaining() -> Option<(Scope, Remaining)>`（整条链上最小的剩余）、`judge() -> (Scope, Spending)`、`whole_turn()`、`find(Scope)`、`set_limit`、`restore`，新增 `Scope { Turn, Goal, Session }`（`whose()` 公有）与 `exhausted_message`；`TurnScope::opening(origin, node)`。`turn.token_budget` 挂在根轮的节点上，不再在 `BudgetPolicy` 里：`EngineBudget` 去掉 `max_total_tokens` 与 `EngineBudget::new`，只剩压缩与输出目标（`settings.engine_budget()`）。口径统一为 `Spend::counted_tokens()`（未命中缓存的输入 + 缓存写入 + 输出，同 provider 配额），`Spend::total_tokens()` 删掉；`Spend` 序列化用线上的字段名（`cache_creation_input_tokens`、`cache_read_input_tokens`），新增 `as_usage`、`max`。`budget_exceeded` 的 `TurnOutcome.message` / `turn_complete.message` 写明是谁的预算。
  - **预算怎么判。** 一轮（`turn.token_budget`）含这一轮起的子 Agent、团队成员与旁路调用：`DelegateRequest` 多公有字段 `spend: Option<Arc<SpendNode>>`（`DelegateRequest::new` 给 `None`，即 delegate 每轮自己一份；宿主 `session.startAgent` 起的子 Agent 为 `None` 时会话池把会话的节点给它），自己起 delegate 的工具从 `ctx.turn.spend()` 取来填上。`BudgetPolicy` 多有缺省实现（`None`）的 `remaining(&Spend) -> Option<Remaining>`，宿主注入的 policy 在引擎各层的上限之外再判，两边都要过；每次调用前从链上最小的剩余里先扣掉这次请求不由缓存供给的输入的估计（`BudgetPolicy::remaining` 的剩余同样扣），不到 `MIN_CALL_TOKENS`（1024）不发，否则 `max_tokens` 压到剩余。delegate 判预算用的 policy 随 `HostContracts` 继承根会话的 `Builder::budget_policy`（`HostLayers` 多 `budget_policy`）。`AgentEvent::TurnComplete` 的 `usage` 是这一轮的全部（自己、旁路、delegate），多 `usage_breakdown: UsageBreakdown { own, side, delegates }`；`TurnOutcome::total_usage` 仍只是本 agent 主循环的；新增 `Agent::run_turn_charged_to(content, turn_id, cancel, spend)`。hook 的模型调用记进触发它的那一轮：`hooks::HookInput` 多不序列化的公有字段 `spend: Option<Arc<SpendNode>>`（及 `spending_into`），引擎触发 hook 时填上；`prompt` hook 的调用经 `UsageTap::charging` 记在它上面、`max_tokens` 压到剩余，`agent` hook 把它放进 `DelegateRequest.spend`；自己实现 `PromptHookExecutor` / `AgentHookExecutor` 的嵌入方从 payload 取它。
  - **会话累计与目标预算。** `SessionLedger.spent` 是会话的 `Arc<SpendNode>`（u64、不饱和，失败的轮里已报出的用量也在内，含子 Agent、团队成员与旁路调用，包括两轮之间的调用），原来的 `api_calls` / `tool_calls` 挪到 `calls: SessionCalls`（仍是本 agent 自加载以来的）；`/cost` 与 telemetry `session_end` 的 token 合计随之含子 Agent。累计记进会话日志（`agix.spend` / `total`，载荷 `base::context::SpendRecord`，`SessionLedger::{spend_record, spend_unrecorded}`），`SessionState::restore_from_log` 多一个参数放回，`runtime::turn::recorded_spend(entries)` 读它；设置 `session.token_budget` 给它设上限；`session.get` 多 `usage`（`SessionDetail.usage: Option<Spend>`）。目标：`Goal.token_budget: Option<u64>`，`GoalView.tokens_used: u64`；目标的轮在用量链上插一层目标节点（`SessionLedger::open_goal`、字段 `goal_spent: Option<GoalSpend>`）。`GoalSlot::create` 收 `GoalLimits { max_rounds, token_budget, max_rounds_limit }`（宿主用 `GoalLimits::asked`），`update` 多 `spent`、`max_rounds_limit` 两个参数，`next_round(spent)`、`view(spent)`，新增 `block_if_spent(spent, next)`（`next` 是下一次调用输入的估计，付不起时目标转 `blocked`）与 `SessionState::goal_block_unaffordable(input)`；`GoalAction::Edit` 多 `token_budget`；`Session::goal_create(objective, limits, origin)`、`Session::goal_update(.., max_rounds_limit)`。模型的 `GoalUpdate` 只能调低目标的预算。`EngineConfig.model_autonomy: ModelAutonomy { goal_max_rounds, max_jobs, min_job_interval_minutes }` 来自设置 `goal.max_rounds`（`settings::GoalSettings`）与 `schedule`（`settings::ScheduleSettings { max_model_jobs, min_model_interval_minutes }`，只限模型经 `CronCreate` 建的任务，宿主的 `schedule.create` 不受限）；`CronJob` 与 `NewCronJob` 多 `by_model: bool`（`schedule.list` 的任务随之带 `by_model`），`tools::cron::shortest_gap_minutes`。协议：`session.setGoal` / `updateGoal` 的 `token_budget`，目标状态的 `token_budget`、`tokens_used`。
  - **provider 路由挪出 `base`。** `base::provider::TaskRouter` 删掉，`RoutedProvider` 挪到 `model::routing`，实现也在那里——`ProviderSet` 是一个 (项目, 场景) 的 provider、顺序、档位，连同健康与配额记账（`new`、`single`、`with_quota`、`map_models`、`order`、`head`、`provider`），`SessionRouter` 是一个会话的视图（`ProviderSet::session(preferred)`，即原 `TaskRouter::for_session`；`order`、`head`）。runtime 与模块只见新 trait `base::provider::TaskRouting { model_for, model_name_for, resolve_model, observe }`，`SessionRouter` 实现它：`Builder::task_router` 与 `HostContracts.task_router` 收 `Arc<dyn TaskRouting>`。`TaskRouter::{accepts, model_spec, tasks, provider_config}` 删掉。库式用法：`model_router::build_task_router[_with]` 返回 `ProviderSet`。新增 `ModelEvent::Served { provider }`：路由过的调用的流以它开头，推理内容按它打标，路由出来的模型 `Model::provenance()` 是 `None`；对 `ModelEvent` 穷举 `match` 的要补。
  - **RPC 命名对齐（不兼容）。** `session.setModel` 的参数与结果里的 `thinking_mode` 改名 `thinking`（同设置 `model.thinking`），`session.get` 与 `session.inspectContext` 的 `reasoning.{thinking_mode, thinking_mode_source}` 改名 `{thinking, thinking_source}`，`session.call` 的 `request.thinking_mode` 改名 `thinking`；`stop_reason` 的 `max_turns_reached`（从没发出过）改成引擎实际发的 `max_turns`。库式用法：`ReasoningChange.thinking`、`ModelChoice.thinking`、`runtime::agent::ReasoningReport.{thinking, thinking_source}`、`CallRequest.thinking`，`base::message::StopReason::MaxTurnsReached` → `MaxTurns`。
  - **构建。** RPC 的机器可读 schema 只在测试或 `assembly` 的 `rpc-schema` feature 下编译，发布的 agixd 与嵌入方不带。
- **0.3.21**
  - 新增 `EmbedConfig.credentials: Option<HostCredentials>`（缺省 `None`）与类型 `agix::embed::HostCredentials`：给 `api_key_env` 指名的变量在环境里没有的 provider 提供密钥；库式用法是 `Assembly::credentials`。
  - 设置按块重排，旧键不再读（读到时告警里给出新键名，对照表是 `settings::renamed::RENAMED_KEYS`）：`Settings` 上的字段随之变成 `permissions`、`files`、`turn`、`agents`、`prompt`、`app`、`web` 等块，`providers` 是带 `id` 的列表，`model.order` 排 provider、`model.tasks` 写档位。下面 0.3.20 条目里的 `history.location`、`worktree_dir` 现在是 `app.history`、`app.worktree_dir`。
  - 删掉 `model.fallback_model` 与 provider 条目里的明文 `api_key`；新增 `provider_switched` 事件、`providers_unavailable` 错误码与按 provider 的 token 配额（用量落在状态根的 `quota/`）。
  - 库式用法：`model_router::build_task_router_with` 多一个 `quota_dir`（用量账本的目录，宿主路径同样记配额）；`QuotaLedger::record` 改为返回 `Result`；`StreamParams` 多一个 `usage_tap`（流开始时就报出输入侧用量，中途断开的调用也计入配额）。
- **0.3.20**
  - 引擎名与产品名（#90）：`EmbedConfig.product_name` 改名为 `EmbedConfig.product`（含义不变，`EmbedError::InvalidProductName` 的消息前缀随之是 `product`）；新增 `EmbedConfig.engine`（缺省 `agix`）与 `EmbedError::InvalidEngineName { name, reason }`；`EmbedConfig.project_dir` 改为 `Option<String>`，`None` 取 `.<engine>`，原来写 `config.project_dir = ".codix".into()` 的改成 `Some(".codix".into())`。0.3.19 的“产品名别名”去掉：改了产品名不再给基础库挂第二个名字，基础库只在引擎名下（缺省 `agix`，与产品名无关）。库式用法：`js_lib::{alias_for, install_as}` 删掉，换成 `js_lib::install_named(ctx, name)` 与 `js_lib::check_global_name`；`ptc::Library::for_product` → `for_engine`（`shown()` → `global()`）；`script_host::QuickJsEngine::for_product` → `for_engine`，`script_host::bindings::engine_for` 收引擎名；`base::paths::known_state_roots` 多一个 `&Layout`；新增 `Layout::{with_engine, with_project_dir, engine, state_dir_names}`。
  - 宿主 JS 库（#91）：新增 `EmbedConfig.js_libs`（缺省空）与 `EmbedError::InvalidHostLibrary { namespace, reason }`；类型 `agix::embed::{HostJsLib, HostCallContext, HostJsError, PackageSource}`（即 `base::interface::host_js`，另有 trait `HostFn`）。库式用法：`Assembly::js_libs`；`base::command::{ExtCommand, PluginFunction}` 多一个 `source: PackageSource`，`packages::manifest::Plugin::{source, from_host}`；`base::tool::ToolContext` 多一个公有字段 `via`（`ToolContext::new` 给 `"model"`）；`ptc::HostLibraries` 与 `RunExtTool::with_host_libraries`；`runtime::agent::Builder::host_js_libs`，`HostContracts` 多一个 `host_js_libs`。
  - 宿主调插件函数与宿主自带包（#92）：新增 `EmbedConfig.plugins`（缺省空）、`Assembly::host_plugins` 与 `SessionPool::register_host_plugin(dir)`；`Embedded::pool() -> &Arc<SessionPool>`；`SessionPool::{call_plugin, plugin_functions}` 与 `agix::session::{PluginCall, PluginCallTarget, PluginCallResult, PluginCallError, PluginFunctionInfo}`（协议上是 `plugin.call` / `plugin.functions` / `plugin.cancel`）。
  - 项目层目录分出 `state/`（#93）：记忆、scratchpad、团队、telemetry、`tool-results`、导入标记从 `<project_dir>/` 直下挪到 `<project_dir>/state/`，打开项目时旧位置的这几项一次性改名过去（旧项是符号链接、`state/` 里已有同名的都不动，之后只读新位置），`.gitignore` 的引擎条目只剩 `state/`、`settings.local.json`、`mcp.local.json`，旧的行不替人删；模型不问就能写的只有 `state/scratchpad/`、`state/memory/`，直接放在 `<project_dir>/` 下的 `scratchpad/`、`memory/` 不再算工作区；`product` 的小写不能是 `STATE_RESERVED_NAMES` 里的名字。库式用法：`base::paths::{LOCAL_DATA_IGNORED_FILES, LOCAL_DATA_IGNORED_DIRS}` 删掉，换成 `LOCAL_CONFIG_FILES`（`local_data_gitignore()` 另加 `state/`）；`ConfigRootAt::writable_entry` → `writable_state_entry`（判的是 `state/` 直下的名字），新增 `ConfigRootAt::state_dir`；新增 `entry::{STATE, HISTORY}`、`PROJECT_STATE_ENTRIES`、`PROJECT_STATE_ENGINE_DIRS`、`STATE_RESERVED_NAMES`、`Layout::{project_state_dir, host_state_dir}`、`PathSettings::project_state_dir`、`ConfigPaths::{project_state_dir, local_state_dir}`；`ConfigPaths` 的 `local_memory_dir`、`scratchpad_dir`、`telemetry_dir`、`tool_results_dir`、`imported_marker` 与 `Layout::teams_dir` 改指 `state/` 下。
  - 会话历史按项目存放（#94）：新增 `EmbedConfig.history: Option<HistoryLocation>`（缺省 `None`，即按全局设置 `app.history`，缺省 `Global`，行为同前）与类型 `agix::embed::HistoryLocation`；设置新增 `history.location`（`global` | `project`，只认全局层）。库式用法：`Assembly::history`；`JsonlHistoryStore::with_project_history` 与 `agix::history::{ProjectHistory, ProjectHistoryDir, SessionIndex, SidecarLocator, HistoryError}`；`HistoryStore` 多两个有缺省实现的方法 `sidecar_locator(session)`（会话记忆、metadata、输入历史所在的目录，每次用时再定位）与 `history_dirs_for(path)`（导入导出要避开的目录），包装别的存储的实现要转发；`HistoryError::ProjectUntrusted { session, project }`；`session::session_memory::SessionMemory::in_sidecar(locator)`，`SessionMemory::path()` 改为返回 `io::Result<PathBuf>`；`CheckpointObjects::located(locator)`；`history::path` 新增 sidecar 文件名常量与 `session_index_file`。会话记忆不再由 runtime 按 `global_data_dir` 自己拼路径，而是问存储。

- **0.3.18** git worktree 从 `<项目>/<project_dir>/worktrees/<名>` 挪到仓库根的 `.wkts/<名>`，第一次建 worktree 时往仓库 `.gitignore` 加的是 `.wkts/`，不再是整个 `<project_dir>/`。目录名由设置 `worktree_dir` 改（只从全局层与场景层读，项目层写了丢掉并告警，`config.set` 写项目层得 `not_at_tier`）。不做兼容：旧位置的 worktree 不再被认作引擎的（要用就自己 `git worktree move`），用户 `.gitignore` 里已有的 `<project_dir>/` 一行不替人删。`<project_dir>/.gitignore` 的引擎条目（`base::paths::local_data_gitignore`）加上 `.imported.json`、`memory/`、`scratchpad/`、`teams/`，有项目的会话建起来时就补上：项目层目录里只有协作者共用的配置随仓库提交。新增 `base::paths::ensure_project_data_gitignore`（项目层目录是符号链接或在项目之外时不写）；`.gitignore` 是符号链接时两处引擎写入都不动它。库式用法：`base::paths::{PROJECT_WORKTREES_DIR, entry::WORKTREES}` 删掉；新增 `DEFAULT_WORKTREES_DIR`、`Layout::{with_worktrees_dir, worktrees_dir_name, worktrees_dir_names}` 与 `WorktreesDirError`，`Layout::{worktrees_dir, gitignore_entry, main_checkout_copies, worktree_of}` 改按 worktree 目录算，`Layout::new` 拒 `.wkts`；`Settings.worktree_dir`、`settings::NOT_FROM_PROJECT_KEYS`、`LoadedSettings::place_worktrees`；`tools::worktree::name_the_directories` 的占位符由 `{project_dir}` 换成 `{worktrees_dir}`；`WorktreeError::WorktreesDirIsALink`（worktree 目录是符号链接时不建）；`Layout::with_worktrees_dir` 拒以 `.git` 开头的名字；`tools::worktree::prune_orphan_worktrees` 删掉（没有调用方，它会删 worktree 目录里认不出的目录）。
  - 新增 `SessionPool::set_permission_mode(session_id, PermissionMode)`,回 `PermissionModeChoice { session_id, mode, requested, clamped }`(即 `session.setPermissionMode`);`SessionEvent` 多一个 `PermissionModeChanged(session_state::ModeChange)`(`ModeChange { mode, source }`,`ModeChangeSource { Host, Model }`);`base::context::Session` 多一个 `host_permission_mode`(trait 唯一的实现在 `session-state`,外部不用实现);`SessionState` 新增 `switch_permission_mode`、`permission_mode_changes`、`subscribe_permission_mode`。新增 `session_state::ModeCeiling` 与 `EngineAttachment::with_mode_ceiling`:挂上的引擎带着它时,`plan` 退出后要回到的模式(含从日志读回的)按它夹紧;不带(自己拼 `EngineAttachment` 的测试)照旧不夹。`base::interface::permission::Delegate` 多一个公有字段 `tightens_only`(`Delegate::new` 给 `false`)与 `Delegate::mode_under(parent)`:自己实现 `PermissionPolicy` 又判子 Agent 调用的,用 `mode_under` 代替 `parent.for_delegate(delegate.mode)`,只能收紧的 agent 类型才不会被 `bypassPermissions` 的父会话带走。
- **0.3.17** 嵌入配置：`EmbedConfig.project_dir`（项目层目录名，缺省 `.agix`）与 `EmbedConfig.product_name`（产品名，缺省 `Agix`），非法值分别以 `EmbedError::InvalidProjectDir { name, reason }`、`EmbedError::InvalidProductName { name, reason }` 在 `start` 返回；缺省时行为同前。
  - 改名的后果都不做回退读取：项目里已有的 `.agix`（改了 `project_dir`）不再被读，要保留里面的设置、规则、记忆由宿主搬进新目录，没搬之前它仍受保护，模型改不了它；Agix 名下平台路径的 managed settings 与 MCP 策略（改了 `product_name`）不再被读，放进新名字的目录或用 `managed_settings` / `mcp_policy` 点名；改名之前建的 worktree 分支 `agix/worktree-*` 不再被认作本实例的（孤儿清理删的是 `<新名字>/worktree-<名>`，旧分支留在仓库里，自己 `git branch -D`）。内建场景的展示名变成 `<名> Code` / `<名> Mini`（id 不变），OpenTelemetry 的 `service.name` 变成小写名。
  - 库式用法的签名：`scene::builtin(id, &Layout)`、`SceneRegistry::register_builtin(&Layout)`；`DaemonPaths::layout()`（有缺省实现，外部实现不用改）与 `StaticDaemonPaths::with_layout`；`LoadedSettings::load*`、`ConfigPaths::new`、`PathSettings::new`、`SensitivePaths::new` 多一个布局参数，`WritePolicy::new` 改收 `Arc<SensitivePaths>`；`packages::{discover_plugins, scan_plugins}`、`packages::install::install`、`packages::manifest::Plugin::load` 多一个 `&Layout`；`ModelFactory::build` 多一个 `network: Option<&Arc<dyn Network>>`（模型请求走的出口，带部署的 `User-Agent`；不用就忽略），`ModelFactoryRegistry::with_network`；`model_router::build_task_router` 多一个 `&Layout`；`PromptContext.project_dir` 换成 `PromptContext.layout`，`ScenePromptContext` 多一个 `layout`；`telemetry_otel::{start_otel, process_exporter}` 多一个 `service` 参数；`OAuthHttp::{for_server, for_configured}` 多一个 `User-Agent` 参数，新增 `McpAuth::set_user_agent`、`platform::LocalNetwork::with_user_agent`；`skills::bundled::bundled_body`、`settings::import::already_decided`、`permissions::ruleset::denied_read_paths`、`tools::worktree::{create_worktree, prune_orphan_worktrees}`、`DelegatorHookExecutor::in_worktrees_of` 各多一个布局（`bundled_body` 另加状态根）参数。删掉的：`settings::managed::path`、`mcp::policy::managed_policy_path`（平台路径只在 `Layout::{machine_dir, managed_settings_file, mcp_policy_file}`）、`base::paths::{WORKTREE_BRANCH_PREFIX, QUARANTINE_PREFIX}`（换成 `Layout::{worktree_branch_prefix, quarantine_prefix}`）、`permissions::path_rule::main_checkout_copies`（换成 `Layout::main_checkout_copies`）、`base::paths::sensitive` 里写死 `.agix` 的常量与按名字判定的自由函数（换成 `SensitivePaths` 的方法）、`SkillManager::register_bundled`、`history` 的 `ProjectSessionState` 与 `project_local_dir` / `project_session_state_file`、设置里的 `model.auth_token`（没人读；模型调用的凭据只来自 `providers` 与 `CredentialSource`，写了它现在是未知键）。
  - `SessionPool`：`mcp_status(project_root, scene)` 返回 `Result<McpStatusList>`（以前无参、返回 `Vec<McpServerStatus>`，只报 daemon 自己那一对）；`add_mcp_server` / `update_mcp_server` 多一个 `tier`，新增 `remove_mcp_server`、`set_mcp_enabled`、`mcp_reconnect`、`set_settings(SettingsChange)`、`list_models`、`create_session_as`（`create_session` 签名不变）；`set_session_model` 多一个 `ReasoningChange`；`ProviderTier` 删掉，改用 `SettingsTier`（多了 `Scene`）；`ProviderView.models` 由 `Vec<String>` 改成 `Vec<ModelEntry>`；`CommandInfo` 多 `tier` / `dir`。新增取值：`BusyReason::Importing`、`ModelErrorKind::NoCredentials`、`FailureKind::NoCredentials`、`AnthropicError::NoCredentials`；`AnthropicError::{RateLimited, Overloaded}` 多一个 `body` 字段（服务自己的说明会进轮错误）。
  - 行为变化：没有凭据不发请求——缺省 client 缺 key 时，每一次模型调用在发出任何网络请求前以 `no_credentials` 失败；宿主的 `CredentialSource` 答空串等于没给，配置里的 provider 为此在 `assemble` 失败（不再带空密钥建 client）。`setProvider`、`mcp.addServer` / `updateServer` 不再替不存在的项目建出目录（`PROJECT_NOT_FOUND`），加载器会丢掉的值直接拒绝；写设置不再重载两次（写入方写完自己重载一次，监视器认得这是引擎自己写的）；`config.setProvider` 与 `config.set` 不再把没连过的 (项目, 场景) 的 MCP server 连起来，`mcp.status` 也不启动没连过的对；`created_at` 报会话原始创建时间，不是这次带回内存的时刻；项目层目录名必须以 `.` 开头；错误响应体最多读 4096 字节、等 5 秒；出网跟随重定向换了源时只带描述请求的头；路径名“不分大小写”的比较按文件系统的完整折叠（`ſ`、`ß` 等）；`WebSearch` 与子 Agent 跟着 `session.setModel` 换模型；对话与子 Agent 的模型名分别取 `task_models.main` / `task_models.subagent`，没有 `task_models` 条目的任务的模型名与 client 来自同一个 provider。
- **0.3.16** `EmbedError::GroupsHeld`：`groups_file` 里的拥有者是另一个活着的进程。`orphans::{live_owner, stop_persisting}`。`EmbedConfig.groups_file`：进程组的回收与登记不再要 `instance`，不给 `instance` 的宿主给它一个文件就有；给了 `instance` 而不给它，行为同前。`EmbedError::GroupsInProcess`：一个进程里已有一个不带 `instance` 而登记进程组的 `Embedded` 在跑时，再起一个要登记的。
- **0.3.15** 新增 `agix::embed` / `daemon::embed`：`EmbedConfig` 十三个字段（上表除 `groups_file`、`project_dir`、`product`、`engine`、`plugins`、`js_libs`、`history` 外）与 `EmbedConfig::new`，`EmbedError::{UnknownScene, AlreadyRunning, Startup, InstanceInProcess}`，`Embedded::{start, serve_stream, serve_unix, closed, shutdown}`。自己照抄 agixd 启动过程的宿主（`load_daemon_config` + `assemble::pool` + `DaemonServer::new`，进程内 `accept_connection` + `dispatch_public`，继承来的流自己写读循环）换成 `Embedded::start` 加 `serve_stream`；进程内连接的回复改走出站队列，往队列里塞屏障帧的做法可以删掉。同一版起 agixd 与 `assemble::pool` 没有凭据也能启动。
