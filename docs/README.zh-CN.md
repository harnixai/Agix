# Agix

**English: [README.md](../README.md)**

**一个拿来做产品的 agent 引擎,而不是一个拿来聊天的助手。**

---

## 目的

Agix 是 agent 产品底下的那台引擎:它运行一个**会话**——装配提示词、调模型、执行工具、
把上下文控制在预算内、写历史、把活委派给子 Agent——并把这一路上的每个决策都留给建在
它上面的产品。这里没有交互界面,那是宿主的事。

它面向的是在它之上做二次开发的人:

- **Rust 应用**把引擎当库链接进来,直接驱动一个 `Agent`。
- **其它任何东西**——IDE 插件、Web 前端、用别的语言写的编排器——运行仓库里附带的
  参考宿主 `agixd`,经 Unix socket、TCP 或 WebSocket 用 JSON-RPC 和它说话。

两者是同一台引擎。

---

## 为什么选 Agix

**一套引擎,机房里成百上千地铺开,板子上照样跑得动。** Agix 为大规模部署和嵌入式部署而
生:一个小小的二进制,心里有数的资源占用,没人盯着也能一直干下去的循环。

1. **小到哪里都放得下。** `agixd` 是一个 11 MB 的二进制,不用装任何运行时;空闲时内存约
   10 MB,启动约 30 ms。一个真实任务——四个子 Agent 并行外加一段程序,6 分钟里 91 次模型
   调用——内存峰值不到 30 MB,CPU 总共约 8 秒(M2 Max 上 `release` 构建实测)。异步工作
   线程用 `app.worker_threads`(`--worker-threads`)按硬件配,小板子上一个就够;用不到的
   模块在构建档位里整个去掉。
2. **一个进程,成批会话。** 一个 daemon 同时服务几十个会话,跨项目、跨场景,走 Unix
   socket、TCP 或 WebSocket。闲置的会话被回收,用到时从磁盘恢复。崩溃留下记录,残留的命令
   在下次启动时清掉,和别的服务一样交给 systemd 或 launchd 托管。
3. **放手不管也安全。** 场景标成无人值守,就没有任何东西等人:要审批的事直接拒绝并说明原因,
   写入不出你给的根目录,`Bash` 只在操作系统沙箱里跑(macOS 用 Seatbelt,Linux 用
   bubblewrap)。权限只来自配置,不来自任务文本,注入的内容给不了自己任何权限。
4. **同样的 token,干更多的活。** 有了 `RunCode`,模型写一小段程序来驱动工具——并行、循环、
   分发给子 Agent 并按 JSON Schema 校验结果——只把结果带回对话。目标模式让会话一轮接一轮干到
   完成为止。固定前缀约 5.8K token,跨会话共享厂商的前缀缓存;长会话自己压缩、接着干。在六个
   工程任务上,与另一个开源 agent 框架用同一个模型对比:**总耗时少 39%,提示 token 少 63%,
   CPU 只用五分之一,内存只占十三分之一,每次都做对**([实测](#实测))。
5. **主导权在你的产品手里。** 每个会话、任务、子 Agent、目标和询问,都能通过同一套 JSON-RPC
   看到、叫停、修改。47 个扩展点,用 Rust 或 QuickJS 脚本替换或拦截任何子系统,不用
   fork。每个会话都是只追加的日志,可以恢复、分叉、导出,也能不联网逐字节回放。

---

## 实测

六个工程任务,Agix 0.3.15 与 DeepSeek Harness(DSH)0.2.2-rc.1 用同一个模型对比。每项跑
三轮取中位数,36 次运行全部做对。

| 任务 | 形态 | 提示 token(Agix / DSH) | 墙钟 s | CPU s | 内存峰值 MB |
|---|---|---|---|---|---|
| `stats` | 照精确规格写一个小程序 | 29.3K / 44.5K | 13 / 28 | 0.56 / 3.55 | 18 / 278 |
| `inventory` | 修好一个包里的三个缺陷,再加一个命令行入口 | 75.0K / 123K | 27 / 50 | 0.81 / 3.53 | 21 / 283 |
| `biglog` | 回答一个 1.4 MB 访问日志里的问题 | 50.1K / 33.0K | 13 / 9 | 0.66 / 1.99 | 18 / 281 |
| `shop` | 陌生项目上同一会话两轮:先修缺陷,再加功能 | 125K / 127K | 21 / 27 | 0.71 / 4.34 | 21 / 283 |
| `audit` | 二十四个模块分给四个子 Agent 审查,修好并列出坏了的 | 369K / 1.74M | 70 / 141 | 3.05 / 16.95 | 22 / 303 |
| `migrate` | 三十个模块里 58 处调用迁离弃用的接口 | 153K / 128K | 28 / 28 | 1.00 / 3.01 | 25 / 282 |
| **合计** | | **802K / 2.19M** | **172 / 282** | **6.8 / 33.4** | |

- **资源。** 这几列最稳,三轮之间几乎不变。Agix 的 daemon 从没超过 25 MB。一个 daemon 承载
  所有会话;DSH 是每个会话一个约 280 MB 的 node 进程。
- **token。** token 省下的大头在 `audit`,也就是子 Agent 那个 workflow。`biglog` 和 `migrate`
  上 Agix 花得更多:它多走了一两次模型调用,去交叉核对自己的答案。
- **波动。** 提示 token 跟着模型调用次数走,单次运行两边都可能差两三倍,所以要比中位数,不要比
  单次。

怎么跑的:

- **配置。** Agix 跑 release 版 `agixd`、`harnix` 场景,无人值守,缺省权限策略;DSH 跑
  `--profile headless`。两边都用 `deepseek-flash`,thinking 开、effort high。两边轮流跑,不同时跑。
- **对错。** 产物用模型没见过的输入实际运行来判定。
- **token。** 取厂商报的数,从两边各自前面的录制代理读,子 Agent 的调用也算在内。
- **CPU 与内存峰值。** 取回收 harness 进程时内核的计数,子进程也算在内;Agix 算的是 daemon 的
  整个生命期,启动也在内。
- **墙钟。** DSH 的墙钟含每轮的进程启动;Agix 的 daemon 这时已经起好了。
- **机器。** 2026-11-15 在 M2 Max 上测(macOS 虚拟机,8 核,16 GB)。

DSH 不在本仓库里。怎么装、怎么自己跑这个对比,以及每个任务、每个指标的含义,见
[`benchmark/compare_dsh.md`](benchmark/compare_dsh.md);这次测试的完整记录(版本、环境、每一轮)
见 [`benchmark/2026-11-15-agix-0.3.15-vs-dsh-0.2.2-rc.1.md`](benchmark/2026-11-15-agix-0.3.15-vs-dsh-0.2.2-rc.1.md)。

---

## 特性

**部署**
- `agixd`:JSON-RPC 2.0(NDJSON)daemon,走 Unix socket、TCP 或 WebSocket;后两者带
  令牌认证。
- 也可以把引擎嵌进自己的进程:用 builder 建一个 `Agent`,直接用 daemon 的整个会话池,或者用
  `agix::embed`——agixd 的参数进、协议连接出。
- `release` 构建出 11 MB 的二进制;`release-fast` 体积翻倍,换一点速度和 panic 时展开。
- 构建档位(`minimal`、`coding`、`full`),按模块开关:MCP、子 Agent、skill、web 工具、
  结构化输出、`RunCode`、WebSocket、脚本、插件、沙箱、OpenTelemetry。
- `app.worker_threads`(`--worker-threads` / `AGIX_WORKER_THREADS`);会话上限,超出时回收
  最久未用的,与闲置超时(`app.session_cap`、`app.session_idle_timeout_secs`);一个进程服务多个场景。
- 发现文件,客户端按场景找 daemon。SIGTERM 正常关闭;崩溃记录;被强杀后清理进程组。

**模型**
- Anthropic 与 OpenAI 兼容两种接口,托管模型和本地模型(llama.cpp、Ollama、vLLM 等)都能接。
- 按调用种类路由(`main`、`subagent`、`compact`、`memory`、`web_search`、`web_fetch`),
  会话可单独选厂商,中途可换模型。
- 退避重试,外加恢复策略:换模型、压缩后重试、调高 `max_tokens`。
- 对缓存友好的提示词:会话自己的信息放在第一条消息里,不常用的工具用 `ToolSearch` 按需加载,
  压缩时发出缓存编辑。

**自主运行**
- 目标模式:自动一轮接一轮,直到完成、受阻或预算用尽。
- 定时任务;带校验的计划模式;待办清单。
- 后台跑的 shell 命令、监视器、子 Agent 和程序,结束时都发通知回来。
- `RunCode`:一段调用会话工具(包括子 Agent)的 JavaScript 程序,跑在内嵌的 QuickJS 里。
- `RunExt`:跑插件带来的命令,模型写一行命令行(`forge spec list`),插件的 JavaScript 做机械的那部分。

**多 Agent**
- `Agent` 工具,带类型的 Agent:前台或后台,各有自己的模型和工具,结果按 JSON Schema 校验。
- 团队,有信箱和共享记忆。
- 子 Agent 跑在挂在父会话下的侧链会话里。
- 并发和深度上限;权限、hook、预算随委派传下去。

**工具**——46 个内建:文件、shell、web、任务、计划、目标、定时、git worktree、skill、团队、
MCP、询问、通知、监视、结构化输出(见下文[能力](#能力))。

**上下文**
- 先做 LLM 摘要,再退到更便宜的手段;有预测式触发、熔断,阈值可配。
- 会话记忆与召回。
- skill 目录。
- token 和输出预算。

**安全**
- 固定的权限顺序:先拒凭据(任何模式都拒),然后是工具自身的检查,再是规则(deny 永远优先),再是路径安全,最后是权限模式。
- 权限模式:`default`、`acceptEdits`、`plan`、`dontAsk`、`bypassPermissions`。
- shell 命令跑在操作系统沙箱里,git hook、git 配置、Agent 设置在里面只读;引擎自己的 git
  也在沙箱里跑。
- 无人值守场景只拒绝、不询问。
- 权限询问超时一律按拒绝处理。
- worktree 隔离。
- 工作区信任:用户没信任的项目按插件级权限对待——它自己的设置只能收紧,它的 hook、MCP
  server、skill 命令与授权、更宽的 agent 类型,以及决定数据送到哪里的设置(provider、端点、
  instruction_files)都等信任之后才生效(在全局层或经 RPC 确认;缺省不信任,
  [ARCHITECTURE.md](ARCHITECTURE.md) §6.6)。

**宿主控制**
- 九十二个 RPC 方法:创建、恢复、分叉、重建、回滚、导出、导入会话;读历史,读下一次模型调用的确切上下文,读会话的模型调用录像,按内容搜索会话;
  答复权限询问和问题。
- 启动、发消息、停止、跟随任务,按字节偏移读任务输出。
- 换模型、提示词或权限模式;管理场景、MCP 服务器、插件、定时任务,以及信任哪些项目。
- 宿主在自己的连接上给所有会话提供工具与 hook(`daemon.setHostTools`、`daemon.setHooks`),
  给宿主工具报进度,给单个会话挂它自己的 skill 目录(`options.skill_dirs`)。

**扩展**
- 公开的 47 个扩展点:31 个契约、10 个拦截点、6 个注册点。
- 编译进宿主的 Rust 插件,与 QuickJS 脚本;可以打成插件包安装,安装时强制披露。
- 二十九个时机的 hook,后端有 command、prompt、HTTP、agent 四种,宿主连接也能经 RPC 登记 hook
  ([hooks.md](hooks.md))。
- MCP 客户端(stdio、SSE、streamable HTTP、WebSocket),按 MCP 2026-07-28 协商,对 2025-11-25 的
  server 回落到 `initialize`;server 在调用中向用户的询问推给宿主作答。声明式场景;自定义 Agent 类型。
- MCP 授权(2026-07-28 规范,OAuth 2.1):发现、客户端 ID 元数据文档或动态客户端注册、PKCE、
  RFC 9207 `iss` 校验、刷新;登录由宿主经 RPC
  驱动,回调走回环或宿主自己的地址;无人值守的 server 用 client credentials 或企业托管授权;
  令牌存在 0600 文件或宿主自己的存储里([mcp_auth.md](mcp_auth.md))。
- 企业 MCP 策略:按 server 名、命令、URL 的允许与拒绝列表,从用户写不了的系统路径读
  (或 `--mcp-policy`);策略读不了就拒绝所有 server([ARCHITECTURE.md](ARCHITECTURE.md) §6.4)。
- 部署方的 managed 设置:从用户写不了的系统路径读(或 `--managed-settings`),其中的 deny / ask
  在 `bypassPermissions` 下照样生效,还能关掉 bypass;读不了就拒绝所有工具调用
  ([ARCHITECTURE.md](ARCHITECTURE.md) §6.1)。

**记录**
- 只追加的 JSONL 历史:可恢复、可分叉,崩溃后可重建。
- 22 种遥测事件,可导出到 OpenTelemetry([telemetry.md](telemetry.md))。
- 录制器:不联网,逐字节回放;录像随会话保存,经 RPC 读(`session.calls`)。
- 会话导出成 zip 或目录,子 Agent 一起带上。

---

## 架构

一个分层的 Rust 工作区。依赖只向下指,反过来的依赖会让
`tests/guards/tests/deps.rs` 失败。

```
 外壳      daemon ─────────── agixd:命令行、进程生命周期
 装配      assembly ───────── 会话池、(项目, 场景) 绑定、provider 路由、模块清单;
             │                JSON-RPC 服务与实例发现
 模块      agents(+team) · mcp · skills · script-host · packages
 内核      runtime(K3)→ hooks · tools · session · compaction(K2)
             → model · scene · history · telemetry · permissions · ptc ·
               settings · memory · prompt-files · session-state(K1)
 本机      platform ───────── 本机的进程、文件、网络与 OS 沙箱
 契约      core(包名 `base`)── trait、共享数据类型与纯判定
```

内核 crate 从不依赖模块 crate,只有 `assembly` 与 `daemon` 知道有哪些模块。一轮是固定的步骤
骨架,每一步上的决策是开放的;一轮只有一个出口,所以 `TurnComplete` 的意思是"空闲,且已落盘"。
每次工具调用——模型发起的、`RunCode` 的子调用、skill 内嵌命令——走同一条管线。历史是只追加的
日志,投影成模型看到的消息。每个 agent 是一个独立任务,之间只经通道通信,不共享可变状态。
工具经四个契约(`Process`、`FileSystem`、`Network`、`Sandbox`)碰到机器,权限按固定次序裁决,
deny 永远优先。设计全文——一轮、会话与场景、历史日志、执行层、权限与信任、锁、磁盘布局——
在 [`ARCHITECTURE.md`](ARCHITECTURE.md)。

### 扩展面

引擎公开一份从代码生成的接缝清单:**47 个扩展点**——31 个**契约**(换掉一个子系统)、
10 个**拦截**(坐在进行中的某件事的路径上)、6 个**注册**(往一个集合里贡献)。用 Rust
就实现 trait 交给 builder;不重新编译引擎的,经下面几条路够到其中一部分:

| 途径 | 够到什么 | 代价 |
|---|---|---|
| **脚本**(QuickJS,默认) | 九个点:四个写提示词、两个围着工具调用、两个围着模型调用、记忆召回 | 一个 `.js` 文件加一行配置;每次调用微秒级 |
| **插件包** | MCP 服务器、脚本、声明式场景、agent 类型、skill、模型经 `RunExt` 跑的命令 | 一个带 `plugin.toml` 的 zip;要给模型看的文本安装时全部披露 |
| **Hooks** | 一轮里的二十九个时点、四种后端:命令、提示词、HTTP、agent | 观察、改写输入、拦下工具调用、结束一轮 |

另外,任何说 MCP 的东西本来就是扩展。脚本和 `RunCode` 跑在同一套内嵌 QuickJS 里:限额
限的是开销,不是恶意代码,所以脚本能做什么按它从哪来定。引擎不带 WebAssembly 运行时,
需要的宿主在 Rust 接口上自己做。

### 构建档位

| 档 | 装配什么 |
|---|---|
| `minimal`(`--no-default-features`) | 只有内核:模型、turn loop、场景、会话、遥测、hooks、文件与 shell 工具 |
| `coding` | + MCP、子 Agent 与团队、skills、web 工具、structured output、程序化工具调用(`RunCode`) |
| `full`(默认,另带 `scripts`、`plugin-packages` 与 `sandbox`) | + WebSocket 监听 |

`daemon.ping` 报出构建带了哪些模块;属于没编入的模块的方法回 `METHOD_NOT_FOUND`,并
指明是哪个模块。

### crate 一览

| 层 | crate | 职责 |
|---|---|---|
| 契约 | `core`(`base`) | trait、消息、提示词类型、扩展点清单、纯判定 |
| 本机 | `platform` | 执行层契约背后的本机进程、文件系统、网络与 OS 沙箱 |
| 内核 | `runtime` | turn loop、流式工具执行、`Builder`、`Module` |
| 内核 | `model` | Anthropic Messages、OpenAI Chat Completions 与 Responses 适配,重试与退避 |
| 内核 | `tools` | 内建工具(`web`、`structured-output` 两组是 feature) |
| 内核 | `scene` | 内建场景、`SceneRegistry`、声明式场景 |
| 内核 | `history` · `session` · `compaction` | transcript 日志、会话状态、上下文压缩 |
| 内核 | `permissions` · `hooks` · `telemetry` | 规则引擎、生命周期 hooks、遥测与录制 |
| 内核 | `settings` · `memory` · `prompt-files` · `session-state` | 设置分层与项目信任、文件式记忆、读进提示词的 markdown(`SKILL.md`、`AGENTS.md`、规则)、一个会话的可变状态 |
| 内核 | `ptc` | `RunCode`:模型写的程序,在内嵌的 QuickJS 里跑([`ptc.md`](ptc.md)) |
| 基础库 | `js-lib` | 每个内嵌 QuickJS 都有的全局 `agix`:YAML、glob、路径、JSON Schema([`agix_js.md`](agix_js.md)) |
| 模块 | `agents` · `team` | `Agent` 工具、agent 类型、团队 |
| 模块 | `mcp` | MCP 客户端与工具适配、MCP OAuth 授权 |
| 模块 | `skills` | skill 目录与 `Skill` 工具 |
| 模块 | `script-host` | QuickJS 载体 |
| 模块 | `packages` | 插件包:manifest、安装、缓存、披露 |
| 装配 | `assembly` | 会话池、配置、provider 路由、JSON-RPC 服务 |
| 外壳 | `agix` | 嵌入门面:按主题再导出,自身不含逻辑 |
| 外壳 | `daemon` | `agixd` 二进制 |

`crates/` 下 26 个 crate,加上 `daemon/`、七个测试成员与 `examples/plugins/rust` 的示例插件,
工作区共 35 个成员。

---

## 快速开始

### 运行 daemon

```sh
git clone https://github.com/harnixai/agix && cd agix
export ANTHROPIC_AUTH_TOKEN=sk-...        # 或 ANTHROPIC_API_KEY;兼容端点另设 ANTHROPIC_BASE_URL
cargo run --release -p daemon --bin agixd # 监听 ~/.agix/scenes/code/daemon.sock
```

和它说话——每行一个 JSON 对象:

```sh
sock=$HOME/.agix/scenes/code/daemon.sock
echo '{"jsonrpc":"2.0","id":1,"method":"session.create","params":{"project_root":"'$PWD'"}}' \
  | socat - UNIX-CONNECT:$sock
# → {"jsonrpc":"2.0","id":1,"result":{"session_id":"…",…}}

echo '{"jsonrpc":"2.0","id":2,"method":"session.run_turn",
       "params":{"session_id":"<id>","message":"列出这个工作区里的 crate"}}' \
  | socat - UNIX-CONNECT:$sock
```

一轮在跑时,`session.event` 帧不断推回来(文字增量、工具调用、工具结果、询问、任务
进度);这一轮结束时响应到达。常见的客户端用一条连接订阅会话(`session.subscribe`),
用另一条驱动它。协议参考 [`daemon_rpc_protocol.md`](daemon_rpc_protocol.md) 里记了
九十二个方法——会话、历史与上下文、任务与子 Agent、场景、配置、项目信任、MCP、插件、定时任务
——每个文档里写的响应形状都有测试拿真实 daemon 的回答对照。

常用参数:`--scene` / `--scenes`(服务哪些场景)、`--listen` / `--listen-ws` 加
`--token`(带认证的 TCP / WebSocket)、`--unix-socket-auth`(unix socket 也要认证)、`--mcp-policy`、`--managed-settings`,
以及 `--set KEY=VALUE`:在所有设置文件之上设任何一个设置(`--set permissions.mode=plan`、
`--set 'files.read_dirs=["/data"]'`、`--set turn.max_model_calls=50`;值先按 JSON 读,读不了按字符串;
可重复)。`--set` 的简写:`--max-tokens N`(`model.max_tokens`)、`--permission-prompt-timeout N`
(`permissions.prompt_timeout_secs`)、`--session-cap N`(`app.session_cap`)、
`--session-idle-timeout N`(`app.session_idle_timeout_secs`)、`--worker-threads N`
(`app.worker_threads`;`AGIX_WORKER_THREADS` 排在所有参数之前,参数给了以参数为准)。与 `app`
的其它键一样,后三个也可以写在全局 `settings.json` 里,不必写进服务的命令行。没有设置读的键、
旧键、放不进的值让 daemon 在启动时就退出,报出是哪个 `--set`。
每个运行中的 daemon 在 `~/.agix/daemon/instances.d/` 下写一个发现文件,客户端按场景
找到它,不用拼路径。

### 嵌入引擎

当库用——一个 agent,直接驱动:

```rust
let (mut agent, mut events, _input) = runtime::agent::Builder::new()
    .scene(Arc::new(scene::code::spec(Interaction::Interactive)))
    .model(Arc::new(model::adapter::AnthropicModel::new(client)))
    .settings(Arc::new(settings))
    .build()?;

tokio::spawn(async move {
    while let Some(event) = events.recv().await {
        if let AgentEvent::TextDelta { text, .. } = event { print!("{text}"); }
    }
});
agent.run_turn("列出这里的文件".into(), "turn-1".into(), CancellationToken::new()).await?;
```

场景、模型、设置三样必需;其余都有能用的默认,每个默认都是可以替换的 trait。完整程序
在 [`crates/runtime/examples/minimal.rs`](../crates/runtime/examples/minimal.rs):

```sh
ANTHROPIC_API_KEY=... cargo run --release -p runtime --example minimal -- "列出这里的文件"
```

当会话池用——把 daemon 的引擎放进自己的进程,设置分层、路由、历史布局、模块都相同,
再加上自己的场景:

```rust
let pool = agix::pool::assemble(
    &agix::pool::load_daemon_config("claude-sonnet-4-6", None, None, "code", &paths),
    Arc::new(agix::scenes::code::spec(agix::scenes::code::Interaction::Interactive)),
    agix::pool::Assembly { scenes: vec![my_scene], ..Default::default() },
)
.await?;
```

`agixd` 就是这一调用加上命令行与监听器;工作区里有别的地方用第二种方式组装会话池,
测试就会让构建失败。

当协议端点用——`agix::embed`,`agixd` 自己就从这里起:`EmbedConfig` 把 `agixd` 的参数当字段,
`Embedded::start` 起引擎,`Embedded::serve_stream` 在本进程的任意字节流上服务 JSON-RPC 协议
(`serve_unix` 在 socket 上),看不到别的 Agix 类型。它是 0.x 里唯一承诺稳定的 Rust 面
([`agix-sdk.md`](agix-sdk.md) §10)。嵌入方依赖的是 `agix` 这个 crate,指南见 [`agix-sdk.md`](agix-sdk.md)。

### 扩展它

- 给某一个点换个做法 → 写脚本:一个 `.js` 文件加 `settings.json` 里一行
  ([`extending_quickjs.md`](extending_quickjs.md))。
- 打包一组能力——MCP 服务器、脚本、场景、agent 类型、skill → 做成插件包
  ([`plugin_packages.md`](plugin_packages.md))。
- 在自己的构建里加工具、拦截或提示词贡献 → 写 Rust 插件(`runtime::plugin::Plugin`),交给
  builder,或经 `Assembly` 加给每个会话。同一个点上 Rust 插件和脚本都有时,Rust 先、脚本
  说最后一句。两种插件怎么写见 [`plugin_development.md`](plugin_development.md),
  `examples/plugins/` 下各有一个能跑的样例。
- 换掉一个子系统 → 实现它的 trait([`extension_points.md`](extension_points.md) 列出了
  每一个,以及代价和谁能用)。

---

## 能力

### 工具——46 个内建

| 类别 | 工具 |
|---|---|
| **文件** | `Read` `Write` `Edit` `Glob` `Grep` |
| **Shell** | `Bash`(沙箱、路径安全、可控超时) |
| **Web** | `WebFetch` `WebSearch` |
| **任务**（运行中的子 Agent、Shell 命令、监视器） | `TaskOutput` `TaskWait` `TaskStop` `TaskMessage` |
| **规划** | `EnterPlanMode` `ExitPlanMode` `TodoWrite` |
| **目标** | `GoalCreate` `GoalGet` `GoalUpdate`(目标让会话一轮轮自动推进下去,直到完成或受阻,或轮数上限、可选的 token 预算用完) |
| **定时** | `CronCreate` `CronDelete` `CronList` |
| **工作区** | `EnterWorktree` `ExitWorktree`(每个会话独立的 git worktree) |
| **协作** | `Skill` `Agent` `TeamCreate` `TeamList` `TeamDelete` |
| **团队成员**(只发给团队成员) | `SendMessage` `ReadMail` `ListPeers` `TeamMemorySave` `TeamMemoryRead` |
| **协议** | `MCP` `ListMcpResources` `ReadMcpResource`,另外每个发现的 MCP 工具一个 `mcp__<server>__<tool>` |
| **交互** | `AskUserQuestion` `PushNotification` `Monitor` `StructuredOutput` |
| **程序** | `RunCode`(一段调用本会话工具的 JavaScript 程序,只把返回值交回)、`RunExt`(已装插件回答的一行命令;有插件声明了命令才有) |
| **编辑 / 元 / 诊断** | `NotebookEdit` · `ToolSearch` · `Ping` `Sleep` |

`tools::register_builtin_tools` 装上其中 21 个自给自足的;其余需要会话状态,由 builder
或模块注册。延迟加载的工具只报名字,要用时经 `ToolSearch` 取 schema。`code` 场景不提供
`NotebookEdit`、worktree 两个工具、`Ping`、`Sleep` 和 `PushNotification`;
`StructuredOutput` 只在宿主给了输出 schema 时才有。场景分有人值守
(有人应答提示)和无人值守:`harnix`、`mini` 无人值守,`code` 创建时二选一,会话也能用 `options.attended` 改场景的缺省。无人值守的会话
没有 `AskUserQuestion` 和计划模式,需要批准的调用直接拒绝而不是去问,`Bash` 只在 OS 沙箱里跑——
见 `docs/ARCHITECTURE.md` §3.2.3、§6.3。

### 模型路由

```json
{
  "providers": [
    { "id": "ds", "vendor": "deepseek", "api": "openai_chat",
      "base_url": "https://api.deepseek.com", "api_key_env": "DEEPSEEK_API_KEY",
      "models": ["deepseek-v4-pro", "deepseek-v4-flash"],
      "tiers": { "strong": "deepseek-v4-pro", "balanced": "deepseek-v4-flash" },
      "quota": { "day": 20000000, "month": 300000000 } },
    { "id": "claude", "vendor": "anthropic", "api": "anthropic",
      "api_key_env": "ANTHROPIC_API_KEY",
      "tiers": { "strong": "claude-opus-4-6", "balanced": "claude-sonnet-4-6",
                 "fast": "claude-haiku-4-5" } }
  ],
  "model": { "order": ["ds", "claude"], "tasks": { "subagent": "balanced" } }
}
```

provider 有 `id`、`vendor`(`anthropic`、`openai`、`deepseek`、`qwen`、`vllm`、`sglang` 或
`generic`;决定怎么开关思考、是否回传推理内容、effort 怎么写)、`api`(`anthropic`、`openai_chat`
或 `openai_responses`)、`base_url`、存放密钥的环境变量名(`api_key_env`;设置里不写密钥,
嵌入引擎的宿主可以自己提供密钥)、`models`、`tiers`,以及可选的 `quota`。

每类调用要的是一个档位——`strong`、`balanced` 或 `fast`——按任务写在 `model.tasks`
(`main`、`subagent`、`compact`、`memory`、`title`、`web_search`、`web_fetch`;缺省依次是 `strong`、
`balanced`、`fast`,`web_fetch` 缺省不设),每个 provider 说明自己哪个模型对应哪一档:
`strong` 必填,`balanced` 缺省取 `strong` 的模型,`fast` 缺省取 `balanced` 的。

每次调用都从 `model.order` 的第一个 provider 起;它配额用尽、没有凭据、凭据被拒、连不上,或一直
回 5xx / 529 / 429 直到重试用完,以及回 402 时,换下一个;400 是请求本身的问题,不会换别家重试。
连不上的 provider 会被晾一小段时间。`order` 里的 provider 都不可用时,这一轮以
`providers_unavailable` 失败,并逐个说明原因。切换时丢掉上一家私有的推理内容,会话收到
`provider_switched` 事件(同名遥测事件也记一条)。

`quota` 是每 `day`、`week`、`month` 的 token 上限,可以只写其中几项。计数口径是未命中缓存的输入加缓存写入加输出,
缓存命中不计。周期按本机时区,周从周一、月从 1 号起。用量按 provider id 记在状态目录的
`quota/` 下,同一台机器上的所有实例共用;越过上限的那一次调用照常完成,从下一次调用起跳过这个
provider。`config.getProvider` 返回各周期的已用量与上限。

没有配 `providers` 时,引擎自己提供 `anthropic`,密钥取 `ANTHROPIC_AUTH_TOKEN` /
`ANTHROPIC_API_KEY`,三档都是 `--model`。会话可以把自己选的 provider 排到最前面,也可以指定档位或模型 id。
`WebFetch` 默认返回页面转成的纯文本(超过
50,000 字符分段,用 `offset` / `limit` 续读,续读会再抓一次页面);只有 `model.tasks.web_fetch` 配了档位时,才由模型读页面、按调用
的 `prompt` 作答。

### 上下文、遥测、录制

- **压缩**:先让 LLM 写摘要,不行再退到更便宜的办法(截断、微压缩、折叠),压到阈值减去
  系统提示词与工具定义之后剩下的大小。`compaction` 设置里可以打开两项:带熔断的预测式
  提前清理(`reactive`),以及每小时一次、以 cache edit 下发、不白扔已付过钱的提示缓存的
  清理(`clear_on_cache_expiry`)。超过 30,000 字符的工具结果(`Read`、`WebFetch`、`TaskOutput`
  自己分页的除外)整个存进项目的 `.agix/state/tool-results/`,模型拿到头尾预览和路径,用 `Read` 的 `offset`
  读后续;阈值是 `compaction.inline_result_chars`(`0` 关闭)。
- **遥测**:23 种结构化事件,可导出到 OpenTelemetry,每次调用四个 token 计数,不属于任何
  一轮的调用——压缩、记忆、hooks、搜索——也算进去。
- **录制**:把任意 `Model` 包一层,一个会话就成了自包含的目录;回放逐字节一致、不用联网。录像不脱敏
  ——提示词、工具参数与结果按发出去的原样记下——所以要像保管 transcript 一样保管它。daemon 里录制的
  会话把录制器记在自己的日志里,重启之后接着录,`session.calls` / `session.call` 经 RPC 读录像。

---

## 运行 `agixd`

| profile | 命令 | panic 时 |
|---|---|---|
| `release`(默认) | `cargo build --release -p daemon --bin agixd` | 中止进程;约 11 MB |
| `release-fast` | `cargo build --profile release-fast -p daemon --bin agixd` | 展开;任务里的 panic 只结束那个任务;体积翻倍,略快 |

agent 跑的命令各有自己的进程组,daemon 崩了它们还活着,所以 daemon 收两次尾:panic hook
在中止前停掉它们;被 `SIGKILL`、OOM 带走的,同名实例下次启动时停掉遗留的。崩溃记录在
`~/.agix/daemon/crashes/`。daemon 不会自己重启——放在 launchd 或 systemd 下、失败时
重启;systemd 下用 `KillMode=process`,由 daemon 而不是 systemd 决定停哪些后台命令。

配置按层叠加,优先级从低到高:内建默认 → `~/.agix/settings.json` →
`~/.agix/scenes/<scene>/settings.json` → `<项目>/.agix/settings.json`,每层可再带一个
`settings.local.json`,优先于它旁边的文件 → `cli` 层(`agixd --set`,或嵌入方的
`EmbedConfig.settings`),叠在每个项目、每个场景之上。完整的配置面是生成的
[JSON Schema](schemas/settings.schema.json)。值按字面取:`$VAR` / `${VAR}` / `${VAR:-默认}`
只在 MCP server 的 `command`、`args`、`env`、`url`、`headers` 里展开。引擎不再读的旧键会被忽略,
加载时的告警会指出取代它的新键。

层与层怎么合:

- 对象逐键合并;高层写的标量或列表替换低层的(项目里的 `model.order` 替换全局的)。
- `providers` 是定义,各层相加;某个 id 在先读的层里已经定义过,就以它为准,后面重复的忽略并告警。
- `permissions` 的规则列表(`allow`、`allow_exact`、`ask`、`deny`)不合并:各层的规则并列生效,
  deny 压过任何 allow。
- `hooks` 按事件合并:某一层列了某个事件,就替换低层对这个事件的列表。`mcp.servers` 按名字、按字段合并。
- `app` 只认全局层(和 `cli` 层);scene、项目层写了会被忽略并告警。
- `cli` 层按全局层的规矩合并,不受项目信任限制;`config.reload` 重读的是它下面的文件。
  `config.get {tier:"cli"}` 和 `daemon.doctor` 把它报成 `cli`。
- 部署的 managed 文件能写任何键(外加 `permissions.disable_bypass`),它写的压过各层、`cli` 层、宿主在内存里改的值和会话自带的
  `options.settings`;它的规则列表与各层并列,deny、ask 在 `bypassPermissions` 下照样生效。文件读不了就一个值都不套、拒绝所有工具调用。
- 用户没信任的项目只能收紧(见工作区信任):`providers`、`hooks`、`mcp.servers` 条目、
  `prompt.instruction_files`、`prompt.override`、`sandbox.network.proxy`、`telemetry.otlp_endpoint`、
  `recorder` 都等信任之后才生效,它的 `model.order` 只能排在它之上已定义的 provider。

值可能从哪来,优先级从低到高:

| 入口 | 谁写 | 作用到 | 约束 |
|---|---|---|---|
| 内建默认 | 引擎 | 全部 | — |
| `~/.agix/settings.json`(+ `.local`) | 用户 | 所有项目、所有场景 | 文件层里只有它能写 `app` |
| `~/.agix/scenes/<scene>/settings.json`(+ `.local`) | 用户 | 这个场景 | — |
| `<项目>/.agix/settings.json`(+ `.local`) | 项目 | 这个项目 | 用户信任之前只能收紧 |
| `cli` 层:`agixd --set`、`EmbedConfig.settings` | 启动 daemon 的人 | 所有项目、所有场景 | 不受项目信任限制 |
| 宿主在内存里改的值(`HostOverrides`) | 嵌入方 | 守护进程自己那一对照原样;别的对只带宿主新加的 | — |
| `session.create` 的 `options.settings` | RPC 客户端 | 这个会话,连同它的子 Agent、团队成员 | 只收会话作用域的键;不开 `permissions.client_may_loosen` 只能收紧 |
| managed 文件 | 部署方 | 全部 | 压过以上各层;有松紧次序的键下层更严时留下层的;读不了就拒绝所有工具调用 |

`session.create` 的 `model`、`provider` 不是设置:只选这个会话的对话用哪个模型。推理强度、思考开关与提示词是设置:
`options.settings` 的 `model.effort`、`model.thinking`、`prompt.append` / `prompt.override`,会话与它的子 Agent 一样用;
`session.setModel`、`session.setPrompt` 改的也是这一层。

各块:

| 块 | 内容 |
|---|---|
| `providers`、`model` | 模型 provider、每类调用要的档位、回退顺序([模型路由](#模型路由));`model.effort`、`model.thinking`(`auto`、`on`、`off`)、`model.max_tokens` |
| `permissions` | `mode`;字符串写的规则列表(`allow`、`ask`、`deny`;`allow_exact` 按字面比对内容,"每次都允许"记在这里);`prompt_timeout_secs`(提问没人答就拒绝);`client_may_loosen`(RPC 客户端能否要求比 `mode` 更宽的模式) |
| `files` | `extra_dirs`(像项目一样读写)、`read_dirs`(不问就能读;写 `"/"` 即任意处可读)、`allow_write`、`deny_write` |
| `sandbox` | `enabled`、`require`(平台没法强制沙箱时拒绝执行命令)、`allow_read`、`deny_read`(追加到内建的凭据列表)、`network`、`env`、`cache_dirs`、`reap_on_close`、`unattended_needs_network_policy` |
| `web` | `WebFetch`、`Ping` 的 `allowed_domains`,也是 `sandbox.network.mode: "allowlist"` 读的那份名单 |
| `turn`、`session`、`goal`、`schedule`、`agents`、`team` | `turn.max_model_calls`、`turn.token_budget`、`turn.parallel_tools`;`session.token_budget`;`goal.max_rounds`;`schedule.max_model_jobs`、`schedule.min_model_interval_minutes`;`agents.max_depth`、`agents.max_concurrent`;`team.enabled`、`team.stage_concurrency`、`team.max_members`、`team.member_idle_secs` |
| `compaction` | `threshold_tokens`、`keep_recent`、`reactive`、`clear_on_cache_expiry`、`inline_result_chars` |
| `prompt` | `append`、`override`、`instruction_files`、`language`、`path_style`(`absolute`、`relative`) |
| `hooks`、`scripts`、`plugins`、`mcp`、`skills` | 生命周期 hook([hooks.md](hooks.md))、脚本绑定、插件开关、`mcp.servers.<名>`(只写 `enabled` 的条目是对别处声明的 server 的开关)、`skills.allow_shell` |
| `memory`、`telemetry`、`checkpoints`、`recorder` | `memory.enabled`;`telemetry.enabled`、`telemetry.otlp_endpoint`;文件检查点;LLM 录制 |
| `app` | 只认全局文件层、`cli` 层与 managed 文件:`trusted_projects`、`trust_all_projects`、`history`、`system_proxy`、`worktree_dir`、`session_cap`、`session_idle_timeout_secs`、`worker_threads` |

几个本文别处没提到的开关:

| 设置 | 缺省 | 作用 |
|---|---|---|
| `providers[].models[].max_output` | 按模型名查 | 调用这个模型时 `max_tokens` 最多发多少,回复在上限处被截断后重试也最多抬到这么多 |
| `memory.enabled` | `true` | 文件记忆:记忆提示、每轮提醒里的索引、召回 |
| `turn.token_budget` | 无(不限) | 一轮能花的 token(未命中缓存的输入、缓存写入与输出,缓存命中不计,同 provider 配额),连同这一轮起的或派了活的子 Agent、团队成员,以及旁路调用(压缩、记忆、`WebSearch`、`WebFetch` 的抽取、`prompt` 与 `agent` hook)一起算;每次调用前先估计它会被计的输入(预计不由 provider 缓存供给的部分)并从剩余里扣掉,要的输出不超过扣完的数,扣完付不起一次调用时以 `budget_exceeded` 结束这一轮。这是估计不是预留:同时在跑的子 Agent 各自至多超出一次调用。会话只能经 `session.create` 的 `options.settings.turn` 把它调低 |
| `session.token_budget` | 无(不限) | 一个会话从创建起能花的 token,口径同 `turn.token_budget`,算它所有的轮——子 Agent、团队成员、旁路调用、定时任务与目标的轮都在内——跨 daemon 重启(累计记在会话日志里,`session.get` 的 `usage` 报它)。每次调用同 `turn.token_budget` 一样先扣掉输入的估计再判;剩余付不起一次调用时这一轮以 `budget_exceeded` 结束,之后的每一轮都不调模型、同样以它结束 |
| `schedule.max_model_jobs` | `20` | 一个会话里模型自己经 `CronCreate` 建的、还在的定时任务最多几个;再建被拒,理由在工具结果里。宿主经 `schedule.create` 建的不计也不受限 |
| `schedule.min_model_interval_minutes` | `1` | 模型建的周期任务两次触发至少隔几分钟(按它存活的 7 天算);`1` 即不在 cron 自己的分钟精度之外再加限制。一次性任务不受它限制:同时能有几个由 `max_model_jobs` 管,它们的轮花的算在会话上 |
| `goal.max_rounds` | `10000` | 模型设或改的目标(`GoalCreate`、`GoalUpdate`)最多能有几轮;给多了压到它。目标自己的 `token_budget`(经 `session.setGoal` 或 `GoalCreate` 设)在目标各轮花完时让目标转 `blocked`;`GoalUpdate` 只能调低它,调高归宿主(`session.updateGoal`) |
| `telemetry.enabled` | `true` | 每个会话在项目的 `.agix/state/telemetry/` 下写一份遥测 JSONL(无项目时在状态目录的 `telemetry/` 下) |
| `telemetry.otlp_endpoint` | 无 | OTLP 端点(不设看 `OTEL_EXPORTER_OTLP_ENDPOINT`);只在带 `otel` feature 的构建里生效 |
| `prompt.instruction_files` | `[]` | 再加几份指令文件,会话构建时按序读入,与 `AGENTS.md` 一样交给模型 |
| `hooks` | 无 | 生命周期 hook:事件名 → hook 列表([extension_points.md](extension_points.md)) |
| `skills.allow_shell` | `true` | `false` 时 skill 正文里的 `` !`cmd` `` 换成一行说明,不执行 |
| `team.enabled` | `false` | 给场景支持团队(`code`、`harnix`)的会话注册 `TeamCreate` / `TeamList` / `TeamDelete` |
| `team.member_idle_secs` | `1800` | 团队成员空闲这么久就停掉 |
| `sandbox.enabled` | `true` | `false` 时命令不进 OS 沙箱运行 |
| `sandbox.network.mode` | `unrestricted` | 沙箱里命令的出网:`unrestricted`、`deny_all`、`allowlist`(`web.allowed_domains`;后端不按域名放行,沙箱里的命令在 macOS 上只连得到名单里的 localhost,Linux 上整个断网)或 `proxy`——要按域名过滤,在前面放一个代理 |
| `sandbox.network.proxy` | 无 | `proxy` 模式下沙箱里的命令唯一能连的地址(`"127.0.0.1:3128"`,或 `{"http", "socks", "no_proxy"}`),经 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` 告诉命令;代理不在本机时 macOS 只能按端口限,Linux 要 `socat`,没有就整个断网 |
| `app.system_proxy` | `true` | 引擎自己的 HTTP(模型调用、`WebFetch` / `Ping`、MCP over HTTP、OAuth)跟随 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`;`false` 直连。daemon 启动时生效;与上面沙箱命令的出网无关 |
| `app.history` | `global` | 会话放在哪。`global`:状态目录的 `projects/` 与 `sessions/`。`project`:受信项目的会话放进它的 `.agix/state/history/<id>/`(worktree 里的放进主仓库的),`sessions/<id>/` 只留索引;不受信的项目照旧放全局。已经放在任一处的历史原地照读;项目里的历史只在项目受信时读。daemon 启动时生效 |
| `app.worktree_dir` | `.wkts` | `EnterWorktree` 与 `Agent` 的 `worktree` 把 git worktree 建在仓库根的这个目录下(`<仓库>/.wkts/<名>`),仓库的 `.gitignore` 也只加这一行;一个以 `.` 开头的路径分量,不能以 `.git` 开头,也不能是配置目录或别的工具的目录 |
| `app.session_cap` | `32` | 内存里留几个会话;再多一个就把最久未活动的空闲会话移出内存,下次用到时从磁盘恢复。daemon 启动时生效,`config.reload` 不改它 |
| `app.session_idle_timeout_secs` | `3600` | 会话没有活动多少秒后移出内存。daemon 启动时生效,`config.reload` 不改它 |
| `app.worker_threads` | 每个 CPU 核一个 | `agixd` 的异步工作线程数,至少 1。启动最先读一次,`config.reload` 不改它;嵌入的引擎跑在宿主的运行时上 |

| 变量 | 作用 |
|---|---|
| `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` | 模型凭据(或由宿主提供自己的凭据来源) |
| `ANTHROPIC_BASE_URL` | 兼容端点 |
| `AGIX_DAEMON_TOKEN` | TCP / WebSocket 握手 token |
| `AGIX_CONFIG_HOME` | 全局根目录,默认 `~/.agix` |

---

## 构建与测试

Rust 1.88+。测试不需要网络,也不需要 key。

```sh
cargo build --workspace
cargo test  --workspace                                      # 全部离线
cargo test  -p blackbox --features real-model --test real_model  # LIVE 层,对真实模型
```

从单元测试到组装好的产品:各 crate 的测试;一张行为网,让真实的 `Agent` 对着写好的
模型回答跑;`daemon/tests`,走真实 socket;架构守卫,查工作区的结构;`tests/blackbox`,经
JSON-RPC 驱动整个产品——一次对脚本化的模型服务器,一次(`live_*` 用例)对录制代理后面的真实
模型。每层怎么工作、怎么跑:[`tests/README.md`](../tests/README.md)。

---

## 文档

| 文档 | 用途 |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | 引擎怎么拼起来——分层、模块、一轮、会话与场景、历史、执行、并发、磁盘布局 |
| [upgrading-0.4.md](upgrading-0.4.md) | 从 0.3.x 升级应用:设置改名表、`options.settings`、协议与预算的变化、`EmbedConfig` |
| [agix-sdk.md](agix-sdk.md) | 把引擎嵌进自己的 Rust 进程:门面 crate、一个池、一轮、事件、宿主工具、关停,以及 `agix::embed` 入口 |
| [extension_points.md](extension_points.md) | 每个接缝:能换什么、能往哪里贡献、能拦截什么,代价与谁能用 |
| [hooks.md](hooks.md) | hook 协议:配置、四种后端、退出码、超时、payload、回答,每个事件在何时触发 |
| [telemetry.md](telemetry.md) | 遥测文件、信封与脱敏,每种事件与它的字段 |
| [daemon_rpc_protocol.md](daemon_rpc_protocol.md) | JSON-RPC 参考——方法、事件、错误码、认证 |
| [plugin_development.md](plugin_development.md) | 写 Rust 插件与 QuickJS 插件,样例在 `examples/plugins/` |
| [extending_quickjs.md](extending_quickjs.md) | 写脚本扩展 |
| [plugin_packages.md](plugin_packages.md) | 插件包的格式、安装与信任 |
| [ptc.md](ptc.md) | `RunCode`:模型写的程序能调什么、权限、限额与提示词 |
| [runext.md](runext.md) | `RunExt`:插件命令——声明、命令行、权限与 `writes`、限额、斜杠形式 |
| [agix_js.md](agix_js.md) | 内嵌脚本里的全局 `agix`:每个函数、限额与错误码 |
| [forge.md](forge.md) | 样例插件 `forge`:需求、规格与变更流程做成插件命令 |
| [mcp_auth.md](mcp_auth.md) | 远端 MCP server 的 OAuth:配置、登录流程、RPC 与通知、凭据存储、安全 |
| [schemas/settings.schema.json](schemas/settings.schema.json) | 生成的 `settings.json` schema |
| [benchmark/](benchmark/compare_dsh.md) | 与 DeepSeek Harness 的对比测试:准备、六个任务、量什么与怎么量,以及每次测试的记录 |
| [tests/README.md](../tests/README.md) | 测试分层与 LIVE 层 |

## 设计原则

1. **库优先。** daemon 是参考宿主,不是产品。
2. **引擎管机制,你管策略。** 每个决策都在一个 trait 后面。
3. **扩展弄不坏引擎。** 失败只损失它自己那一份。
4. **默认安全。** 你可以主动放宽,不会意外变松。
5. **没有经过检查的说法不写**——这些文档也一样。

## 许可

Apache-2.0
