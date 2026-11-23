# Agix 架构

Agix 是一个**库**,不是一个应用。它提供的是"跑一个 agent 会话"这件事的引擎:
装配提示词、调模型、执行工具、管理上下文与历史。**交互界面不在这里**——那是使用方
应用的事。`daemon` 是仓库里附带的一个参考宿主,
把引擎包成 JSON-RPC 服务,同时也是集成测试的载体。

这份文档讲**组件、概念、和它们之间的约束**。总览与快速开始见仓库根目录的 `README.md`
(中文版 `docs/README.zh-CN.md`)。想知道怎么用请看:

- `agix-sdk.md` —— 把引擎直接嵌进自己的进程
- `daemon_rpc_protocol.md` —— 通过 RPC 使用它
- `extension_points.md` —— 能扩展什么(总揽)
- `extending_quickjs.md` —— 怎么用脚本扩展;`extension_points.md` —— 全部扩展点
- `plugin_development.md` —— 怎么写 Rust 插件与 QuickJS 插件,样例在 `examples/plugins/`

---

## 1. 分层

```
 外壳    daemon · agix ──── agixd:参数、进程(信号、日志、崩溃收尾),建在 embed 上;agix:嵌入门面,只再导出
           │
 装配    assembly ───────── 会话池、(项目, 场景) 绑定与失效、provider 路由、插件包、模块清单;
           │                JSON-RPC 服务(Unix socket / TCP / WS)与实例发现
           │
 模块    agents(+team) · mcp · skills · script-host · packages
           │                agents、mcp、skills 经 runtime::module::Module 进入会话;script-host 走
           │                runtime::plugin::Plugin;packages 只被 assembly 用,team 只被 agents 用
 内核    runtime ────────── turn loop、提示词组装、工具调度、扩展点调度、Module 接口(K3)
           │
         hooks · tools · session · compaction ──────── K2
           │
         model · scene · history · telemetry · permissions · ptc ·
         settings · memory · prompt-files · session-state ──────── K1
           │
 本机    platform ───────── 执行层的本机实现(进程、文件、网络、OS 沙箱)、崩溃钩子、进程身份
           │
 契约    core(包名 base)── 内核面向外的 trait、共享数据类型与纯判定
         js-lib ─────────── 内嵌 QuickJS 的全局 agix,不依赖任何第一方 crate,只随 JS 引擎编入
```

**依赖方向**(`tests/guards/tests/deps.rs` 检查):

- 守卫读 `cargo metadata`,不读 `Cargo.toml` 文本:target 段、build 依赖、optional 依赖都在内;
  每个工作区成员在 `CRATES` 一张表里有一个级别,没登记的 crate 过不了。第一方的 normal / build
  边只能向下指,dev 边不限。
- 内核分三级,同级之间不互相依赖:**K1**(`model`、`history`、`telemetry`、`scene`、
  `permissions`、`ptc`、`settings`、`memory`、`prompt-files`、`session-state`)只需要契约与本机层,**K2**(`hooks`、`tools`、`session`、`compaction`)
  建在 K1 上,**K3**(`runtime`)建在两者之上。内核 crate **不依赖任何模块 crate**。
- 级别不对的依赖只能登记在 `NOT_YET` 里,写明什么能去掉它;名单带陈旧检查,边没了条目就得删。
- 模块依赖契约与内核;模块之间的依赖只经 `Module::requires`(硬依赖)与 `Module::after`(软顺序)声明,
  现有三个模块只用了 `after`。
- 只有装配层与外壳知道有哪些模块;模块 feature 的 `#[cfg]` 只出现在
  `assembly`(模块清单、会话池选哪份 `mcp_hub`、RPC 服务里个别方法)与 `daemon` 里。
  MCP 在 `assembly` 里只由两处决定:`modules.rs` 的模块清单,和 `session_pool/mod.rs`
  在 `parts/mcp_hub/live.rs` 与签名相同的桩 `off.rs` 之间二选一;其余代码(server 的
  `mcp.*` 在内)只写一份,`feature = "mcp"` 不超过 6 处(`tests/guards/tests/layering.rs`)。
- `platform` 只依赖契约,契约不依赖它。用它的是真要碰本机的 crate——`tools`、`runtime`、
  `model`、`mcp`、`packages`、`assembly`、`agix`、`daemon`,清单与
  各自的理由在 `MAY_USE_PLATFORM`;`scene`、`history`、`session`、`compaction`、
  `telemetry`、`hooks`、`ptc`、`permissions`、`settings`、`memory`、`prompt-files`、`session-state`、`team`、`skills`、`script-host`、`agents`、`js-lib` 只经契约,测试里用到的除外。
  子 Agent 要的执行层是父会话的那一套,随 `HostContracts` 经 `Builder::inherit` 交给子会话,自己不碰本机。
- 内核与模块不能带模块命名的 feature,也不转发模块 feature;模块开关只在 `assembly`
  里以 `dep:<模块>` 的形式出现。`wasmtime` 一类的运行时任何成员都不依赖。
- 唯一装配点由可见性保证:`SessionPool::builder` 是 `pub(crate)`,`assembly` 之外建不出第二个池,
  要池只能走 `assembly::assemble::pool`(门面里它叫 `agix::pool::assemble`)。
- 起一个说协议的引擎(池加上它的进程级职责)走 `assembly::embed`(`agix::embed`、`daemon::embed`),
  `agixd` 自己也从这里起;只要池的嵌入方仍直接调 `assemble::pool`。`agixd` 的命令行参数
  作字段进,协议连接出;读场景、建池、实例锁与发现文件、进程组登记、导入探测都在这里,`agixd` 的
  `main` 只剩命令行、运行时、日志、崩溃钩子、信号与 otel 收尾,TCP / WebSocket 与 token 在
  `assembly::agixd`。它是 0.x 里唯一单独承诺稳定的 Rust 面(`tests/guards/tests/embed_surface.rs`),
  数据根只从配置来,不读进程环境。
- 进程级状态(`static`、`thread_local!`)由 `tests/guards/tests/process_state.rs` 按语法树
  扫生产代码:类型不是朴素数据的 `static` 必须登记名字、种类与理由,`static mut` 不允许,
  登记了而代码里没有的条目失败。

RPC 服务和会话池在同一个 crate 里是体积上的取舍:两者共用大量泛型实例(serde_json、
tokio、rmcp),分在两个 crate 时各编一份,release 的 `agixd` 因此大约 2.8 MB。

**`core` 不依赖工作区里的任何别的 crate。** 它定义 trait、共享数据类型与纯判定(生产代码的行数有
棘轮,`tests/guards/ratchet/base.txt`,现值 21958 行:变少时用 `AGIX_UPDATE_RATCHET=1` 压下去;要上调就在同一个
提交里改这个文件,并在提交说明里写清为什么这些代码只能放在 `core`),并带着每个 crate 都要用的 fs、clock、path、text 小工具(`write_atomic`、`now_rfc3339`、
`resolve_physically`、`truncate_chars` 等);别的 crate 实现它的 trait。
这条约束是整个扩展面成立的前提:一个宿主要替换某个子系统时,面对的是 `core` 里的
一个 trait,而不是某个具体 crate 的内部结构。`tests/guards/tests/base.rs` 守着它:
生产行数不超过棘轮,直接碰文件系统的文件有白名单(七个,各写着理由),不调用模型。
两样带实现的东西留在这里,各有理由:`base::paths::PromptFiles` 的读(`canonicalize` 后过敏感路径
判定、再读**解析后的**路径)被 `memory`、`settings`、`prompt-files` 等互不依赖的 K1 共用,
拆成"判定在 core、读在各家"就是让每家自己记得走这道门,正是它要防的检查与使用之间的换路径;
`base::engine_exec::EngineExec` 不碰机器(进程经 `exec.process`、约束经 `exec.sandbox`),是
"引擎自己起的 git 怎么被约束"的判定,`ToolContext.engine_git` 挂着它,tools(K2)看不到 runtime,
搬走只能抽 trait,等于让这道唯一关口变成谁都能不走的接口。

**`base` 只有契约、共享数据、策略与判定,以及上述小工具,不起进程。** 子进程环境的策略与判定
(`SubprocessEnv::resolve` 给出 `EnvPlan`)在 `base`,把它写进一个 `tokio::process::Command`
的 `platform::apply_subprocess_env` 在 `platform`。引擎自己要起的进程(`git`、`pdftotext`)
不直接 spawn,经 `base::engine_exec::EngineExec` 交给会话的 `exec.process`(§5);排空输出、
收齐退出状态的 `base::interface::exec::capture` 对任何 `Process` 都能用。生产代码里真正
`spawn` 的是 `platform` 的 `LocalProcess` 与沙箱探测,和 MCP stdio 服务(经 process-wrap)。
`tests/guards/tests/sources.rs` 的 SRC-4 守着它:生产代码里只有 `MAY_SPAWN` 登记的 crate(`platform`、`mcp`)
可以起进程;本机的 `Process` 只在装配会话缺省 provider 的地方构建(SRC-5)。

执行层的本机实现也不在 `core`:
"本机"是 `platform` 对这些 trait 的一种回答,`in_process()` 是另一种。本机 HTTP 客户端
(`LocalNetwork`)在 `platform` 默认开启的 `http` feature 下,OS 沙箱后端在它的 `sandbox`
feature 下(`assembly` / `daemon` 的同名 feature 转发过去)。

| crate | 层 | 职责 |
|---|---|---|
| `core`(包名 `base`) | 契约 | trait、共享数据、纯判定,以及 fs/clock/path/text 小工具;消息与提示词类型、扩展点清单、`SkillCatalog` / `McpServers` 等内核面向模块的接口 |
| `platform` | 本机 | 执行层四个契约的本机实现(`LocalProcess`、`LocalFileSystem`、`LocalNetwork`、`PlatformSandbox`)与 `local_providers*`;进程组台账与孤儿收尾、崩溃钩子、进程身份 |
| `runtime` | 内核 | 轮次循环、流式解析、工具调度、`Module` / `SessionContext` |
| `model` | 内核 | 模型协议适配(Anthropic Messages / OpenAI Chat Completions),重试与退避;provider 路由(`routing`:沿 `model.order` 回退、冷却、配额记账、每次调用计一次量),runtime 经 `base::provider::TaskRouting` 用它 |
| `tools` | 内核 | 内建工具;`web`、`structured-output` 两组是 feature |
| `scene` | 内核 | 内建场景、`SceneRegistry`、声明式场景(§3.7) |
| `history` / `session` / `compaction` / `telemetry` / `hooks` / `permissions` | 内核 | 日志、会话状态、上下文压缩、遥测与录制回放、hooks、权限 |
| `settings` | 内核 | `Settings` 与设置文件模型:分层加载与合并、项目信任、托管设置、schema 生成、外部配置导入 |
| `memory` | 内核 | 文件式长期记忆:存储、密钥扫描、提示词片段、LLM 选取 |
| `prompt-files` | 内核 | 引擎读的 markdown 指令文件:`SKILL.md` frontmatter、全局层与自工作目录向上到 git 根收集的 `AGENTS.md`、输出风格、指令文件与 `.agix/rules/` 规则目录 |
| `session-state` | 内核 | 一个会话的可变状态(权限模式、todo、任务表、通知、读缓存、目标、记账),`base::context::Session` 的唯一实现;工具与权限策略经 `Arc<dyn Session>` 看到它,runtime 与会话池持具体类型 |
| `team` | 模块 | 团队协调器(`Coordinator`)、邮箱与 `TeamTool`;`agents` 的团队工具驱动它 |
| `agents` | 模块 | 子 agent(`Agent` 工具)、agent 类型目录、团队工具;依赖 `team` |
| `mcp` | 模块 | MCP 连接、工具适配器、资源工具、prompt 命令、OAuth 授权 |
| `skills` | 模块 | skill 目录与 `Skill` 工具 |
| `script-host` | 模块 | 脚本载体:配额与账本(`ScriptCarrier`)、九个点的适配器、QuickJS 引擎 |
| `js-lib` | 契约 | 每个内嵌 QuickJS 运行时都装的全局 `agix`(`docs/agix_js.md`):YAML、glob、路径、JSON Schema、SHA-256、UUID、参数解析;纯计算,不碰 I/O;只被 `script-host`(`quickjs` feature)与 `ptc` 依赖 |
| `ptc` | 内核 | `RunCode`:模型写的一次性程序,与 `RunExt`:插件包声明的命令(`docs/runext.md`),在同一套内嵌 QuickJS 运行时里跑,经 `ToolContext::dispatch` 调本会话的工具(`docs/ptc.md`);`ptc` feature,与脚本载体用同一套 QuickJS,两者各自独立开关 |
| `packages` | 模块 | 插件包:manifest、获取、缓存、披露;包里的 MCP、脚本、场景、agent 类型 |
| `assembly` | 装配 | 会话池、配置加载、按设置建 provider 集、自检、模块清单,以及其上的 JSON-RPC 服务、实例发现与嵌入入口 `embed` |
| `agix` | 外壳 | 嵌入门面:按主题再导出嵌入方用到的类型与函数,自身不含逻辑(`tests/guards/tests/facade.rs`);用法见 `agix-sdk.md` |
| `daemon` | 外壳 | `agixd` 二进制:参数、信号、日志,建在 `embed` 上;库部分按旧路径重新导出 assembly |

### 1.1 模块怎么进入会话

`runtime::agent::Builder::module(..)` 按顺序装入模块。`build()` 先按场景收窄
(§3.6)、再检查 `requires` 与 `after`(缺失、排在后面、违反软顺序、重复都拒绝),在会话的工具表、事件总线、
权限、hooks 都建好之后、场景的工具策略生效之前,依次调用 `on_session`——模块注册的
工具和内建工具受同一套场景约束。

`SessionContext` 把正在构建的会话交给模块。两样东西是**晚绑定**的,因为读它们的
东西比提供它们的模块先建好:

- **delegator**:`"agent"` 型 hook、`context: fork` 的 skill 与宿主的 `session.startAgent` 拿到的
  是 `runtime` 里的 `LateDelegator`——一个只能被填一次的壳,agents 模块在 `on_session` 里
  `provide_delegator` 填入真正的实现;没有人提供时每个请求都被拒绝(`DelegateError::Unsupported`)。
  壳只转发,不做任何委派的决定(`DLG-2`)。
- **skill 目录**:命令表、agent、其它模块拿到的都是同一个晚绑定的 `SkillCatalog`,
  由 skills 模块提供;没有时为空。

MCP 连接由 mcp 模块交给会话独占(`provide_mcp`),和原先会话独占 `McpManager` 一样。
连接按项目缓存在会话池里;一对 `(项目, 场景)` 第一次连时会话创建最多等
`MCP_SESSION_CONNECT_WAIT`(10 秒),没连完的 server 在后台接着连,结果并进缓存后作废这个
项目的绑定,会话在下一轮之前重建到新连接上。每个 server 的起进程加握手、每个请求都有时限
(`mcp::config::McpLimits`);工具调用断在途中只报错不重发(`mcp/src/connect.rs` 模块头)。
本机启动的 stdio server 知道自己在服务哪个项目:客户端声明 `roots`、以连接所属项目的根(解析过符号链接)
作答,并在它的环境里设 `AGIX_PROJECT_ROOT`(没有项目时为空列表、不设变量);远端 server 不声明 `roots`,
不把本机路径交出去(`mcp/src/project.rs`)。
工具与 prompt 的名字由 `mcp::naming` 从 server 名与工具名造出,保证符合模型 API 的
`^[a-zA-Z0-9_-]{1,64}$`,权限规则按这个名字写。

装配顺序由各模块的 `Module::after` 声明、`build()` 校验,顺序反了直接报错:agents 排在
mcp 之后,因为它要看到可以授予子 agent 的 MCP 服务器;skills 排在 agents 之后,因为
fork 型 skill 要 agents 提供的 delegator。委派出去的子会话用父会话(收窄后)的模块
清单装配,所以嵌套委派照旧;MCP 连接只给第一个装配它的会话。agent 类型写了
`mcp_servers` 的,子会话拿到父会话连接按 server 收窄出的一份 `McpServers`
(`McpServers::granted`,连接共用、不重连),经 `Builder::mcp_servers` 归子会话独占:
授予的工具和父会话的一样宣告给模型、随 server 的变更通知刷新,没授予的 server 看不到(§3.4)。

宿主能换掉的、会话读的几样东西,都有 `Builder` 上的注入口,缺省是本机的那一套,`build()` 里
各接一处,子 Agent 的会话经 `Builder::inherit(HostContracts)` 整份沿用父会话的(§3.4):

| 注入口 | 换掉的契约 | 说明 |
|---|---|---|
| `Builder::exec(ExecProviders)` | `exec.process` / `filesystem` / `network` / `sandbox` | 工具调用、引擎的 git 与 `pdftotext`、warmup 的预连接、command hook 的进程都落在它上面(HTTP hook 的出口:宿主给了 `exec` 就用它的 `network`,否则是不带模型出口策略的本机网络,因为 hook 是运维写的配置,常打 `localhost`);与 `Builder::process_groups` 互斥,同设时 `build()` 报错——要记进程组就把台账带在传入的 provider 里(`platform::local_providers_with_ledger`) |
| `Builder::token_counter(..)` | `token.count` | 压缩触发的那个数、请求开销与 `inspectContext` 的逐块计数、`compact_now` 的目标大小,同一口径 |
| `Builder::prompt_assembler(..)` | `prompt.assembler` | 缺省 `runtime::prompt_assembly::MergedSystemPrompt`(包着引擎自己的 `DefaultAssembler`);契约 `PromptAssembler` 与请求、块类型在 `base` |
| `Builder::rule_provider(..)` | `rules.source` | 缺省 `prompt_files::rule_tiers`(全局、场景、项目三层,后来居上);每次组装提示词时取一次 |

### 1.2 档位

`daemon` 的 feature 按档位组织,脚本载体(`scripts`)、包方案(`plugin-packages`)与
`Bash` 的进程沙箱(`sandbox`)另选:

| 档 | 装配的模块 |
|---|---|
| `minimal`(`--no-default-features`) | 只有内核:模型、turn loop、场景、会话、遥测、hooks、文件与 shell 工具 |
| `coding` | + mcp(`oauth.private_key_jwt` 在另开的 `mcp-oauth-jwt` 后面,会带进 aws-lc,缺省不编入,见 `agix-sdk.md` §1)、agents、skills、web 工具、structured-output、ptc(`RunCode`) |
| `full` | + WebSocket 监听 |

默认 `full + scripts + plugin-packages + sandbox`。`daemon.ping` / `daemon.status` 的 `modules`
列出编入的模块;属于未编入模块的方法返回 `METHOD_NOT_FOUND`,`data.module` 指明模块;
配置了未编入模块的设置段(如 minimal 下的 `mcp`)启动时警告并忽略;没编入
`scripts` 时出现 `scripts` 段记 error,会话同样照常起来、不带脚本。两者都不阻止启动,
级别不同是因为缺了 MCP 服务器或团队工具,模型和宿主看得见(工具不在);脚本没生效却
只是输出悄悄变了样,作者会先去 JavaScript 里找 bug。

### 1.3 失败策略

`release` profile(fat LTO、`opt-level = "s"`、`panic = "abort"`)在 panic 时中止进程,
由 supervisor 拉起;只有明确要 `release-fast`(`opt-level = 3`、`panic = "unwind"`)时
才展开,任务里的 panic 只结束那个任务;工具调用里的 panic 变成这次调用的错误结果,这一轮照常往下走;
处理一个 JSON-RPC 请求时的 panic(比如宿主的 `customize_session` 里)变成这个请求的
`INTERNAL_ERROR`,不会让客户端一直等着,连接和其它请求照常服务。
也就是说,**`release` 构建里这层兜底不生效**:一个工具 panic 会带走整个进程和它上面所有会话,
不是只带走这一次调用。这是有意的取舍(`release` 要小、要快,崩了由 supervisor 拉起、会话从日志恢复),
不想要的部署用 `release-fast`,或者保证工具代码不 panic。`daemon.doctor` 的 `build.panic` 一项
报告这个构建是哪一种。中止前 panic hook 停掉
命令所在的进程组并写崩溃记录(`~/.agix/daemon/crashes/`);来不及跑 hook 的(SIGKILL、
OOM)由同名实例下次启动、拿到锁之后按落盘的进程组记录(`~/.agix/daemon/groups/`)
清理(不给实例名的嵌入方给 `EmbedConfig.groups_file`,下次用同一文件启动时清理):先 `SIGTERM`,500 ms 后 `SIGKILL`。会话历史每个条目整行写进日志,每一步结束和
一轮结束时 fsync 一次,重启后照常恢复;崩溃时正在进行的那一轮不续跑。写到一半被打断的
最后一行在读时略过(记一条 warn),下一次追加前截掉——不会让整份会话读不回来。

一份会话日志同一时刻只有一个写者:写日志的进程对 `sessions/<id>/writer.lock` 持
`flock`,会话在某个 daemon 里活着就一直持有,进程无论怎么退出内核都会释放。另一个
共用同一状态目录的 daemon 要 resume 或给它跑 turn 时直接报错,不换 id 新建。

daemon 不自己重启自己,交给 launchd(`KeepAlive` 设 `SuccessfulExit = false`)或
systemd(`Restart=on-failure`)。systemd 下要设 `KillMode=process`:`daemon.shutdown` 与
`SIGTERM` 走正常关闭,由 daemon 按会话设置决定哪些后台命令停、哪些留,整组杀掉会抹掉
这个区别。

---

## 2. 一轮是什么

`crates/runtime/src/agent/turn/` 是引擎的主循环。**步骤顺序是骨架,不开放**;
每一步上"选哪条路"的判断是决策,**开放**。

```
收用户消息(open_turn)
  └─ UserPromptSubmit hook · 斜杠命令 · 记忆召回 · 先放进空闲期间到的通知(§3.8)
循环(step):
  ├─ 0. 落盘        checkpoint:上一步写进内存的消息先落盘
  ├─ 1. 停止判断    TurnPolicy::before_model_call · BudgetPolicy::remaining  ← 这一轮还该不该继续、这次最多要多少输出
  ├─ 2. 修剪与压缩  fit_context · Compactor::trim_* / should_compact  ← 清哪些旧结果、该不该压
  ├─ 3. 硬顶        over_hard_cap · ContextBudget::hard_cap
  ├─ 4. 装配提示词  PromptAssembler / 注册的块 / 装配钩子
  ├─ 5. 调模型      call_model · Model + ModelInterceptor + BackoffPolicy
  │     ├─ 边流边跑工具 PermissionPolicy → ToolMiddleware → Tool → ToolResultTransformer
  │     └─ 出错 → RecoveryPolicy:换模型 / 压缩重试 / 原样重发 / 失败
  ├─ 6. 复查        TurnPolicy::after_model_call    ← 这一轮还走不走下一步
  ├─ 7. 记账        account_call · charge_budget · BudgetPolicy::on_usage  ← 继续 / 警告 / 耗尽
  ├─ 8. 有工具调用(between_tool_steps):送达这一轮运行期间收到的用户消息与通知,再来一轮
  └─ 9. 没有(reply_stopped):RecoveryPolicy::on_early_stop       ← 抬 max_tokens 重发 / pause_turn 发回去续上
         Stop hook(结束这一轮 / 拦下:把理由交给模型再来一轮);
         BudgetPolicy::on_output_target 决定是否续写
         (收尾时还有没送达的用户消息,再调一次模型回答它们)
出口:finish_turn —— 先落盘;团队成员再发 TaskCompleted / TeammateIdle;最后发 TurnComplete
```

**回复停在 `max_tokens` 时,三个决策按步骤顺序轮流说话,先说的算。** 第 6 步
`TurnPolicy::after_model_call` 说停就停;回复要了工具就照常下一步,另外两个都不问;
否则第 9 步先问 `RecoveryPolicy::on_early_stop`,它决定抬高上限重发就重发,
Stop hook 和 `BudgetPolicy::on_output_target` 这一步都不再问;它放弃了才轮到 Stop hook,
最后才是 `on_output_target`。

**工具在第 5 步里跑,不等回复写完。** 并发安全的 `tool_use` 一到齐就开始,模型接着往下写;
不并发安全的是屏障,等前面那批跑完再单独跑;它跑的时候照样读流,读到的等它跑完再处理。所以第 6、7 步拦不住这一条回复要的工具——
它们决定的是还走不走下一步。要在工具跑之前拦,用权限、`PreToolUse` hook 或
`ToolMiddleware`。

**用量只在一处计,预算是一条链。** 一次调用的用量由路由它的那一层计一次:`model::routing` 的每次调用
(没有路由的会话由 `Builder` 在根上套的 `Metered`)经 `UsageTap::metered` 在回复流的 `EndTurn` 处、或调用中途断开 /
失败 / 被丢弃时按 client 已经报的,同时记进 provider 配额和这次调用的 `UsageTap` 所记的那个
`SpendNode`(`base::interface::budget_policy`)。调用方只说记到哪:这一轮的主循环在发出前把 tap 指向
这一轮的节点,旁路调用(压缩、记忆、`WebSearch` / `WebFetch`)经 `SpentModel` 指向当时打开的那一轮(两轮之间的,如
`session.compact`,指向会话的节点),
发出前同样按链上剩余减去整个请求输入的估计压低 `max_tokens`、不够 `MIN_CALL_TOKENS` 就不发,
`prompt` hook 的调用指向 payload 带的那一轮(`HookInput.spend`:工具管线里的 hook 是那次工具调用所在的
一轮,其余是触发时打开的那一轮,两轮之间触发的指向会话的节点)。

节点连成链:delegate 那一轮的节点在起它的那一轮下面(`Agent` 工具、`TeamCreate` 的阶段成员、派给持久
成员的消息、fork 的 skill、`agent` hook 把起它的那一轮放进 `DelegateRequest.spend`;宿主 `session.startAgent` 起的放
会话的节点),根轮在会话的节点下面,目标轮在两者之间插一层目标的节点。一次调用逐级加到链上每个节点。
引擎自己的上限挂在节点上——一轮的(`turn.token_budget`,挂在根轮,delegate 共用)、目标的、会话的——
第 1 步取整条链上最小的剩余(`SpendNode::remaining`),减去这次请求输入的估计(见下)后压低 `max_tokens`、
不到 `MIN_CALL_TOKENS` 不发(付不起的是目标的上限时目标转 `blocked`),
第 7 步(`SpendNode::judge`)任何一层到顶即以 `budget_exceeded` 结束,最近的一层到 90% 时提醒收尾;
`turn_complete.message` 写明是谁的预算。宿主的 `BudgetPolicy` 在这之外再判一次(`remaining` / `on_usage`
拿根轮的读数 `whole_turn`),两边都要过,注入 policy 不会让引擎的上限失效。所有上限和 provider 配额用同一个
口径 `Spend::counted_tokens`:未命中缓存的输入 + 缓存写入 + 输出,缓存命中不计。delegate 的 `BudgetPolicy`
经 `HostContracts` 继承根会话的。`turn_complete.usage` 是这一轮节点的读数,`usage_breakdown` 按自己 /
旁路 / delegate 拆开。`max_model_calls` 不在这里:每个 agent 一份,在它自己的 `TurnPolicy` 里。

预算的几层,由近及远:

| 层 | 上限 | 量的是什么 | 到顶时 |
|---|---|---|---|
| 一次调用 | `max_tokens`(`model.max_tokens`、模型的 `max_output`),压到链上最小的剩余减去这次输入的估计 | 这次调用的输出 | 回复以 `max_tokens` 截断 |
| 一轮 | `turn.token_budget`(`options.settings.turn` 只能收紧) | 根轮与它的 delegate、旁路调用、`prompt` / `agent` hook | 这一轮 `budget_exceeded`,下一轮重新算 |
| 目标 | 目标的 `token_budget` | 目标各轮(含 delegate、旁路) | 这一轮 `budget_exceeded`,目标转 `blocked`(`blocked_reason` 以 `token budget` 开头),同轮数用完 |
| 会话 | `session.token_budget` | 会话从创建起所有的轮,跨重启 | 这一轮与之后每一轮 `budget_exceeded`,不调模型 |
| provider 配额 | `providers[].quota` | 每个 provider 按日 / 周 / 月,跨会话 | 路由跳过它,换下一个;都用尽报 `providers_unavailable` |

后四层同一个口径(`Spend::counted_tokens`),由同一次计量记账;前四层判在调用前后,配额判在路由选 provider 时。

**调用之前估输入,预算是估计,不是预留。** 一次调用的输入要发出去才知道 provider 怎么计,而缓存命中不计:
按整个请求估会让走缓存的会话还没花什么就被拦下。所以发出前估的是预计不由缓存供给的那部分
(`runtime/src/agent/turn/call_input.rs`,用会话的 `TokenCounter`):这个 agent 上一次调用的请求是这次的
前缀时,是那之后新加的消息,上一次没有读到缓存(provider 报的 `cache_read` 为 0)时再加上它被计的整个输入
(未命中缓存的输入 + 缓存写入,下一次原样再发一遍);不是前缀时(压缩、修剪、宿主改了上下文、系统提示词或
工具变了)是整个请求;这个 agent 还没调过(新会话、恢复后、新起的 delegate)时是模型上一次说话之后的
消息,系统提示词、工具与模型看过的部分当作已缓存。`prompt` hook 的请求整个算。估计从剩余里先扣掉,
剩下的才是 `max_tokens` 的上限。同时在跑的 agent(并发的子 Agent、团队成员、后台子 Agent)判的是同一份
剩余,彼此不预留,所以一层上限可能被超出,超出的量至多是同时在跑的每个 agent 各一次调用的输出,加上
估计的误差(本地计数与 provider 计数的差、第一次调用当作已缓存而实际没有缓存的部分);超出之后下一次调用
之前一定停。

**一条工具管线。** 模型的每次工具调用(`tool_use`)、`RunCode` 程序里的子调用、skill 正文里的
`` !`cmd` `` 内嵌命令,都走同一条:`PreToolUse` → 输入校验 → 权限 → 中间件 → 工具 → `tool_result`
转换器 → `PostToolUse`,遥测、panic 兜底与外部内容围栏也各只接一处。后两类经 `SubCallDispatcher`
(`runtime/src/agent/turn/tool_env.rs`)进来,算这个会话的一次 `Bash` 或工具调用,各有派生的
tool-use id,不能向人询问,要批准的调用直接被拒;宿主看不到它们的 `tool_use` 帧。`tests/guards/tests/tool_pipeline.rs`
守着没有第二条路径。

宿主在轮次之外调插件函数(`plugin.call`,`docs/runext.md` §12)也走这一条:池把为这次调用建的
`PluginCall` 工具对象交给会话的 `HostToolRunner`(`base::interface::plugin_call`,实现在
`runtime/src/agent/turn/host_runner.rs`),它不在会话的工具表里、不进对话,tool-use id 是 `host-<id>`。
谁发起的调用记在 `Caller` 上(`tool.around` 的 `via`):模型(`model`)、程序(`RunCode`)、插件命令
(`RunExt:<命令>`)、skill 的内联命令(`skill`)、用户敲的 `/name`(`Person`,`user`)、宿主(`Host`,
`host`)。`Person` 与 `Host` 的调用本身就是批准,会话要问的视同允许(`Host` 不答复只能本人答的问题,`Caller::answers`),deny 规则与 plan 模式照拦;
插件命令与宿主调的函数的子调用都是 `Caller::Command`(后者 `via` 为 `PluginCall:<函数>`),按调用时的
只读或 `writes` 范围收窄,要问人的直接拒。带会话的调用用会话的引擎,可以与正在跑的一轮并发;不带
会话的,池在给定目录下建一个不写日志、不进会话表、不连 MCP 的临时引擎,只为这一次调用。

**取消有宽限。** 一轮被取消时,还在跑的工具先得到 `tool_cancel_grace`(缺省 2 秒)自己停下,
过了就丢弃、把这次调用答成已取消,不拖着整轮不收尾。MCP server 把调用转成任务时,调用在原地挂着
等任务(§6.4),取消同样先走这段宽限;调用被丢弃时才向 server 发 `tasks/cancel`。

**出错时什么都没留下,才交给 RecoveryPolicy。** 请求被拒(状态码、连不上),或者流在回复
要工具之前断了(流里的 `error` 事件、没等到消息结尾就结束),这次调用的半截回复整个丢掉,
transcript 不变,恢复策略重发的就是失败的那个请求;每次调用至多恢复 4 次,原样重发只用于
流已经开始之后的失败(首字节之前的已经由 client 的退避阶梯重试过)。工具已经跑过的,
回复连同结果照写,这一轮以错误结束——再发一遍会让工具再跑一遍。

**压缩只有一条路径**(改写历史的那种)。阈值逼出来的(第 2 步)、`/compact` 要的、请求因体积被拒后
恢复策略要的(第 5 步),都走 `run_compaction`:`PreCompact` hook、需要时先让模型写摘要、
否则走 compactor 的级联、把丢掉的读文件结果与 skill / 计划模式 / 后台任务的提醒补回前面、
写 transcript 标记、发 `compact` 帧、`PostCompact`,并让下一轮重新注入被压掉的开场上下文
(AGENTS.md 等)和召回过的记忆。阈值量的是整个请求,所以自动压缩把对话压到阈值减去系统
提示词与工具定义、再减去必然补回的提醒之后剩下的大小;压完比压之前还大的不生效。

**修剪不是压缩。** 压缩之前,同一步里先做三种修剪:按时间或缓存命中清旧工具结果的正文、
把工具结果截到预算之内、开了 `compaction.reactive` 时按增长速度提前清。修剪只把旧
`tool_result` 的正文换成占位,不丢消息、不写压缩标记、不触发 `PreCompact`/`PostCompact`,
改了对话的发一帧带 `trim: true` 的 `compact`。修剪与随后的压缩是同一个事务:压缩被取消或
失败,修剪一起退回。允许改写 `session.messages` 的地方只有这两条路径、`/clear` 和宿主的
上下文编辑(`session.context` 的 drop/edit),守卫 G11 列着。

**长工具结果外置。** 工具结果在脱敏 transformer 之后超过 `compaction.inline_result_chars`
(缺省 30,000 字符,`0` 关闭)时,整个写进根会话的结果目录,对话里只留一行标记(总字符数、
行数、路径、"用 `Read` 的 `offset` 读后续")加前 2,000 与后 1,000 字符的预览(围栏保留)。
标记行在最前,预算清旧结果时据此保留路径(`[Old tool result content cleared; full output still
at <path>]`);只有路径直接落在本会话(根会话)结果目录、文件名合 `[A-Za-z0-9_-]+.txt` 的标记
算数(`compaction::saved`),工具打印出来的伪标记按普通文本清。外置文件跟着会话走:`session.resume`
时清掉转录(含侧链)里不再提到的;`session.fork` 把被引用的文件硬链接进新会话目录并改写标记行;
`session.export`(`include` 含 `tool_results`,缺省)带走、`session.import` 落到新根会话目录并改写标记;
`compaction.inline_result_chars` 不受未信任项目影响。自己分页的 `Read`、`WebFetch`、`TaskOutput`(`ToolSpec::pages_itself`)、`RunCode`
里的子调用和 skill 内联命令不外置;失败的结果一样外置;没有可写目录或写失败时不外置,交给
单条预算截断。转录、压缩、录像看到的都只是预览。外置文件的内容可能是 MCP server 或网页写的,
所以 `Read` / `Grep` 指向这个目录时一律按外部内容围栏,不让它借文件洗成项目文件。预览不再提示用
`Bash` 读后续;`Bash` 命令文本点名了本会话外置目录时,整段结果按外部内容加围栏。**已知限制:**
围栏只认命令文本里的目录名,命令把路径拼出来、或 `cp` 进项目后再读,不在其内。外置时写进文件首尾的围栏只包住整份,不跟着片段走。

**一轮只有一个出口。** 循环里有十几处提前结束(hook 截停、预算耗尽、取消、出错),
落盘和宣布结束都只在 `finish_turn` 做一次,所以哪条路都跳不过。顺序本身是契约:宿主
把 `TurnComplete` 读作"会话空闲",`persisted` 为真时还是"它的日志已经有了全部"——daemon 收到它就
把会话交给下一个调用方,并可能立刻从日志重建它。写日志失败的一轮照样以 `TurnComplete` 结束
(它确实跑了,报失败会诱导重发),只是 `persisted` 为假;以失败结束的一轮发的 `Error` 同样带
`persisted`。两种结局只要 `persisted` 为假,池就把会话记成日志落后
(`SessionCore` 的 `LogHealth`),落后期间不驱逐它,重建前先发 `InputMessage::Flush` 让引擎补写,
补写失败就不重建、报 `persist_failed`;没写进去的目标变更和持久状态退回 `SessionState`,下一次写
时再写。每一步开始前也落一次盘,进程在
一轮中途死掉最多丢正在进行的那一步;落盘点都在 `tool_use` 与它的 `tool_result` 已经
配对的地方。

**运行中的用户消息在步骤边界送达。** 一轮在跑时收到的用户消息先排队,在一批工具结果
写进对话之后、下一次调模型之前加进去,所以永远不会落在一对 `tool_use` /
`tool_result` 中间。这一轮在送达之前就结束了(取消、截停、出错)的,消息不进
transcript,以 `UserMessagesReturned` 交还宿主。通知(`SessionState::post_notice`,
如后台子 Agent 结束)走同一组边界;一轮已经结束的,留给下一轮开头。


**骨架与决策的分界是这套设计的核心。** 步骤顺序写死,是因为顺序本身就是"一轮"的定义;
而每一步的判断——跑几步停、错了怎么办、什么时候压缩、花多少 token——是意见,
不同部署持不同的意见。所以它们在契约后面。

---

## 3. 会话与场景

### 3.1 一个进程可以跑多个场景

场景是一份数据(`SceneSpec`):身份、工具面、模块面、预算、提醒、会话命名,都是字段;只有
提示词是行为,由 `ScenePrompt`(`system` 与 `session_context`)给出。`code` 的提示词只带一个
创建时定下的 `Interaction`,`Arc<SceneSpec>` 建好后不再变。所以同一进程并存多个场景没有障碍。
宿主在内建场景上改自己的场景时改字段即可(`let mut s = scene::harnix::spec();`),不必包一层
trait 再逐个转发方法。

内建三个:

| id | 工具 | 用途 |
|---|---|---|
| `code` | 注册的全部工具;`RESIDENT_TOOLS` 之外只报名字,schema 经 `ToolSearch` 取 | 编程,需要项目;daemon 的默认场景。注册的是交互版(`Interaction::Interactive`),嵌入方可换成 `scene::code::spec(Interaction::Unattended)` |
| `harnix` | 常用集:文件读写与搜索、`Bash`、`Skill`、`TodoWrite`、`Agent` 与任务控制、`Team*`、目标、网页检索与抓取、`ToolSearch`、`mcp__*`;宿主经 `extra_tools` 加自己的 | Harnix 宿主会话的默认场景,无人值守 |
| `mini` | `Agent` `Bash` `Edit` `Glob` `Grep` `Read` `Skill` `ToolSearch` `Write`,全部常驻 | 最小可用的编码 agent,无人值守 |

- `--scene` 定 daemon 的默认场景,`--scenes harnix,mini` 追加激活
- `scene.activate` / `scene.deactivate` 运行期增删
- `session.create {"scene": "harnix"}` 指定该会话用哪个;未激活的场景返回 `SCENE_NOT_FOUND`
- **每个会话按自己的场景解析设置层**,包括 `~/.agix/scenes/<scene>/` 下的 settings、
  skills、agents、plugins。设置缓存的键是 `(project, scene)`——只按项目缓存会让先建好
  条目的那个场景的设置泄漏给所有场景。

唯一保留的进程级绑定是 `DaemonPaths::config_root()`,启动时按首个场景算定;
其它场景的目录由 `BindingCache::scene_root` 从 `global_root()/scenes/<id>` 推导。

**`SCENE_MISMATCH`** 守的是"这个会话属于哪个场景",不是"这个 daemon 只能跑一个场景"。
会话的 `Meta.scene` 记录它创建时的场景;操作属于别的场景的会话时拒绝,不跨边界执行。
旧 transcript 没有 `Meta.scene`(v2 之前没这个字段),此时判定为`SceneCheck::Inferred`——按 pool 的默认场景接上,而不是让整份文件解析失败。

### 3.2 `project_root` 的三态

RPC 里 `project_root` 有三种含义,靠 JSON 层面的"键缺失 / 键为 null / 键为字符串"区分:

| 线格式 | 语义 |
|---|---|
| 键省略 | 该 pool 的默认项目 |
| `"project_root": null` | 真正的无项目会话(全局层) |
| `"project_root": "<path>"` | 指定项目 |

serde 的 `Option<Option<T>>` 免费区分前两者,但 `null` 与字符串的区分必须在 wire 层
显式检查,**在到达 `SessionPool` 之前**。无项目会话的 `local_data_dir` 取
`global_data_dir` 本身,而不是 `<project>/.agix`。

**无项目就是没有项目,不是"状态目录的上一级"。** 有没有项目记在 `PathSettings::workspace` 上:
`Project(根)` 或 `Scratch(目录)`。`project_root()` 对后者是 `None`,凡是按项目找东西的地方
(项目层设置、skills、agents、rules、hooks、git、`AGENTS.md`)都跳过;工具在 `SessionConfig::working_dir()` 里
干活——有项目是项目根,无项目是会话自己的 `<状态目录>/scratchpad/sessions/<会话 id>`,建会话时
由会话池定下,子 Agent 继承父会话的设置,所以用同一个。不能退回"状态目录的上一级":状态目录
是 `~/.agix` 时那就是家目录,写 `~/.zshrc` 会被当成项目内的写。

| | 有项目 | 无项目 |
|---|---|---|
| 工具的工作目录(相对路径、`Bash` 的 cwd) | 项目根 | `<状态目录>/scratchpad/sessions/<会话 id>` |
| 不用询问就能写(§6.3) | 项目根、状态目录的 `scratchpad` 与 `memory`、`files.extra_dirs` | 工作目录、状态目录的 `scratchpad` 与 `memory`、`files.extra_dirs` |
| 不用询问就能读 | 上一行,加状态目录、`files.read_dirs` | 同左 |
| 沙箱可写 | 项目根,加上一行其余几处、会话的 `TMPDIR`、`sandbox.cache_dirs` | 工作目录,加同样几处;状态目录的其余部分仍不可写,其中的 socket 仍连不上 |
| git 上下文、引擎跑的 git | 项目根 | 没有 |
| `AGENTS.md` | 全局层，加从项目向上到 git 根 | 不找 |
| 项目层设置、skills、agents、rules、hooks、output style | `<项目>/.agix`、`<项目>/.agents/skills` | 没有这一层 |
| 引擎读进提示词的文件(`PromptFiles`)限定的根 | 项目根 | 没有(只读用户层的东西,仍过凭据名单) |
| `permit_always` 的 `local` | 写进 `<项目>/.agix/settings.local.json` | 同 `session`:只在本会话生效,不写盘 |

`config.*` / `project.*` / `mcp.*`(登录的 `complete` / `cancel` / `setIdentity` 除外)/ `import.*` 这几个**作用在一对 (项目, 场景)** 的方法用的是另一套
两态:省略 = 这个 daemon 自己的项目,给路径 = 它服务的另一个项目,**`null` 拒绝** ——
无项目会话没有项目层可写。

### 3.2.1 一条不变量

> **会话读到的任何按项目索引的东西,都来自它自己的 `project_root`。进程级的只剩
> 不按项目索引的东西。**

按项目索引、因而按 `(project, scene)` 绑定的(`BindingCache` 里的
`ProjectBinding`):合并设置、skills、agent types、commands、provider 路由与主模型、
MCP 连接、脚本绑定的 root、记忆目录、transcript 目录。

进程级的,因为它们**不按项目索引**:

| 进程级 | 为什么 |
|---|---|
| `DaemonPaths::config_root()` | 启动时按首个场景算定,见 §3.1 |
| 工具注册表 | 内置工具是编译期事实 |
| 场景注册表 | 内建、宿主登记(`Assembly.scenes` / `SessionPool::register_scene`)、插件贡献三类,宿主登记的可以是声明式场景;激活集合是运行期状态,但不按项目 |
| 插件集合 | 插件只有全局层与场景层,**没有项目层** |
| `WatchHub` | 一个 `notify` 线程,按 tier 分键去重,watched 目录是 `1 + M + P` |

**加一样按项目索引的东西,就把它放进 `ProjectBinding`**,不要在池上再挂一个
`HashMap<ProjectSceneKey, _>`。

有两张表是有意的例外,同样按 `(project, scene)` 取键,所以不会读到别的项目,只是寿命
比绑定长、放不进去:

| 绑定之外的表 | 为什么不在绑定里 |
|---|---|
| `McpHub` 的连接缓存 | 默认项目的连接在启动时就开始,那时还没有任何绑定;`config.reload` 在原处重连、只换掉这一项,不用等绑定重建 |
| `BindingCache` 的 `last_good_routers` | 新的 provider 配置解析不出或建不出 client 时退回它;绑定一失效就跟着没了,就退不回去。项目的信任变了(受信时建的路由用到了只有受信才生效的 provider)这一项就丢掉 |

**绑定按层失效。** 每个绑定带一个失效标记,用它建出的会话持有同一个标记。设置文件变化
只作废从那一层合出来的绑定:项目层作废同一项目根的,scene 层作废那个 scene 的,全局层
作废全部;显式的 `config.reload` 不知道动的是哪一层,作废全部。缓存的 MCP 连接跟着同
一层作废。skills、agents 目录的变化不作废任何东西,新内容就地读进绑定共享的目录对象,
会话立刻看到。池常驻监视它自己那一对 `(项目, 场景)` 的各层。设置文件只有内容真的变了才算
变化:macOS 的 FSEvents 会把监视开始前不久的写入补报过来,照原样再写一遍也会报,这些都不作废
绑定。

**命令行给的设置是每一对最上面的一层。** `agixd --set KEY=VALUE`(`--max-tokens`、
`--permission-prompt-timeout` 是它的简写)与嵌入方的 `EmbedConfig.settings` 合成 `cli` 层
(`Tier::Cli`),`settings_source::read_pair` 把它排在三层设置文件之上,读进每一对 `(项目, 场景)`,与文件走
同一条合并:按全局层的规矩合(`app` 照收,不受项目信任门禁),`permissions` 规则列表与各层并列生效、来源记为
`CliArg`,放不进的值进 `ignored_values`、层名 `cli`。它是每次读盘的一部分,所以绑定作废、`config.reload`
之后照样在;宿主在内存里改过的值(`HostOverrides`)与 managed 层仍叠在它上面,managed 写了的键以
managed 为准。启动时对守护进程自己那一对
查一次:没有设置读的键、旧键、放不进的值让 `Embedded::start` 返回 `EmbedError::InvalidSetting`,agixd
非零退出并报出是哪个 `--set`。`config.get {tier:"cli"}` 返回这一层,`daemon.doctor` 的 `settings_tiers`
里有一项 `tier: "cli"`,列出它设的键。

**内存里的覆盖只有一条路。** 宿主在内存里改过的值(`HostOverrides`)与会话自带的设置(`session.create` 的
`options.settings`)都经 `settings::LoadedSettings::overlay` 叠到读好的设置上:规则串并进各层的规则(会话的记为
`Tier::Session`、来源 `RuleSource::Session`)、provider 收尾、放不进的值进 `ignored`、managed 层重新压顶,只写在
这一处。合并设置 JSON 的原语(`merge_json_values`、`merge_fitting`)在生产代码里只出现在登记过的地方:加载器的
`LayerFold`、`LoadedSettings::overlay`、managed 层的 `with_managed`,以及写设置文件、校验与拼 `--set` 层这几处不合
读好设置的地方;`tests/guards/tests/sources.rs` 的 SRC-6(`SETTINGS_OVERLAY_SITES`)守着,新加一处不在表里就失败。宿主的改动对守护进程自己那一对照原样叠,只有列表跟着盘:宿主没加的元素盘上没了(撤销信任后项目自己的 provider、改掉的项目设置)就不再有,盘上后加的、宿主没去掉的照样有;对别的对只带它新加的——列表只带宿主新加的元素,接在那一对
自己的列表后面,守护进程自己项目层来的 provider(宿主改过的也是)、`files.extra_dirs` 之类不跨对。

**会话覆盖层:按作用域收键,按未受信项目层的方向判。** 每个设置键在键表(`settings::keys`,见 §6.6)里有一个
作用域:进程(`app`、`plugins`、
`telemetry.otlp_endpoint`)、`(项目, 场景)` 对(`providers`、`model.order`、`model.tasks`、`mcp`、`hooks`、`recorder`、
`scripts`、`permissions.client_may_loosen`——路由、MCP 连接、hook 来源都按绑定建),其余是会话。`options.settings`
只收会话作用域的键,别的进 `invalid_options`。收下的键缺省按同一张表的信任列给的比较方向判:只许更严的键给宽了
就留设置的值、报在 `clamped_options`,allow 规则扣下;设置开了 `permissions.client_may_loosen` 时照样放宽。信任列
里的 `Withheld` 不用——会话能给什么由作用域定。这是安全缺省,不是
边界:连得上 socket 的客户端能 `config.set`,本来就是设置的所有者。会话层之后权限模式再过一遍
`effective_permission_mode`,managed 层的 `disable_bypass` 与规则压得住它。每次建引擎都对这时的设置重新判。
会话自己的推理强度、思考开关与提示词(`model.effort`、`model.thinking`、`prompt.*`)只在这一层:`session.setModel`
的 `effort` / `thinking` 与 `session.setPrompt` 改的也是它(规格里的 `settings`,改过就盖过 `options.settings`,
建会话时的 `options` 留着给同 id 的 `session.create` 比对),一样判、一样作用于子 Agent 与团队成员,一样重建引擎;
没有第二条只管会话自己那段对话的路。

**会话的配置:共享的设置、会话层、会话自己定的值,三样分开放。** 引擎建在 `settings::SessionConfig` 上
(会话池在 `parts/factory` 里给每次建引擎配一个,子 Agent 从父会话的派生):
- `shared()`:为这一对 `(项目, 场景)` 读好的设置,`cli` 层、宿主覆盖与 managed 层都在里面;同一对的会话共用
  同一个 `Arc<LoadedSettings>`,不按会话复制、不按会话改。要"文件里怎么说"的读它。
- `session_layer()` 与 `settings()`:会话层原样留着,`settings()` 是两者经 `LoadedSettings::overlay` 算出来的,不再
  手改;读设置值的代码读它。被压住的键在 `held()`,`requested(key)` / `applied(key)` 给出会话层对一个键要了
  什么、实际用的是什么(`permissions.mode` 的 applied 是会话的模式)。
- 会话自己定的:`permission_mode()`(`effective_permission_mode` 夹紧后的模式)、`model_name()`(规格或路由选出的
  对话模型,`session.setModel` 换过的引擎在下一轮从会话状态取)、`workspace()`(无项目会话自己的 scratch 目录)、
  `worktree()`;`working_dir()`、`worktree_git()`、`engine_config()` 由它们得出。这些不写进 `Settings`:
  `settings().values.permissions.mode` 是设置(连同会话层)说的值,会话按哪个模式跑只看 `permission_mode()`;
  `LoadedSettings::main_model` 是设置给的对话模型。`PathSettings` 也只有这一对的目录,没有 worktree。

子 Agent 的配置照拿父会话的共享设置与会话层,换上自己的模型、模式与 worktree;agent 类型的 `effort` /
`max_turns` 经 `SessionConfig::tuned_for_delegate` 落在派生出的 `settings()` 上。`Settings` 与它的块只放设置文件
写得出的字段,`tests/guards` 的 A7 守着(这些类型里不许有 `#[serde(skip)]` 字段)。

**会话自带的 skill 目录叠在绑定的目录之上。** 绑定里的 skill 目录(`SkillManager`)与命令表对同一对
`(项目, 场景)` 的会话共享;宿主按会话给的目录(`options.skill_dirs` / `session.setSkillDirs`)不进
绑定,而是每个会话一个 `skills::host::HostSkillLayer`(一组 `HostSkillDir`,各自带自己的 skill 与监视
句柄)。会话看到的 skill 视图是 `SceneSkills::resolve`:先取绑定的共享目录,再按
`SkillManager::yields_to_host` 让宿主层顶替内建、插件包、未受信项目的同名 skill(用户的磁盘层与
受信项目的 `.agents/skills` 不让);宿主目录之间先列的赢。子 Agent、团队成员、`context: fork`、
`session.startAgent` 的委派拿到的是父会话同一个层对象,所以跟着它变。

目录对象由池里的宿主目录表(`BindingCache` 里的 `HostDirTable`,`parts/bindings/host_dirs.rs`)按解开链接后的路径去重
共享:每个不同的目录一个对象、一个 `WatchHub` 订阅(tier `Host`),没有内存里的会话持有它时释放;
变化只就地重扫这个目录,波及持有它的会话,不作废任何绑定。规格里记的是 `skill_dirs`(`RecordedSpec`),
重建、恢复、`config.reload`、重启都按它重新取目录。

每个会话也有自己的命令表,建在它自己的 skill 视图之上(MCP prompt 建时登记,之后的走实时的 MCP 回退),
不取绑定共享的命令目录:场景的 `bundled_skills` 没列的内建,在这个会话里也不是 `/name`。

读写边界:会话自己的宿主目录算它的可读根(严格沙箱档、无人值守场景同);进程里任何会话列过的目录
进入一个只增不减的保护集合,内建工具非只读的调用点名其下路径,在每种权限模式下都拒(阶段 `host_skill_dir`;
MCP 工具与宿主工具不报路径,拦不到,同凭据位置)。沙箱的命令靠建会话时的检查挡:沙箱放开写的每个根
(工作目录、可写目录、包缓存、状态根的 `tmp`、`platform::sandbox::implicit_writable_roots`)里的目录都不收,
检查与 `tools::bash::writable_dirs` 同出一处,由 `session_pool` 的单元测试对着真实会话的写根核对。
出现时是符号链接的目录,在出现那一轮按解析到的位置重新登记、监视、保护。缺口是明说的:冷会话的目录要等
持有它的会话载入才受保护;不点名路径的不沙箱命令拦不住(粘在选项上、由变量拼出的路径也算不点名);
落在另一个会话的项目里的宿主目录,那个会话的沙箱命令可以不点名地写它(建会话时的检查只拒绝落在列它的
会话自己范围内的目录)。落在别的(含未受信)项目里的目录按宿主内容收,列哪个由宿主负责。

### 3.2.2 会话规格:重建时照什么重来

一个会话的 `Agent` 会被重建:它的绑定失效、janitor 回收后再被用到、daemon 重启后恢复。
重建照的是**会话规格**,不是触发重建的那次调用碰巧带了什么:创建时的 `options`、
`session.setModel` 选过的模型与厂商、它与 `session.setPrompt` 改过的会话设置层、`session.setSkillDirs` 设的宿主 skill 目录、
`session.setPermissionMode` 设的 `plan` 以外的权限模式(建引擎时按当时的设置夹紧)。规格以 `ns = "agix.session"`、`event = "spec"`
的扩展条目写进会话日志,每次变化追加一条,最后一条为准,所以跨 daemon 重启也成立。
因为最后一条为准,宿主改规格的那几个方法都持着会话锁(`LiveSessions::with_session_lock`)改,持着锁的
会话不被驱逐;内存里没有的规格先从日志读回再改(`SpecStore::update`),不会写出一份只有这次改动的规格。
recorder 只有录制的落盘(`dir` 建会话时冻结成绝对路径,`session.fork` 去掉 `name`);同一进程里的重建共用同一个 recorder,录像接着写、回放接着原来的进度;daemon 重启后录像从头重写,回放要恢复时再给一次。录制时会话和它的子 Agent、起名、压缩等辅助调用进同一份录像,`session.calls` 经协议读它。

重建之前先停掉旧 `Agent` 并等它收尾(有时限),然后才从盘上读 transcript——否则新
`Agent` 读到的是旧的那个还没写完的历史。关停时同样等所有 `Agent` 收尾。超过时限的旧 `Agent`
已经取消、不再读输入,它的条目随即移出内存,像被驱逐的会话一样等下一次调用把它从日志
恢复;所以换规格的调用(`setModel` 换厂商或推理设置、`setPrompt`)先只改内存里的规格,重建成功之后才
记进日志,失败就退回原来的规格,日志与正在跑的引擎不会各说各的。

会话的后台任务(子 Agent、团队成员、后台程序、Monitor)和宿主的插件函数调用跑在它的运行体之下,停掉旧 `Agent`
就停掉了它们,所以**还有后台任务在跑的会话不重建**:绑定失效的那种推迟,这一轮照旧用现在的
引擎,标记留着下次再看;`session.setPrompt`、要重建的 `session.setModel` 返回 `SESSION_BUSY`
(`reason: "background_tasks"`,另带 `running_task_ids`)。
真重建时,旧 `SessionState` 记着的任务(结束了的那些,`session.tasks` / `TaskOutput` 还查得到)
和模型还没读到的通知(`<task-notification>` 等)带到新的上面。`plan` 模式与 todo 列表不靠这一步:
它们记在日志里(`agix.session_state`,§4),驱逐、重启后恢复的会话同样读得回来。

会话可以指定自己的厂商(`options.provider`):把它排到这个会话 `model.order` 的最前,其余照旧
(§3.2.6)。首轮结束后的自动命名(`options.auto_name:false` 的会话不起)也是这个会话的调用:取绑定的
`model.tasks.title` 那一档,而且用的是套过录制器的那份 client,所以录得进录像、
回放时也从录像里取。宿主可以经 `Assembly.customize_session` 在每个会话的
`Builder` 建成之前改它,新建、恢复、重建都经过这一处。

### 3.2.3 有人值守与无人值守

`SceneSpec::interactive` 是"会话进行中有没有人回答问题、批准计划、应答权限提示"的缺省答案。缺省
`false`;只有交互版 `code` 是 `true`(声明式场景在 `scene.json` 里写 `"interactive": true`)。
会话自己的值(`Builder::attended`、`options.attended`)优先,在 `Builder::resolve` 里定一次再往下交:
子 Agent 与团队成员经 `HostContracts::attended` 随根会话,提示词经 `ScenePromptContext::attended`,
权限经 `SessionEnv::unattended`。这一个答案同时决定四件事:

| | 无人值守 | 有人值守 |
|---|---|---|
| 工具 | 没有 `AskUserQuestion`、`EnterPlanMode` / `ExitPlanMode` | 有;`AskUserQuestion` 另要宿主能回答提问(`Builder::question_prompts`、自带的 `Builder::elicitation`,daemon 上是 `options.prompt_types` 列了 `question`) |
| 提示词 | 不写"问用户""先确认";写"任务没明说就不做,在报告里说明" | 写 |
| 内置技能 | 只有能独立做完的 | 再加需要用户在场的 |
| 权限提示 | 不送给任何人,直接拒绝,拒绝消息里写清怎么授权(§6.3) | 送给宿主等回答 |

内置技能由 `SceneSpec::bundled_skills` 点名,缺省只有 `skillify`;有人值守的会话再加 `SceneSpec::attended_skills`;依赖的工具(`skills::bundled::required_tools`,
如 `loop` 要 `CronCreate`)在场景里不被允许的,也不列。磁盘上和宿主 provider 给的技能不受
影响,同名的照样覆盖内置。`code` 无人值守带 `skillify` `simplify` `verify` `debug`
`security-review`,有人值守再加 `batch` `stuck` `remember` `init` `import` `loop` `rename`
`updateConfig`(即 `code` 的 `attended_skills`)。

运行时另有一个与提示词无关的兜底:同一 agent 在一个 turn 里用同样的输入调同一个工具到第 3、
5、8 次,在结果后附一条提醒,让模型看上次的结果、换方法或停下汇报(`runtime::repeat_guard`)。
只提醒,不拦调用;不触发就不花 token。

文件工具给模型看的路径由 `SceneSpec::path_style` 定缺省:`harnix` 用相对工作目录的路径
(参数说明、Glob / Grep / Write / Edit 的输出都按相对写),其余场景用绝对路径;设置里写了
`path_style` 就以设置为准。相对路径省掉的是每次调用都重复出现的那段项目前缀。

### 3.2.4 会话池里一个会话的回合槽

池里每个会话条目是 `LiveSession { core, engine, turn }`:`SessionCore` 是会话本身(`SessionState`、
事件出口 `SessionHub`、steer 队列、进程组账本、名字、上下文用量、日志是否落后),建会话或从日志冷恢复时建,
关闭、驱逐、整池关闭时丢,重建不换;`EngineSlot` 是当前这一代引擎(输入通道、取消与退出令牌、
命名器、绑定是否过期、委派器、脚本记录、健康检查);`TurnSlot` 是回合状态,不再由几个互相对得上的
字段隐式表示:

- **阶段** `SessionPhase`:`Idle`、`Running`(带回合 id、发起方、发起的订阅者)、`Rebuilding`、
  `Exclusive(SessionOp)`(目前只有 `Context`:`inspectContext` / `dropMessages` / `editMessage`
  在一次锁里检查并占槽,答完才放;期间的 `run_turn` 报 `context_operation` 或排队,改动由引擎在
  `handle_context` 里写 `agix.context`)。
  所有转换在池的 `sessions` 锁里同步完成;`is_busy`、`interrupt`、`setModel` / `setPrompt` 的预检查、
  `inspectContext` 一类的上下文请求、目标的换轮,读的都是阶段,不去 `try_lock` 别的东西。
- **占槽与命名是一次转换。** `run_turn` 在一次锁里判定目标轮、占槽,`Idle` 直接进 `Running`,
  回合从占住那一刻起就有名字:这时到达的 `session.interrupt` 发的是 `CancelTurn { turn_id }`,
  引擎在消息到达时记得它已被取消,这一轮以 `cancelled` 结束。
- **占槽之后的部分由池 spawn 的回合驱动跑。** `run_turn` 只等它的 `JoinHandle`:调用方的 future
  被丢下,这一轮照样跑完、还槽。驱动没还槽就结束(panic)时,`TurnTicket` 的 `Drop` 把这个条目
  按 `engine_fault` 移出内存 —— 引擎的事件接收端跟着驱动没了,这个引擎不能再用;`run_turn` 得到
  `kind: internal` 的失败。
- **重建是 `Idle → Rebuilding`。** 先按同一个 `SessionCore` 造好下一代引擎(状态经
  `Builder::session_state` 注入,还没挂上),造不出来就回到 `Idle`、旧引擎照跑;造好了才停旧引擎,
  旧引擎停下之后新引擎 `attach_session`,只读回 transcript,不再从日志恢复状态,最后在同一个
  `LiveSession` 里换掉 `EngineSlot`、回到 `Idle`。同一时刻最多一个引擎挂在一份状态上。任务与目标
  的转发器每个会话只起一次,跟着 `SessionCore` 走;重建期间
  上下文请求与只换模型的 `setModel` 报 `SESSION_BUSY`,`on_busy: "steer"` 的 `run_turn` 等重建结束,
  不往正在停止的旧引擎里塞。
- **代际号** `Generation`:`LiveSessions` 上的计数器,每造一个引擎取一个,也是这个引擎挂到会话状态上
  用的号(`detach` 只摘自己那一代)。回合票据 `TurnTicket` 带着 `Arc<SessionCore>` 和代际号:
  `TurnComplete` 的 context、首轮命名写的是票据上的 core,`session.close` 之后同一个 id 恢复出来的是
  另一个 core,旧回合碰不到它;代际号只用来判断还回回合槽时是不是同一代引擎。
- **事件出口** `SessionHub` 每个会话一个(叶子锁:订阅者、待答提问、最近活动时间),事件当场同步交给
  每个 `SessionWatcher::deliver`,不去锁池;交的时候不持着 hub 的锁,投递由另一把锁串行,`deliver`
  里再往同一个 hub 发帧不会卡住。回合的终止帧(`turn_complete`、`error`)是在池的 `sessions` 锁下
  发出的,所以 `deliver` 不能等池,要用池的工作交给它另起的任务。池不拼 JSON 帧也不认识 `rpc::Client`:会话事件是领域类型
  `SessionFrame`,线上的 §7 帧由 `rpc/events.rs` 的 `SessionEventPayload::project` 投影,
  `server/watch.rs` 为连接实现 `SessionWatcher`。投递是同步入队,所以 `turn_complete`
  一定排在同一连接上 `run_turn` 的响应之前。定时任务与目标轮的 `run_turn` 不带发起订阅者
  (`Option<Arc<dyn SessionWatcher>>` 传 `None`),它们的帧照样发给全部订阅者。
- **待答提问以引擎为准。** 引擎的待答表(`PendingPermissions` / `PendingQuestions` / `PendingMcpAnswers`,子 Agent 经
  `ParentElicitation` 用父会话的那张)由 `OpenPrompt` 登记;回答、超时、回合取消都在它的 `Drop` 里
  一处结束,同时发 `AgentEvent::PromptResolved`。`SessionHub` 收到这个事件才把提问从待答里去掉,并在
  同一把锁里广播 `prompt_resolved`;`respondToPrompt` 只把回答交给引擎,不自己去掉。
- **错误是领域类型。** 池的方法返回 `PoolError`,池里不出现错误码;`server/errors.rs` 一处 `match`
  决定错误码、message 和 `data` 的形状。场景检查(`SCENE_MISMATCH`)在池里做,定时任务与目标轮不检查。
- **`SESSION_BUSY` 带 `reason`**:`turn_running`(必带回合 id)、`rebuilding`、`background_tasks`、
  `previous_engine`(上一个引擎在宽限期内没停下),说的是会话此刻在忙什么,不是哪个方法被拒
  (协议文档 §9.1)。

### 3.2.5 会话池的组成与锁序

`SessionPool` 是门面。状态按所有权分给 `session_pool/parts/` 里的协作者,字段只在各自的
模块里可见;`session_pool/facade/` 里的 `impl SessionPool` 只做编排。门面 12 个字段,每个
协作者不超过 11 个(`tests/guards/tests/layering.rs`,字段数另有只降不升的棘轮)。

| 协作者 | 持有 | 锁 |
|---|---|---|
| `LiveSessions`(`parts/live.rs`) | 在内存的会话表(每个 `LiveSession` 带 `SessionCore`、`EngineSlot`、`TurnSlot`)、每会话的磁盘临界区锁、容量与空闲超时、还在停的旧引擎、关闭状态、代际号;门面经 `with_live` / `with_live_mut` / `core_of` / `insert_evicting` / `remove_if` 等窄方法读写,拿不到表本身 | `session_locks[sid]`、`sessions`(tokio);`stopping`(std,叶子) |
| `SpecStore`(`parts/specs.rs`) | 每个会话的规格(§3.2.2),和出了内存仍要留着的那些 | `specs`、`kept`(std,叶子,互不嵌套) |
| `BindingCache`(`parts/bindings/`) | 自家那一对热替换的设置、托管层、每对的 `ProjectBinding`、`last_good_routers`、`WatchHub` | `bindings` → `last_good_routers`(tokio);`settings`(tokio `RwLock`,取值即放) |
| `SceneCatalog`(`parts/scenes.rs`) | 默认场景、激活的场景、场景注册表、宿主登记与 `scene.register` 的场景 | std `RwLock`,叶子 |
| `McpHub`(`parts/mcp_hub/`) | 按 `(project, scene)` 缓存的连接、登录状态、准入策略;没编进 MCP 时是桩 | 缓存的表锁只在找、加、删槽时持有;连接在槽自己的更新锁下 |
| `SessionDirectory`(`parts/directory.rs`) | 读会话日志里的事实(`Meta`、计数、是否结束)、服务过的项目 | `served`(std,叶子) |
| `EngineFactory`(`parts/factory/`) | 内置工具、宿主权限、Rust 插件、customizer、权限提示时限,以及宿主连接登记处 `HostConnections`(`parts/hosts.rs`,§3.5);按 `BuildRequest` 建 Agent,不插表 | `HostConnections` 的表(std,叶子) |
| `Scheduler`(`parts/scheduler.rs`) | 定时任务表;到期的任务交给门面实现的 `TurnLauncher` | 无 |
| `DaemonEvents`(`parts/events.rs`) | `daemon.subscribeEvents` 的广播和池级遥测 | 无 |

插件子系统 `PluginSubsystem` 以 `Arc` 共享,`BindingCache` 读它的贡献来建绑定。

**协作者持着自己的锁时不调用别的协作者。** 方法只返回值,协作者之间的配合由门面在锁外编排:
比如绑定只报告"某层冒出了新文件"(`Cached::Appeared`),自家那一对要不要走带 MCP 对账的完整
重载由门面决定;插件贡献的场景由门面作参数交给 `SceneCatalog`;`EngineFactory::build` 要的
场景表、定时任务表、MCP 模块、进程组账本都由门面先取好放进请求。

**全局锁序:** `session_locks[sid]` → `sessions` → `TurnSlot.rx` → { `SessionHub.inner` |
`SessionCore.*` | `SessionState.*` }(叶子之间互不嵌套);`bindings` →
`last_good_routers`。其余 std 锁都是叶子,持锁时不 `.await`。`HostConnections` 的表也是叶子:持着它不拿别的锁、
不调订阅者;`SessionHub.inner` 可以在自己的锁下查它,反过来不行。

**冷建的编排**(`create`):拒绝关闭中的池、等同 id 的旧引擎停下(`LiveSessions`)→ 认领日志 →
取规格(`SpecStore`)→ 建 `SessionCore` → 备部件(场景表、定时任务表、MCP 模块、进程组账本、
插件脚本)→ `EngineFactory::build`(带 `SessionSeed` 与 `Restore::{New, FromLog}`)→ 写 `Meta`
→ `attach_session` → 读 transcript、从日志恢复状态 → 起运行体 → 在 `sessions` 锁里腾位置、插表,
被挤出的会话由门面发 `session_evicted` → 起这个会话的任务与目标转发器。**重建的编排**
(`rebuild_engine`)见 §3.2.4:`Restore::Kept`,先造后停再挂,换的只是 `EngineSlot`。转发器只持有
会话的 `SessionHub`、状态和 `TurnSlot`,不需要池;池由 `Arc::new_cyclic` 建成,构造好就转发轮间事件。

### 3.2.6 模型路由:provider、档位与回退

设置里的 `providers` 是一张带 `id` 的列表:每项有 `vendor`、`api`(协议,`ModelFactory` 按它建 client)、
`base_url`、`api_key_env`、`models`、`tiers`(`strong` / `balanced` / `fast` 各由哪个模型担任;`strong` 必填,
缺的档向上补)和可选的 `quota`。`model.order` 是 provider id 的先后顺序,第一个是默认;没写时只用第一个定义的
provider,一个 provider 也没定义时引擎自己提供 `anthropic`(密钥取 `ANTHROPIC_*` 或宿主的 `CredentialSource`,
三档都是 `--model`)。`model.tasks` 给每类调用(`main`、`subagent`、`compact`、`memory`、`title`、`web_search`、
`web_fetch`)指定要的档位;`web_fetch` 缺省不设,`WebFetch` 不用模型。

路由在 `model::routing`:`ProviderSet` 是一个 (项目, 场景) 的 provider、顺序、档位,连同它们的健康(冷却期)
和配额记账,绑定建一个、它上面的会话共用;`SessionRouter` 是一个会话的视图——它自己的顺序(会话指定的
provider 排第一)和上一次调用去了哪(只用来判断要不要报 `provider_switched`)。runtime 和模块只认 `base` 里的
`TaskRouting`(`model_for`、`model_name_for`、`resolve_model`、`observe`),不依赖 `model`。
每次模型调用从 `order` 的头开始找可用的 provider,所以前面的恢复了就自动切回去。
一个 provider 配额用尽、没有凭据、凭据被拒(401 / 403)、连不上或超时、5xx / 529 / 429 在客户端重试用尽后,
或回 402 时交给下一个;连接类故障记一个冷却期,期间不再试它。400 一类是请求本身的问题,不换别家。只有一个
provider 时报它自己的错误种类;`order` 里的 provider 都不行时这一轮以 `providers_unavailable` 失败,
`message` 逐个列出原因。回退发生在调用边界:切换时去掉上一家私有的推理内容(thinking 签名、
`reasoning_content`),会话收到一条 `session.event` 的 `provider_switched`,遥测记同名事件。哪家接了一次调用由这次
调用自己的流说(第一个事件 `ModelEvent::Served`),推理内容按它打标;同一个模型上同时在跑的几次调用(主循环和后台
prompt hook)互不相干。子 Agent、压缩、
记忆、标题、搜索走同一个路由;会话或 `session.setModel` 指定的模型 id 落到别家时,先查它在原 provider 里是哪一档,
再取新 provider 里同一档的模型。

配额按 provider id 计,口径是未命中缓存的输入加缓存写入加输出,缓存命中不计;用量落在状态根的 `quota/`
(§8),文件名就是 id 本身。所以 provider id 只能由小写 ASCII 字母、数字、`-`、`_` 组成(不区分大小写的文件系统上 `Ab` 与 `ab` 是同一个文件):别的 id 加载时整项忽略,
`daemon.doctor` 在它所在层的 `ignored_values` 里列出原因,路由也拒绝它。越过上限的那一次调用不被打断,
下一次调用起这个 provider 不可用,直到周期翻过去。`config.getProvider`
报各周期的已用量与上限。

信任:不受信项目的 `providers` 整段不生效,`model.order` 只能排全局 / 场景层定义的 provider(§6.6);所以项目写不出
一个把流量或环境变量送到别处的 provider。

### 3.3 Sidechain 会话

子代理(`Agent` 工具、Skill `context: fork`、team 成员)的 transcript 独立成一个会话,
通过 `Meta` 里的 `parent_session_id` + `session_kind: Sidechain` 与父会话关联。
可经 `HistoryStore::child_sessions` 找到,默认不出现在 `session.list`。

跑完写 `LogEntry::SessionEnd { state }`。**被取消的轮次刻意不写这个标记**——取消是被打断,
不是到达终点,所以会话必须保持可 resume。对已写终止标记的 sidechain 发起 resume 会被拒。

关闭父会话会**级联删除**其全部 sidechain:父会话一旦关闭,就没有任何路径能再触达它们。

### 3.4 委派

#### 一个入口

模型调 `Agent`、`TeamCreate` 的成员、常驻团队成员、Skill `context: fork`、`"agent"` 型 hook、
宿主的 `session.startAgent`,都构造一个 `base::interface::delegate::DelegateRequest`,交给
`Delegator::delegate`。这是委派唯一的分派:谁来调、怎么跑,由请求里的 `DelegateMode` 决定,
其余(类型、模型、工具收窄、拒绝规则、授予、输出 schema、工作目录、取消、询问去向、调用点)
都在请求的字段里。`delegate` 就是 agents crate 里的 `Delegator::dispatch` 不带 worktree 的那一种;
带 `worktree` 的后台 `Agent` 直接调 `dispatch`,把 worktree 交给后台任务在结束时收尾(下文)。

| 调用方 | `DelegateMode` | 返回 |
|---|---|---|
| 模型的前台 `Agent`;不带 `background` 的 skill fork | `Wait` | `Finished(DelegateOutcome)` |
| 模型的后台 `Agent`;`background` 的 skill fork | `Background { notify_parent }` | `Started(StartedAgent)` |
| `session.startAgent` | `Host { notify_parent }` | `Started` |
| `TeamCreate` 的阶段成员 | `TeamMember { lifetime: Stage { .. } }` | `Finished` |
| `Agent` 的 `team_name` + `name` | `TeamMember { lifetime: Persistent }` | `Started`(这条消息的任务) |
| `"agent"` hook | `Hook { judged_session }` | `Finished` |

- `DelegateMode` 是封闭枚举,没有 `#[non_exhaustive]`:分派对每个模式写明分支,不留通配
  (`DLG-6`),新加一个模式不写它的规则就编不过。`DelegateRequest` 与 `DelegateError` 是
  `#[non_exhaustive]`,crate 外用 `DelegateRequest::new(prompt, mode, stops_with)` 构造再改字段。
- `DelegateOutcome` 带最终文本、`DelegateEnd`(`Completed` / `Cancelled` / `Failed` /
  `CutShort`)、结构化输出和子会话 id;失败和被取消是一次结果,不是 `Err`。`DelegateError`
  只表示没起来:`Limit`、`Depth`、`Unsupported`、`Build`、`Invalid`、`NotJudged`。
- 调用方的取消经 `StopsWith` 进来:`Call(token)` 是调用方的令牌(子会话停在它的子令牌上),
  `Session` 是父会话的结束令牌。委派路径与 hook 执行器不自造无源的 `CancellationToken`(`DLG-8`)。
- `Delegator::stop_member` 停一个常驻成员。

实现在 agents:`agents::delegate::Delegator` 持有 `DelegateEnv`、`AgentCatalog`、`ToolPool`、
`Limits`、`PersistentMembers`。`DelegateRequest` 的字段在 `delegate` 里折成内部的 `Spec`,再按
模式走 `wait` / `start` / `stage_member` / `message_member` / `judge`,它们最终经
`Delegator::build_delegate` 建出子会话(`DLG-2`)。

#### 父会话只捕获一次

`DelegateEnv` 在 agents 模块的 `on_session` 里由 `SessionContext` 一次取全:宿主契约、
父会话的 id、状态、事件、待决权限与结束令牌、hook runner(弱引用)、场景、设置、模型、
给子会话的 MCP 份额、skill 目录、深度。之后不改,没有「事后填入」的槽:`agents` 与 `team`
里没有 `SetLater`、没有 `RwLock<Option<…>>` 字段、`AgentTool` 与 `Delegator` 上没有 `set_*`
(`DLG-4`)。唯一晚绑定的是 runtime 的 `LateDelegator`,因为读它的 hook、skill 与宿主比 agents
模块先建好。

#### 子会话继承什么

子会话由 `runtime::agent::Builder` 建出,在整个 agents crate 里只有 `build_delegate` 一处
`Builder::new`,它先 `.inherit(env.host.clone())`(`DLG-1`)。`HostContracts` 是宿主为这台机器
与部署答出的那一整份:`exec`、`token_counter`、`rules`、`environment`、`permission`、
`history_store`、`telemetry`、`task_router`、`scene_registry`、记忆(存储、召回、召回钩子)、
`instruction_provider`、`plugins`、`modules`,以及 `HostLayers`(工具与提示中间件、结果变换、
模型拦截、恢复策略、压缩器)。`Builder::build` 解析一次,交给每个模块看(`SessionContext::host`),
`inherit` 把这一整份设到子会话的 builder 上。

`Builder` 的每个字段恰好属于三类之一(`DLG-3`):由 `HostContracts` 整份继承;由每个委派自己
决定(场景、模型、工具、设置、会话 id、血统、`tool_selector`、`mcp_servers`、深度、team 席位……);
不给委派(`frozen`、`session_state`、`steering`、`run_slots`、`cron_store` 这类宿主自己的)。
新增 `Builder` 字段不分类,守卫过不了。`inherit` 必须先于委派自己的 setter 调用——自己的
(例如收窄后的权限)后设、覆盖继承来的。

工作目录来自请求的 `workdir`。与父会话的不同时(`Agent` 的 `worktree`),它记在子会话设置的
`PathSettings::worktree` 上:工具的工作目录、提示词里的工作目录、hook payload 与宿主调用帧的
`cwd` 都是它,项目仍是父会话的——项目层设置、`.agix/hooks/`、信任与插件照项目根算;worktree
里检出的 `.agix` 只是提交进仓库的那一份,不当项目层读。它的可写目录另加仓库 git 目录里在 worktree 里提交
要写的几处(`base::paths::WorktreeGit::writable`):对象库 `objects/`(其中的 `objects/info/` 除外)、
这个 worktree 在 `worktrees/` 下那一项(其中的控制文件除外,见 §6.3)、它自己分支
`agix/worktree-<名>` 的 ref、ref 的 `.lock` 与这个分支的 reflog。git 目录的其余部分它读得到、
写不到:别的分支与 tag 的 ref、`packed-refs`、`info/`、主检出的 `HEAD`、索引与 reflog、别的
worktree 那一项、配置与 hooks。所以它能 `git add`、`git commit`(含再提交与 `--amend`),动不了
别的分支;它的分支被打包进 `packed-refs` 之后(用户自己跑过 `git gc` / `git pack-refs`)它就提交
不了——建 worktree 时分支是松散的 ref,不是的就不给。git 目录本身另进它的可读目录。整个项目根
不给它。

这个 git 目录是 `create_worktree` 建 worktree 时得到的那个(`find_repo` 的 `--git-common-dir`),
连同建完时 `.git` 链接指到的那一项与分支名作为 `WorktreeGit`,随 worktree 一起经
`Workdir::Linked` 交给委派,记在 `LinkedWorktree::git`,之后不再顺着
worktree 的 `.git` 链接与 `commondir` 去找——沙箱里早先起的命令(上一次 `Bash` 留在后台的进程、
并行的子 Agent 的命令)可能已经改过这两个文件。收下它的每一处都核对:建完时链接要从
`<git 目录>/worktrees/` 下直属的那一项指回这个 git 目录、分支是松散的 ref(`tools::worktree`),
委派建设置时与用它时(`PathSettings::worktree_git`)git 目录解析后要等于 `<项目根>/.git`、那一项
在它的 `worktrees/` 下直属、分支名不出 `refs/heads/`;不符就不给,记一条
警告,子 Agent 只是提交不了。项目在仓库的子目录里时它不等于 `<项目根>/.git`,同样不给——根会话
那时也写不到仓库的 git 目录。沙箱的守卫拿的也是它(`SandboxPolicy::worktree_git`),不读链接。

于是这样的子 Agent 的文件工具,不问就能读的范围是 worktree 加这个 `.git`、能写的是 worktree 加
上面那几处,主检出里的文件要审批,与它的命令能写的范围一致。它再用 `worktree` 起子 Agent 时,`create_worktree` 从项目根
找仓库,新 worktree 与它的并列在 `<项目根>/.wkts/` 下,不嵌在它自己的 worktree 里
(从 worktree 里找,仓库根是那个 worktree,git 目录不在 `<根>/.git`,沙箱里会被拒)。

权限规则里的相对路径按会话的 `cwd` 解析,worktree 子会话的 `cwd` 是 worktree;只这样解析,项目的
`Read(.env)` 就护不住主检出里的 `.env`(用绝对路径或 `../..` 去拿)。所以 `SessionEnv` 另带
`refusal_rule_roots`:工作目录是 worktree 时为项目根。路径规则的 deny 与 ask 对 `cwd` 和这些根各
判一次,哪一处命中都算;allow 只按 `cwd` 判,不因此变宽(`permissions::policy::subject`)。搜索
工具走目录时跳过的 deny 路径(`tools::security::walk_exclusion`)同样两处都算。反过来,worktree
里的文件是主检出里同一路径的一份拷贝:路径落在某个 `<目录>/<worktree 目录>/<名>/` 下时(会话的
worktree 目录,以及不是它时的缺省 `.wkts`,§8.1),把这两段去掉得到的 `<目录>/<其余>` 也算它的别名,deny 与 ask
在别名上命中照样算,allow 不算(`base::paths::Layout::main_checkout_copies`,`RuleSet::evaluate_path` 里与
符号链接的别名同一处处理)。于是锚在项目上的规则——项目下的绝对路径,或根会话里的相对路径——对 worktree 里那一份同样
成立,不论是 worktree 子 Agent 自己读,还是根会话去读一个留下的 worktree;worktree 是从仓库根建的,
项目在仓库的子目录里时,`<仓库>/.wkts/wt/sub/x` 的别名是 `<仓库>/sub/x`,正好落在项目下。
这只是按路径的字面:不读磁盘,也不问那里是不是真有一个 worktree。这一层不认识 worktree 的来历,只认
这份根的清单与这个目录名。

子 Agent 的侧链 `Meta` 记下它的 `cwd` 与父会话的项目(`project_root`)。冷恢复侧链时
(`session.resume`,或 `run_turn` 指名它)按记下的项目建绑定,所以它还是在项目里,不会变成无项目
会话。`cwd` 不是项目根的,是它工作过的 worktree:恢复前再核对一遍(`tools::worktree::reopen_worktree`)——
它仍直属于 `<仓库>/` 下的 worktree 目录(按恢复时绑定的设置,含缺省 `.wkts`)、项目在这个仓库里、它的 `.git` 链接与 `commondir` 仍指回
`<仓库>/.git`——通过了才把它连同这个 git 目录记进会话(`SessionCore` 的 worktree,每一代引擎的设置
`PathSettings::worktree` 都取它),工作目录、可写目录与 `refusal_rule_roots` 于是与当初一样;git
目录取的是 `<仓库>/.git`,不是链接里写的,照旧要等于 `<项目根>/.git` 才给。worktree 已经不在或核对
不过的,恢复被拒,错误里写明原因,不在主检出里接着干。

worktree 由 `Agent` 工具建、由它收尾(`tools::worktree::WorktreeHandle::remove_if_untouched`):
子 Agent 结束时,worktree 里没有未提交或未跟踪的改动、HEAD 与 `agix/worktree-<名>` 分支都没有
建它那个提交以来的新提交,才把目录与分支一起删掉;否则两样都留着,前台调用的结果末尾、后台
子 Agent 的任务通知里各有一段 `<worktree>`(路径、分支、为什么留),后台的任务 `detail.worktree`
也记着。git 读不出状态时同样留着。被 `.gitignore` 掉的文件不算改动,随目录一起删。起不来的
后台子 Agent(名额满、会话在关)它的 worktree 当场按同样的规则收尾。留下的 worktree 没有别的
路径会删:回退只报告 worktree 已被删掉的文件(`WorktreeRemoved`),会话结束与 daemon 退出不碰
它,也没有别的路径清理;同名再起会因目录已在被拒。

agent 类 hook 的子 Agent 由根会话的 delegator 起,子会话共用根会话的 `HookRunner`。它在 payload
的 `cwd` 里干活,前提是那是项目根或 `<项目根>/` 下 worktree 目录(会话的与缺省 `.wkts`)里直属的一个目录,否则仍在根会话的
工作目录(`runtime::hook_executors::DelegatorHookExecutor`)——所以判 worktree 子 Agent 的调用时,
它读的是那个 worktree。

#### 深度

委派链由 `agents.max_depth`(默认 3,即 `EngineConfig::max_agent_depth`)**计数**约束,
不靠从工具集里摘掉 `Agent`。`Limits::spawn_guard` 在每一条建会话的路径上检查;到顶时返回
`DelegateError::Depth`,模型看得见,可以改为自己做。同时在跑的个数另有上限,见 §3.8。

**过滤工具名不构成约束**:`Builder::build()` 会为子会话新建注册表,子会话自己的 agents
模块再把 `Agent` 注册进去(到顶时不注册),与父级传入的工具集无关。

#### 子会话的工具

子会话的工具在 `Delegator::build_delegate` 一处决定,上表的每条路径都走它:

- **只继承模板里的内建工具**:父会话建的时候传进 `Builder::tools` 的那一份
  (`SessionContext::template_tools`),只要 origin 是 `Builtin`、不是 `Agent`、父会话还留着
  的。引擎、模块、场景、插件、MCP 在建会话时注册的不继承,子会话建的时候注册自己的那份。
- **收窄只在子会话的 `ToolPolicy`**:agent 类型的 `allowed_tools` / `disallowed_tools`,
  加上请求的 `tools`(skill 的 `allowed-tools`、`session.startAgent` 的两个列表),合成一个
  `ToolSelector` 经 `Builder::tool_selector` 交给子会话;它和场景的工具面一起管子会话的
  全部工具:继承的、引擎和模块注册的、场景和插件贡献的、MCP 适配器(运行期才接入的也算)。
  只有 `ToolSearch`、`StructuredOutput` 不受它收窄。请求只能收窄,不能放宽。
- **拒绝与授予**:请求的 `deny_rules` 只有 `deny` 生效——子会话可以比父会话多被拒绝,
  不能多被允许;`grants` 是只在子会话自己会话里生效的 Turn 授予,不回传父会话。
- **MCP 按 server 授予**:按 `mcp_servers` 授予(§1.1);类型写了 `allowed_tools` 时,
  授予的 server 视同列在其中。`mcp__<server>` 按工具的 `ToolOrigin::Mcp` 比对配置里的
  server 名,不从工具名里解析(`DLG-5`)。
- **子 Agent 没有定时任务**:子会话不注册 `Cron*`,也不起调度。
- **宿主工具只给声明了 `subagents` 的**:它们不是模板里的内建工具,`build_delegate` 显式把父会话
  `subagents:true` 的那几个交给子会话,再受子会话的 `ToolPolicy` 收窄;子会话的调用经
  `DelegateHostCalls` 走父会话的通道,带上它的任务与侧链会话。

`build()` 结束时检查工具表没有重名(重名时分发只认第一个):测试与 debug 构建里断言失败,
release 构建记一条 error。

### 3.4.1 会话目标

目标(`base::context::goal`)让一个会话一轮轮自己推进:一轮正常结束(`end_turn`、
`stop_sequence`、`max_tokens`)时,目标若仍是 `active` 且已武装,就开下一轮,注入
`<goal_round>` 提醒目标与轮次,直到模型标完成或受阻、或轮数或 token 预算用完(转 `blocked`)。

- **谁开下一轮**:daemon 下是会话池,和定时任务同一条路径(`run_turn`,`initiator:"goal"`),
  会话用 `Builder::goal_rounds_from_owner()` 建;直接嵌入时 `Agent::run` 自己开。
- **谁能做什么**由这一轮是谁发起的决定(`TurnOrigin`,从 `turn_id` 读出,模型改不了):人发起的
  一轮什么都能做;自动轮只能标完成或受阻,受阻要从自武装起的第 3 个自动轮起;子 Agent 碰不到目标。
- **一次只等一轮**:`next_round` 交出下一轮时把它记成待开始,不计数;这一轮的 turn 开始时
  `claim_round` 认领它才计数。等着的期间别的 turn 结束不会再交出一轮,目标被暂停、清除、
  修改或解除武装则作废这一轮,认领失败,它就不跑。
- **武装不落盘**:每次变化以扩展条目(`ns:"agix.goal"`)记进会话日志,从日志恢复的目标一律不
  武装,要人说继续;daemon 在内存里原地重建会话时把武装和待开始的那一轮带过去。一轮以取消、
  hook 中止、预算耗尽或出错结束,也解除武装。
- **目标的预算**:目标的各轮在用量链上插一层目标的节点(`SessionLedger::goal_spent`),`token_budget`
  是它的上限,目标设下时从零算,只算目标的轮(不算设它的那一轮、也不算期间宿主或定时任务的轮)。
  剩余付不起一次调用(减去这次调用输入的估计后不到 `MIN_CALL_TOKENS`)时,这一轮在调用前以
  `budget_exceeded` 结束,目标转 `blocked`;交下一轮前也查一次。要继续,先把 `token_budget` 调高再恢复。节点的累计随 `agix.spend`
  落盘,恢复时按目标 `id` 取回。模型设或改的目标,`max_rounds` 不超过设置的 `goal.max_rounds`
  (`ModelAutonomy`,经 `EngineConfig` 给工具),宿主设的只受格式上限 10000 约束。模型经 `CronCreate`
  建的定时任务同样由 `ModelAutonomy` 约束(`schedule.*`),判在工具里、不在宿主也用的 `CronStore` 里;
  任务按根会话计(子 Agent 没有定时任务工具,`CronCreate` 在 delegate 里也拒绝)。

### 3.5 场景的工具面

场景对工具有三种方向不同的控制:

| 字段 | 作用 |
|---|---|
| `tools.allow` / `tools.deny` | 白名单与黑名单,与注册表取交集——**只能减** |
| `extra_tools` | 贡献注册表里没有的工具——**只能加** |
| `tools.deferred` | 哪些工具只报名字不带 schema |
| `options.host_tools`(会话的,不是场景的) | 宿主自己实现的工具——**只能加**,与 `extra_tools` 同一个重名检查 |

`allow` 与 `deny` 就是一层 `ToolSelector`,与智能体类型、调用方的名单同一套匹配:黑名单
始终生效,白名单为 `None` 时放行黑名单以外的全部;`X*` 按前缀匹配,`mcp__*`、`mcp__<server>`
按工具的来源(`ToolOrigin`)匹配 MCP 工具,不从工具名里拆。这份判定作用在会话注册表本身:`Builder::build()` 把不许的工具从注册表里移走,之后运行期才出现的 MCP 适配器
也按它过滤。宣告、`ToolSearch`、分派读的是同一张表,不存在"不宣告但能调用"的工具。

这份判定在 `Builder::build()` 里算一次,成为会话的 `ToolPolicy`(`runtime/src/tool_policy.rs`):
场景允许的,加上插件经 `Registrar::admit_tools` 放行的自己登记的工具(场景 `tools.deny`
里的除外)。注册表过滤、每次请求的工具宣告、MCP 适配器同步都读这一份;`runtime` 里别处不直接读
场景的 `tools.allow` / `tools.deny`(也不问 `permits_tool`),`daemon/tests/wiring_coverage.rs`
守着这一条。

**Tool 接口是 9 个方法**:`name`、`description`、`spec`、`input_schema(style)`、`guide`、
`call_traits(input)`、`permission_facts`、`validate_input`、`call`。与调用输入无关的事实写在 `spec()`
返回的 `ToolSpec` 里:`origin`(`Builtin` / `Mcp { server }` / `Plugin { name }` / `Host`,来源不再靠 `source`
字符串的前缀)、`traits`(只读、并发安全、破坏性)、`stance`(`ModeStance`:普通、协调类原语、退出
plan 模式的那一次调用)、`short_description`、`callable_from_code`、`code_result`、`needs_dispatch`;
看输入才知道的(`Bash` 与 `Monitor` 的只读)覆写 `call_traits`。**权限面只有 `permission_facts`**:
工具陈述这次调用的匹配内容、够多远、是不是沙箱里的命令,缺省什么都不声称、于是要问(§6.1)。
并发分批、`RunCode`、工具表、`ToolSearch`、遥测都读 `spec()` 与 `call_traits()`,权限层读
`ToolFacts::gather` 汇集的那一份,没有第二套能力方法。`ToolSpec`、`CallTraits`、`ToolOrigin`、
`PermissionFacts`、`ToolFacts` 是 `#[non_exhaustive]`,加字段不破坏实现方。
工具没有"我是延迟的""我被谁包过"这样的属性,见下面第 3 条。

宿主声明的工具(`Builder::host_tools`,daemon 上是 `session.create` 的 `options.host_tools`)是
同一种 `Tool`:origin 为 `ToolOrigin::Host`,在场景 `extra_tools` 之后、插件工具之前注册,
和插件登记的工具一样过场景白名单、受 `tools.deny` 约束,`deferred` 的并进会话的延迟名单;
宣告、`ToolSearch`、权限、hooks、中间件、结果转换器、遥测都不另开一条路。它的 `call` 只是把
调用交给 `HostCalls`(`base::interface::host_call`),引擎自带的实现发 `AgentEvent::HostToolCall`、
等 `InputMessage::HostToolResponse`,超时、取消、宿主不在都有确定的结局,一轮总能收尾。挂在会话
而不挂在场景上:场景是 daemon 上所有连接共享的,工具是某个宿主连接的能力。

**连接级的工具与 hook(`daemon.setHostTools` / `daemon.setHooks`)。** 工具除了随会话声明,还能挂在
宿主**连接**上:登记处是 `EngineFactory` 里的 `HostConnections`(`parts/hosts.rs`),按连接记下它提供的
工具、登记的 hook 与能把帧交到它手上的订阅者。它是叶子锁,持着它不拿别的锁、不调订阅者;
`SessionHub` 可以在自己的锁下查它。

- **工具在建引擎时取快照**,经 `BuildRequest` 并进 `HostToolDecl`(带提供者的 `HostId`),所以只对此后
  建起来的引擎生效,不进会话日志与 `options`,重建时重新取;会话自己的 `options.host_tools` 同名项优先,
  与插件或场景工具重名只丢连接这一个并记 warn。**hook 每次触发时现读**登记表(`HookRunner::with_hosts`),
  所以对已建好的会话也立刻生效,且叠加在设置 hook 之后跑——设置 hook 先、拦下就不问宿主,宿主看到的
  是设置 hook 改写后的输入。
- **回答只认提供者。** 调用带着 `HostId`,`session.respondToolCall` 与 `session.reportToolProgress` 的
  来路连接对不上就拒绝(`INVALID_PARAMS`);`session.respondHook` 只认登记者。
- **帧的收件人:** 连接提供的工具的 `host_tool_call` 发给订阅者 ∪ 提供者,不论提供者订没订阅这个
  会话;`hook_call` 只发给登记者。这样旁观的订阅者看不到、也答不了 hook。
- **断开的顺序:** 先从登记处摘掉这条连接并放开它的锁,再逐个 hub 结束它在途的调用
  (`host_gone`)。两步之间发起的调用,在 hub 锁下查登记处已经查不到提供者,当场以 `no_host` 结束,
  不会悬着。工具与 hook 都是,hook 的失败再按各自的 `on_error`。
- 登记了 hook 的连接能看到、能拦下整个 daemon 里每个会话的工具调用——这是它的影响面,不是漏洞;
  所以数量有上限(一连接 16 条、8 条连接,同一连接上悬着的调用 256 个),并只在宿主被信任时使用。

三条硬约束:

1. **`extra_tools` 重名直接失败,不覆盖。** 注册表按名字返回首个同名项,静默覆盖会让
   调用点拿到的和模型看到的不是同一个工具。检查跑在引擎态工具注册**之后**,
   否则场景能占用 `Agent` / `Skill` 这些名字并赢得查找。
2. **`ToolSearch` 永不 defer。** 它是取回 deferred 工具 schema 的唯一途径。
3. **延迟是会话属性,不是工具属性。** `Builder::build()` 把场景的 `tools.deferred`
   写进 `EngineConfig::deferred_tools`,请求的工具表、`ToolSearch` 和调用失败时附 schema
   都读这一份名单;会话中途接入的 MCP 工具同样按名单延迟。工具自己没有"我是延迟的"
   这种声明,也没有包装器。

### 3.6 场景收窄模块

`SceneSpec::modules`(`allow` 为 `None` 是全部,`deny` 再减)对模块做的事和 `tools` 对工具做的
一样:**只能减**。`Builder::build()` 在任何模块运行之前按 `ModuleSurface::permits` 过滤装配
列表,`SessionContext::host` 里的 `modules` 和子 agent 拿到的都是过滤后的那份。留下的模块 `requires`
一个被排除的模块时构建失败——依赖关系由模块声明,场景不能悄悄拆掉它。

### 3.7 声明式场景

`scene::DeclaredScene` 把 `scene.json` + `prompt.md` 读成一份 `SceneSpec`(外加来源目录),
提示词是模板渲染的 `ScenePrompt`。它和 Rust 写的场景一样是加载后不再变的值:两个文件只读一次。注册走
`SceneRegistry::register_declarative*`,不替换已登记的 id;插件包里的那份 id 固定为
`plugin:<name>`。运行中的 daemon 经 `scene.reload` 重读 `~/.agix/scenes/`,经
`scene.register` / `scene.unregister` 登记、撤销一个不落盘的场景。格式与占位符见
`extension_points.md`。

### 3.8 并发:每个 agent 是一个任务

几个词在本文与代码里的含义:

| 词 | 含义 |
|---|---|
| **Agent** | 执行实体:自己的上下文与步进循环(一轮之内「调模型 → 调工具」直到结束);会话的主 Agent 与常驻团队成员另有输入循环(`Agent::run`),一次性子 Agent 跑完一轮即止。都是线程池上的一个 tokio 任务 |
| **Task**(运行任务) | 在跑的工作的句柄:状态、输出、取消、收件箱。类型有 agent / shell / monitor / teammate / cron / code(`RunCode` 的程序)/ ext(`RunExt` 的命令、宿主调的插件函数);`TaskOutput` / `TaskWait` / `TaskStop` / `TaskMessage`、`session.tasks`、`task_*` 事件说的都是它 |
| **Todo** | 记账清单(`TodoWrite`),不执行任何东西 |
| **Event / Message** | Agent 之间、Agent 与宿主之间的全部交互:输入通道与收件箱进,`AgentEvent` 与 `session.event` 出 |

会话的主 Agent、前台与后台子 Agent、`TeamCreate` 成员、常驻团队成员,各自是多线程
tokio 运行时上的**一个任务**,带自己的循环与上下文。一个 Agent 一轮里并发的工具调用
是这个任务里交错执行的 future(`runtime::streaming`),不另起任务——它们共享这个
Agent 的上下文,本来就该在一处。

前台子 Agent 也是独立任务:`DelegateMode::Wait` 的分支 spawn 它并等结果
(`run_one_turn_detached`),所以同一轮的几个前台 `Agent` 调用、`TeamCreate` 同一阶段
的成员是真并行,一个子 Agent 的同步重活不会卡住兄弟和父 Agent。父 Agent 的调用被丢弃
时子任务被中止,不留无人观察的子 Agent;子任务 panic 变成父 Agent 的工具错误;取消照旧
走 `CancellationToken`(`StopsWith`,§3.4)。

**任务只经租约登记。** 委派出来的每个任务(前台、后台、宿主起的、常驻成员的每条消息)
都由一个 `base::context::TaskLease` 持有:创建即在父会话上登记,持有者用 `conclude(status,
notice)` 说它怎么结束的。租约被丢弃而没有 `conclude`——工作 panic 了、从队列里被丢掉、
持有者自己没了——任务记为被停止(`Cancelled`),带 `notifying` 的还会给父会话发一条停止通知;
已经被外部停掉的任务不重复通知。所以不会有任务停在 `running` 上无人收尾。agents 里不直接调
`register_task`、不持有 `TaskScope`(`DLG-7`)。

常驻成员的收件箱是 `MemberInbox`:排一条消息就是登记它的任务(租约随消息进队列),消息被拒
(成员已停)或成员停止时还留在队列里,租约丢弃,这条任务以 stopped 结束。成员被停下时,
正在跑的那条消息的任务也当场以 stopped 结束(同 `TaskStop` 停它),不等那一轮收尾;晚结束的
那一轮不再改它的状态;每条这样结束的消息,父会话照样收到一条 stopped 的通知。成员的循环一次处理一条消息,在同一个 Agent 上续跑——复用就是「常驻」。

**Agent 之间只经通道说话,不共享可变状态:**

| 方向 | 通道 | 入口 |
|---|---|---|
| 父 → 子 | 收件箱:`RunningTask::inbox`,即子 Agent 的 steering 队列,步骤边界读到 | `TaskMessage`、`session.messageTask` |
| 子 → 父 | 结束时 `post_notice` 一条 `<task-notification>`,父 Agent 下一个步骤边界或下一轮开头读到 | 自动;宿主另有 `task_started` / `task_finished` 事件 |
| 父等子 | `RunningTask::wait_ended` | `TaskWait`(any / all + 超时)、`TaskOutput` |
| 成员之间 | 团队 mailbox 与团队共享记忆(§8) | `SendMessage` / `ReadMail`、`TeamMemorySave` / `TeamMemoryRead` |

通知在任务结束**之前**投递,父 Agent 已经用 `TaskWait` / `TaskOutput` 读到结果的,
通知随之撤回,同一件事不说两遍;被 `TaskStop` 停掉的不发通知。

**写新的 Agent 级代码守四条:**

1. 长期运行的 Agent 级工作用 `tokio::spawn` 起自己的任务,不要在另一个 Agent 的任务里
   poll 它。持有 `JoinHandle` 的一方负责它的生命周期——丢弃即中止,或交给取消令牌。
2. 一个 Agent 的状态只由它自己的任务改。别人经收件箱、通知、事件跟它说话;锁只用于
   短暂的快照读写,不跨 `.await` 持有。
3. 超过约 1 ms 的同步工作(扫目录、解析大文件、全量正则)放 `spawn_blocking`。独占线程
   只留给需要线程亲和的东西(`notify` 的 watcher 那类)。
4. 资源上限都在一处可配:委派深度 `agents.max_depth`(§3.4);同时在跑的子 Agent
   `agents.max_concurrent`(默认 6,整棵会话树共用一份);`TeamCreate`
   每阶段 `team.stage_concurrency`;常驻成员 `team.max_members`(满了先回收最久
   空闲的,全在忙就拒绝新成员;0 即不要常驻成员),空闲超过 `team.member_idle_secs` 的成员自己停下。
   常驻成员的取消令牌是父会话结束令牌的子令牌:会话结束时,空闲的和正在跑一轮的成员都停下。

**名额只有一条规则:持名额者不等。** 规则是 `agents::delegate::SlotRule`:子 Agent 自己跑在
一个名额上,它再为自己的子 Agent 申请名额时,拿不到就被拒绝(`DelegateError::Limit`),不排队——
否则整棵树的名额可能全被这样的等待者占着。拒绝是一个模型看得见的工具错误,它可以等
(`TaskWait`)或自己做。根会话不占名额,它的申请可以排队。会话持不持名额在建它的 `Limits`
时按深度定下(深度大于 0 即持有),调用方改不了。整棵会话树共用一个名额池(`SubagentSlots`)。
各模式怎么取名额:
- `Wait` 和 `Background`、`Host`:调用方会问人(`AskRoute::Host`,模型的 `Agent` 调用、
  宿主的 `session.startAgent`)时立即取,到上限直接拒绝,根会话里也一样,让模型和宿主立刻
  知道;`Wait` 而 `AskRoute::Refuse` 的(`RunCode` 程序里的 `agent()`,以及程序里触发的 skill
  fork)走 `SlotRule::take`:根会话里排队等空位(`docs/ptc.md` §5)。
- `TeamMember` 的阶段成员走 `SlotRule::take`:lead 是根会话时排队等,lead 是子 Agent 时拿不到
  就记为失败,团队如实报出;每阶段另受 `team.stage_concurrency` 约束。
- 常驻成员只在处理一条消息的那一轮占名额,空闲时归还;按起它的会话的规则申请,根会话的成员
  排队等,子 Agent 的成员拿不到时这条消息的任务以失败结束。
- `Hook`(agent hook 起的子 Agent)不占名额:名额是模型的,模型占得满的守卫不是守卫;它们靠
  "hook 起的子 Agent 自己的事件不再起 agent hook"收住(`DelegateError::NotJudged`),不会
  一层套一层。

**为什么不是一个 Agent 一个 OS 线程。** 多线程运行时已经把任务分到所有核上,一个 Agent
绝大部分时间在等模型 API;专用线程意味着每个 Agent 一个运行时或一套同步桥,换来的只是
同步重活不互相阻塞——这一点 `spawn_blocking` 已经给了。

---

## 4. 历史:状态即日志

会话状态存在一份 append-only 的 JSONL 里,`history::transcript` 把它**投影**成模型看到的消息。

- **投影可换**(`TranscriptProjection`),挂在 store 上——日志和读它的规则必须一起走,
  否则 resume / fork / 搜索会看见三个不同的过去
- `LogEntry::Extension { ns, event, payload }` 让内核之外的东西(插件、脚本、宿主)
  把状态放进同一条时间线。**未知 `ns` 必须可跳过**:卸载插件后旧日志仍能加载。
  引擎和 daemon 自己也用这条通道,`agix.` 开头的命名空间是会话日志格式的一部分。除
  `agix.context` 逐条生效、改变模型看到的内容外,其余都是"最后一条为准"的快照:

  | `ns` / `event` | 谁写 | `payload` | 读回 |
  |---|---|---|---|
  | `agix.session` / `spec` | 会话池,规格变化时 | `{options?, model?, provider?, prompt?, effort?, thinking_mode?, skill_dirs?, permission_mode?}` | 重建、恢复时照它建引擎(§3.2.2) |
  | `agix.goal` / `change` | 引擎与会话池,目标变化时 | `{goal}`,清除后为 `null` | 恢复时取回目标,不武装(§3.4) |
  | `agix.session_state` / `change` | 引擎,一轮结束和引擎停下时,有变化才写;宿主经 `session.setPermissionMode` 进出 `plan` 时,空闲的会话当即由引擎写,不在内存里的由会话池写 | `{plan: null \| {exit_to: <模式> \| null}, todos: [{content, status, active_form}]}` | `Agent::restore_state_from_log` 放回 `plan` 模式(`exit_to` 为 `null` 即退出到 `default`)和 todo 列表 |
  | `agix.spend` / `total` | 引擎,每一步之前、一轮结束和引擎停下时;会话池,子 Agent 与团队成员每调完一次模型和结束时;有变化才写 | `{session: <四字段用量>, goal?: {id, spent: <四字段用量>}}`:会话从创建起、目标从设下起的累计(`base::context::SpendRecord`) | `Agent::restore_state_from_log` 取各条的逐项最大值放回(目标的只取同一 `id` 的);回滚不作废它 |
  | `agix.context` / `drop`、`edit` | 会话池,宿主改上下文时 | `{at, text?, at_ms}` | 投影时套到消息上 |
  | `agix.checkpoint` / `pre`、`post` | 工具管线,文件工具写盘之前(已 sync)与之后;写在**根会话**里,子 Agent 的也是 | `pre`:`{id, files:[{path:{rel\|abs}, before:{state, sha256?, bytes?}}], tool, tool_use_id, rewind_id?, agent?}`;`post`:`{pre, ok, after:[{state, sha256?}]}` | `session.rewind` 算恢复什么、`session.checkpoints` 列每轮写了什么 |
  | `agix.rewind` / `begin`、`commit`、`abort` | 引擎,宿主回滚时 | `{rewind_id, cut_after, conversation, …}` | `commit` 且 `conversation` 时作废一段对话(见下) |

  `agix.spend` 有两个写者(引擎与会话池),各写各看到的累计;累计只增不减,所以读回取逐项最大值,
  谁后落盘都一样。`session.fork` 照抄分叉点之前的这几条,分叉出来的会话从源会话那时已花的接着算。
  `agix.session_state` 只记会话自己定下、设置里推不出来的那部分(`base::context::DurableState`)。
  不在 `plan` 里的会话,权限模式由规格(`session.setPermissionMode` 设的,否则 `options` 的)按恢复时的
  设置夹紧决定,不由这条决定。`exit_to` 也不照单全收:这条可能来自更宽的设置,或经 `session.import` /
  `session.fork` 从别处来,引擎挂上会话状态时(`SessionState::attach`)与读回这条时都把它按引擎设置的
  `session_state::ModeCeiling` 夹紧,同 `options.settings.permissions.mode`;只能由程序设的模式(`bubble`)除非就是
  引擎自己的模式,否则换成引擎的模式。
  `ExtensionsAreVisible::all()` 不分 `ns` 地把扩展条目渲染给模型,这些也在其内——
  要让模型读到某个扩展的条目,按 `ns` 列名单
- **模型可见即可从日志重建**。一个靠询问活着的插件来渲染条目的投影,会产出一个
  重新打开就不一样的会话。`model_visible_content_is_reconstructible` 把这条变成可检查的
- 大内容可以外置到 `BlobStore`,日志里只留引用;**引用解析不了时条目保持惰性,不是报错**。
  store 要由嵌入方自己装(`JsonlHistoryStore::with_blob_store` / `with_paste_store`);
  daemon 默认不装,大内容照样写在日志行里

**文件 checkpoint 与回滚。** 统一工具管线在 `authorize` 之后、`call_tool` 之前,凭工具声明的
`Subject::WritePath`(`Edit(...)` 规则认写的同一份声明)认出这次调用要写哪个文件,先把它此刻的
字节按完整 sha256 存进根会话 sidecar 的 `checkpoints/objects/`(`HistoryStore::checkpoint_objects`),
再追加并 sync 一条 `pre`,然后才放行;写完记 `post`(只记哈希)。记不下来就不执行这次调用。
`Bash` 与 MCP 工具不声明写路径,不在其中——命令跑之前不知道它会写什么。回滚由宿主经
`session.rewind` 发起、在引擎里执行:文件按**物理时间线**恢复(切点之后每个路径最早一次写的
写前内容,不论那次写所在的轮次是否已被作废),恢复本身也记 `pre` / `post`,所以回滚可以再被
回滚;对话不截日志,追加一条 `agix.rewind/commit`,`history::transcript::voided` 把
`(cut_after, commit)` 之间的可作废行(消息、压缩、`agix.context`、`agix.session_state`)标出来,
**所有投影入口先过它**,而不是放进可替换的 `TranscriptProjection::apply`——自定义投影漏不掉,
resume、轮数、durable state 的恢复与回滚后当场的视图是同一个函数算的。目标、宿主与插件的
扩展条目不作废。日志仍然 append-only。

**一个会话连同它的侧链可以在同一时刻截下来**(`HistoryStore::snapshot_lineage`)。
`JsonlHistoryStore` 不持任何锁:先量一遍父会话项目目录里每份日志的长度,再各自读到量下的
位置,读的时候截到最后一个整行——append 一次写完整的一行,量到的位置若落在写了一半的行
里,那一行不算。一个 append 慢的会话不会挡住另一个会话的快照。走的是 `Meta` 里的父子关系,只沿 `session_kind: sidechain`
往下走,分叉只记 id。默认实现一个接一个地 `load`,不是同一时刻,自己实现 store 的要覆盖它。
`session.export` 就站在这上面:快照加上图片本体、两种 RPC 视图和一份带版本的清单写成
一个归档;`session.import` 读同一份布局,用 `HistoryStore::append_enveloped` 把转录连同行 id、
时间戳与标签原样写成新会话(`keep_ids` 沿用归档里的会话 id 时,先以 `create_new` 原子占位、占不到就报冲突,并发的两次导入撞同一个 id 只有一个成功),压缩行对前面行 id 的引用才不会断(`daemon_rpc_protocol.md` 的
`session.export`、`session.import`)。包装 store 的类型要转发它,默认实现拒绝而不是退成 `append_tagged`。

**留存的时间戳与标识符走 `Environment` 契约**,所以在 `FixedEnvironment` 下同一段对话
能重放出逐位相同的日志。daemon 不用它:会话的时钟是 `ShiftedEnvironment`(从
`clock.start` 或录像开录的时刻起按墙上时钟走),标识符每次新生成,所以 daemon 的回放
得到的是内容一致、时间戳与 id 不同的日志(`daemon_rpc_protocol.md` 的 `options.recorder`)。
纯测量用的 `Instant` 不走契约——那正是它该做的事。

---

## 5. 执行层:工具怎么碰到机器

四个契约一组设计,因为它们互相咬合:沙箱约束进程,进程要文件与网络,
网络策略又要能作用于沙箱里的进程。

| 契约 | 管什么 |
|---|---|
| `Process` | 程序在哪里跑。**输出是流式的**——长命令的输出要边跑边到用户眼前 |
| `FileSystem` | 工具的文件在哪里。整值读写,因为所有调用点都是整值的 |
| `Network` | 每一次出网请求,以及它能不能出去 |
| `Sandbox` | 进程**怎么**被约束——永远不决定**要不要**约束 |

两个 provider:`platform::local_providers()`(本机,生产里用的)与 `ExecProviders::in_process()`
(内存文件树、预置的命令应答、离线网络、诚实地报告自己什么都不约束的沙箱)。

**一个会话一套。** `Builder::build()` 造一次,工具调用、引擎自己的 git 都用它,并经
引擎挂到会话状态上的 `EngineAttachment`(`SessionState::attach`)交给权限判断:权限层判路径(`canonicalize`)、判沙箱是否完整兜得住
命令时,问的是这次调用真正要跑在上面的那套,子 Agent 的调用按子 Agent 会话的那套判。

五条容易搞错的:

1. **路径安全在契约之上,符号链接解析在契约之内。** 一个路径能不能写是策略,
   provider 不该能取消它;但远端的符号链接图是远端的。顺序是:
   `provider.canonicalize` → `check_write(已解析的路径)` → `provider.write`。
2. **出网策略问的是"模型能够到哪里"**,不是"这个进程能连出去哪里"。请求带着
   `Origin`:模型选的目标受 `web.allowed_domains` 约束,运维配的端点(模型 API、MCP、遥测)不受。
   把它作用到模型 API 上会让 agent 连自己的大脑都够不着。模型选的目标还不许落到本机或
   内网地址(`is_internal_address`:回环、私网、链路本地、CGNAT、`0.0.0.0`、带着这些 IPv4
   的 IPv6……),除非 `web.allowed_domains` 点名了那个主机;HTTP hook 的 url 带
   `HttpRequest::public_only`,同样不许。重定向的每一跳都查;名字在发出前解析查一次,
   连接时用的解析器再查一次,中间换了答案的名字也进不去。

   **引擎自己的 HTTP 走不走系统代理,与沙箱内命令的出网是两回事。** 模型调用、`WebFetch` / `Ping`、
   MCP over HTTP、OAuth 经 `platform::LocalNetwork` 发出,缺省遵循环境里的 `HTTP_PROXY` / `HTTPS_PROXY` /
   `ALL_PROXY` / `NO_PROXY`(回环地址总是直连);`app.system_proxy: false` 让它们全部直连,
   `public_only` 解析器对代理主机的豁免随之取消。沙箱里命令的出网由 `sandbox.network.mode` /
   `sandbox.network.proxy` 管,不受它影响。HTTP 客户端是进程级的,所以这项只认全局与托管层(scene、项目层写了
   记 warn 并丢弃),值在建池时(`assemble::pool`)交给 `platform::configure_system_proxy`,
   之后的改动要重启 daemon 才生效,`config.reload` 把它列在 `restart_required`。`daemon.doctor` 的 `network.proxy`
   报当前姿态。
3. **受约束的策略绝不静默降级为无约束执行。** 后端报告 `Full` / `Partial` / `None`
   并列出兑现不了的部分;`sandbox.require` 决定不足 `Full` 时是否拒绝。
4. **一个命令是它起的整个进程组。** 本机 provider 让每个子进程领一个会话(也就领了一个
   进程组,且没有控制终端——命令没法经 `/dev/tty` 用 `TIOCSTI` 往 daemon 的终端里塞输入,
   等沙箱外的 shell 去执行),停的时候整组停;前台进程退出后输出最多再等 `OUTPUT_GRACE_AFTER_EXIT`,还被后台进程占着就
   连它们一起停。Bash、Monitor 与 command hook 经会话的 `exec.process` 起(hook 的进程因此落在会话选的执行层和进程组台账上),引擎自己跑的 git 与 `Read`
   调的 `pdftotext` 经会话的 `exec.process` 起(`base::engine_exec`),都是这条规矩;MCP
   stdio server 经 process-wrap 起,同样领一个会话、停的时候整组停。
5. **子进程的环境只在一处决定。** `base::interface::exec::SubprocessEnv`(策略与判定在 `base`,写进命令的 `platform::apply_subprocess_env` 在 `platform`):缺省
   继承宿主环境但去掉引擎自己的凭据,`allowlist` 模式只留 `PATH` / `HOME` / 语言环境等几项
   加上 `allow` 列出的。Bash、Monitor、command hook、MCP stdio 服务、引擎自己跑的 git 与
   `Read` 调的 `pdftotext` 都按它起——引擎的 git 与 `pdftotext` 经会话的 `exec.process` 起,
   与会话的命令同一份 `sandbox.env`,其余也按它取策略——MCP 服务取的是声明它的
   那份设置(daemon 按项目、scene 建连),再叠各服务自己的 `env`。这和沙箱无关——沙箱关着,模型一句 `printenv` 读到什么也由它决定。

   **引擎自己的凭据**是固定的 `ANTHROPIC_*`、`AGIX_DAEMON_TOKEN`,加上配置里点名"密钥在这个
   环境变量里"的每一个变量:

   | 谁点名 | 哪些变量 | 在哪里记下 |
   |---|---|---|
   | 任何一层设置的 `providers` | `api_key_env` | `LoadedSettings::load` |
   | 全局层、场景层设置的 `mcp.servers` | `oauth.client_secret_env`、`oauth.private_key_jwt.key_env`,以及 `command` / `args` / `env` / `url` / `headers` 里的每个 `$VAR` | `LoadedSettings::load` |
   | 插件包的 `[[mcp]]` | 同上 | 插件启用时(`active_plugins`) |
   | 任何连上的 MCP server(经 `mcp.addServer`、嵌入方直接交给 `McpManager` 的也算) | 同上 | 连接之前(`spawn_service`) |
   | MCP 策略文件的 `enterprise_idp`、`Assembly::mcp.enterprise_idp` | `client_secret_env` | 装配时 |
   | 嵌入方的 `EnvCredentials` | 它读的每个名字 | 构造时 |
   | 带 `otel` feature 的构建 | `OTEL_EXPORTER_OTLP_HEADERS` 与按信号分的三个 `*_HEADERS` | 建池时 |

   项目自己那一层的 MCP server 不算:它们读不到 daemon 的环境(§6.4),写的变量名不是 daemon
   的密钥。记下的名字放在 `env.rs` 的进程级集合里(子进程继承的是进程的环境,所有池共用一份),
   由 `hold_back_credential` 加入、`is_engine_credential` 查询;都发生在按这份配置起任何子进程
   之前。集合只增不减:设置重载时新点名的变量随之加入,不再点名的仍然剔除——密钥还在进程的
   环境里,一份删掉了 `oauth` 块的设置不该让 `printenv` 重新读得到它。`PATH`、`HOME` 这类
   `allowlist` 缺省保留的变量不论谁点名都不剔除。确实要交给子进程的写进
   `sandbox.env.allow`,它对这份设置起的所有子进程生效。

   MCP stdio server 也按这条起:它显式的 `env` 照给(`"env": {"GITHUB_TOKEN": "$GITHUB_TOKEN"}`
   给它的就是这个值),继承来的环境里没有别的 server、provider 的密钥。一个 server 要用的密钥
   写进它自己的 `env`,不要靠继承。

**沙箱里的命令出网只经宿主指定的代理(`sandbox.network.mode: proxy`)。** 按域名过滤、凭据掩码、TLS
终止都是下游代理的事,内核只守一条两个后端都兑现得了的边界:命令只能连到 `sandbox.network.proxy`
的端口,并拿到指向它的 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`(大小写各一份,由
`PlatformSandbox::confine` 加在命令自己的 `ProcessSpec.env` 之后,所以盖过继承来的同名变量,也不受
`sandbox.env` 的 `allowlist` 剔除)。变量只是方便,`unset` 掉直连照样连不出去。
macOS 是 `(deny network*)` 后只放开 `localhost:<代理端口>`(覆盖 `127.0.0.1` 与 `::1`);代理不在
回环上时 profile 只能写 `*:<端口>`,报 `Partial`。Linux 是 `--unshare-net`,再把一个 unix socket
绑进命令的 `/tmp`:socket 由 daemon 进程里的中继(`platform::sandbox::egress::relay`,按代理地址
惰性起一个、随进程存活)转发到代理,命名空间里用 `socat` 把它还原成 `127.0.0.1:<代理端口>`;
系统目录里没有 `socat`、中继起不来时照 `deny_all` 断网,报 `Partial`。代理端口是 daemon 自己的端口
时两边都拒(中继每个连接都再查一次围栏表)。没配 `sandbox.network.proxy` 时断网,`unmet` 写明;配了而模式
不是 `proxy` 时地址被忽略。项目层设置不能写 `sandbox.network.proxy`,`sandbox.network.mode` 只能收紧到
`deny_all`(`proxy` 与 `allowlist` 不能互换)。**Proxy 模式只约束沙箱内命令的出网**;
引擎自己的 `WebFetch` / `Ping` 走引擎网络,不经代理,也不受 `web.allowed_domains` 约束(与 `unrestricted`
下同,名单只表示"点了名的主机可以是内网地址")——域名过滤由宿主代理负责,而宿主代理管不到引擎自己的
出网;引擎网络不经宿主代理,不计划改。`daemon.doctor` 的 `sandbox.network.proxy`
报沙箱兑现了多少。

**沙箱配置只有三份类型,各管一段。** `settings::SandboxConfig` 是设置文件里 `sandbox` 一节的形状(含
`deny_read` 的"没写"与"写了空表"两种);`base::interface::exec::SandboxSettings` 是装配后会话持有的
那份(缺省已填);`SandboxPolicy` 是某一条命令此刻要约束的内容,由前者按这条命令的权限结论与会话约束
推导。后端只认 `SandboxPolicy`,并报告兑现了多少(`Enforcement`)。

沙箱是**轻量限制,不是安全隔离**。已知逃逸路径写在
`crates/platform/src/sandbox/mod.rs` 的模块头里,七条;第 3 条对引擎自己的
git 已经收口(见 §6.3),剩下的是用户事后在项目里自己跑的 git;第 4 条(让别的进程替命令
在沙箱外做事)在无人值守会话里收口;第 5 条(命令在项目里埋一个嵌套仓库)由命令结束后的检查
隔离,后端本身挡不住(都见 §6.3);第 7 条(macOS 上命令把可写目录里被拒读的文件改名再读)Linux
没有。它限住的写入与读取范围见 §6.3。

---

## 6. 权限

### 6.1 规则怎么裁决

权限分两半,会话以 `PermissionSet { policy, grants }` 持有:`PermissionPolicy::decide` 裁决一次调用,
`PermissionGrants` 记下会话里给出的放行(`Turn` / `Session` / `Project` 三档;`Project` 写项目的
`settings.local.json`)。内建的 `permissions::policy::Policy` 两个都实现,宿主只给策略时配
`without_grants`(此时询问里不提供 `permit_always`)。**每次工具调用只在 `Policy` 里判一次**,
没有第二套判断:工具自己不裁决,只陈述事实(下面"工具陈述事实"),`Policy` 拿事实、规则与模式
得出结论;沙箱怎么关由结论带出的 profile 决定(§6.3),不由工具临时去探。

**runtime 一侧只有一条流程**(`runtime/src/agent/turn/tool_exec.rs`、`tool_permission.rs`):
`PreToolUse` hook(可改写输入)→ `validate_input`(只此一次)→ 用被判会话的上下文造出这次调用真正
要用的 `ToolContext` → `ToolFacts::gather` → `Policy::decide` → 结论是 `Prompt` 就等回答(全仓
只有这一个等待循环,子 Agent 也走这里)→ 记遥测(记的是最终结论)→ 把结论里的
`approval { basis, profile }` 放进 `ctx.approval` 交给 `call` → `call` 返回后按 `obligations`
收尾(§6.3 的嵌套仓库检查)→ 按 `relays` 与 `Reach` 给外部内容加围栏(`relays` 有值的、`Server`、
`Host`、`Outside`;不认识的 reach 也加)。`PreToolUse` hook 回的 `additional_context` 在这个 `Policy`
结论放行之后、调用执行之前才送:被 hook 拦下、被规则拒绝、询问被拒的调用收不到,hook 不能靠一条
被拒的调用往对话里塞内容。`prompt` / `agent` 型 hook 的 `approve` 与 `additional_context` 都要写
`may_approve: true` 才算:判断用的模型读的是不受信的 payload,它说"不行"才可信,说"可以"不算;
`command` / `http` 是运维写的配置,不受这条限制。

**工具陈述事实。** `Tool` 的权限面只有 `permission_facts`,缺省 `PermissionFacts::unknown()`:
什么都不声称,于是要问。事实有四项:`subject`(规则拿什么匹配:路径、命令行、URL 或不透明串)、
`reach`(这次调用够多远)、`runs`(是不是一条在会话沙箱里跑的命令,前台还是后台)、`question`
(要问人时的措辞)。`reach` 的取值:

| `Reach` | 含义 | 谁这么说 |
|---|---|---|
| `SessionOnly` | 只动会话自己的状态:待办、任务、信箱、计划、目标 | `Sleep`、`Task*`、目标类工具、`ExitPlanMode`、`AskUserQuestion`、`ToolSearch` 等 |
| `Delegates` | 起别的调用,每个子调用各自再判 | `Agent`、`Skill`、`RunCode`、只读调用的 `RunExt` |
| `Operator` | 只到运维配置的端点 | `WebSearch` |
| `Inside` | 落在会话免询问的根之内(读按可读根,写按可写根) | 根内的 `Read` / `Grep` / `Glob` / `Write` / `Edit` / `NotebookEdit`;只读通道里的 `Bash`、根内只读的 `Monitor` |
| `Outside` | 根之外的路径 | 根外的文件工具 |
| `EditsProject` | 跑模型没写的代码,它至多改项目之内的文件:除非模式接受编辑,否则询问 | 会写的 `RunExt` 命令 |
| `Host { listed }` | 模型选的主机;`listed` 表示 `web.allowed_domains` 点了名 | `WebFetch`、`Ping` |
| `Server` | MCP server 的工具;它对自己的说法(`readOnlyHint`)不算数 | MCP 适配器 |
| `Persists(..)` | 留下一个之后会自己动作的东西 | durable `CronCreate` |
| `Refused(..)` | 工具自己知道这个输入不该跑 | 文件工具的路径安全检查(§6.2) |
| `Unknown` | 不知道 | 缺省;其余的 `Bash` / `Monitor` 命令 |

`relays`(`Relays`)说的是结果里的内容从哪来,与 `reach` 无关:请求只到运维配置的端点、交回的却是
别人写的东西时写它——`WebSearch` 是 `Web`,`ReadMcpResource` 是 `McpServer(..)`。它只决定围栏,
不进权限判定。

`spec().stance`(`ModeStance`)是与输入无关的另一项:`Coordination` 是不碰文件、shell、网络的协调
原语,自含的调用不受 `plan` 与 `dontAsk` 拦;`LeavesPlan` 是退出计划模式的那一次调用。

**判定顺序。** `Policy::decide` 按下表走,分判定与应答两段:S0–S14 得出临时结论,S15–S18 只处理
"要问"。S 的编号与 `policy/judge.rs`、`policy/answer.rs` 注释里的一致,表按执行顺序排,所以 S7
排在 S4 / S5 之前。判定段在 `crates/permissions/src/policy/judge.rs`(入口 `decide` 在 `policy/mod.rs`),
应答段在 `policy/answer.rs`,子 Agent 带来的规则在 `policy/delegated.rs`,宿主策略在 `policy/host.rs`。

| 阶段 | 检查什么 | 结论 |
|---|---|---|
| S0 | managed 层读不了 | `Deny(ManagedUnreadable)`,所有调用,任何模式 |
| S1 | 注册表里没有这个名字 | 先过 delegate 的类型级 deny 与宿主的 deny(命中即 `Deny`);否则 `Ask(UnknownTool)`,hook 的 approve 答得了;不再走 S2–S14 |
| S2 | 凭据:调用里任何一个字符串(路径及其解析后的写法、shell 的词与赋值、`--flag=value` 的值)落在 `SensitivePaths` 里 | `Deny(Credentials)`,**任何模式**,bypass 也一样 |
| S3 | `PreToolUse` hook 拦截 | `Deny(Hook)` |
| S3b | delegate 的类型级 deny 规则命中(程序名读不出来的 shell 段同样拒) | `Deny(DelegateRule)`,任何模式 |
| S7 | 命令(`runs`)在沙箱没关、`sandbox.require` 打开时,会话沙箱兜不到 `Full` | `Deny(Confinement)`;**问人之前就拒**,bypass 也拦 |
| S4 | 归一模式:读会话的实时模式;委派取 `PermissionMode::for_delegate`;managed 关了 bypass 就当 `default` | 不产出结论 |
| S5 | `bypassPermissions`:只看 managed 层与宿主的 deny | managed deny → `Deny(ManagedRule)`;宿主 deny → `Deny`;managed 的 ask(或程序名读不出的 shell 段)→ `Ask(ManagedRule, PersonOnly)`;其余 `Permit(Bypass)`,跳到 S18 |
| S6 | `reach` 是 `Refused` | `Deny(ToolRefusal)` |
| S8 | 规则:各层(managed、用户、项目、local、会话授权)加宿主层,deny > ask > allow;程序名读不出的 shell 段按 ask;都没命中再查"永久允许"记下的原样内容 | 命中 deny → `Deny(Rule / ManagedRule)`;ask 记下待 S11(同一次调用 managed 也 ask 时标成 managed 的,`PersonOnly`);allow 记下待 S13 |
| S9 | 免于 bypass 的路径(`/etc/passwd`、`rm -rf /` 一类) | `Deny(BypassImmune)` |
| S10 | 模式硬拒:`plan` 拒非只读(`LeavesPlan` 除外);`dontAsk` 拒没有 allow 命中的;`Coordination` 且自含的调用两样都免 | `Deny(PlanMode / DontAsk)` |
| S11 | S8 记下的 ask | `dontAsk` 下 `Deny`;否则 `Ask(Rule / ManagedRule)` |
| S12 | 自含快速通道:`reach` 属于 `SessionOnly`、`Delegates`、`Operator`、`Inside`,且不是后台命令,不是"非只读并且指向引擎配置目录",也不是"strict profile 兜不住而宿主要求兜住"(`strict_unheld`) | `Permit(Contained)` |
| S13 | S8 记下的 allow | `Permit(Rule)` |
| S14 | 模式分派 | `acceptEdits` 且只读、非 `strict_unheld`、`reach` 属于 `SessionOnly`、`Inside`、`Host { listed: true }` → `Permit(AcceptEdits)`;`bubble` → `Ask`;其余 → `Ask(ModeDefault)`,写引擎配置的标 `Ask(Configuration)`,措辞取 `facts.question` |
| S14b | delegate 自己的 `allow`(fork 的 skill 的 `allowed-tools`、它自己跑的 skill)盖住了 S14 的询问 | `Permit(Rule)`;只覆盖 `ModeDefault` / `Configuration` 的询问,deny、规则 ask、managed 的问都不受它影响 |
| S15 | 无人值守的受限命令:`Ask(ModeDefault)`、无人值守、`runs`、`reach` 是 `Unknown` 或 `Inside`、会话沙箱 `Full`,并且(没开 `sandbox.unattended_needs_network_policy` 或网络受限) | `Permit(UnattendedConfined)` |
| S16 | hook 的 approve | 对 `HookOrPerson` 的询问 `Permit(HookApproved)`;managed 的问(`PersonOnly`)不吃它 |
| S17 | 在场性 | 无人值守 → `Deny(Attendance)`,附"授权走设置"的提示;没有任何渠道能问人(`NoOneToAsk`:宿主没给权限也没接问人的通道、程序起的子 Agent、skill 的内联命令、`RunCode` 的子调用)→ `Deny(Attendance)`,附原因;有人值守 → `Prompt` |
| S18 | 输出 | 每个 `Permit` 带 `basis`、`profile`、`obligations`(见下) |

顺序上的几条理由,不能随手调换:

- **hook 改写在先,权限判改写之后的输入。** hook 的 approve(S16)在在场性(S17)之前:它是预先给的回答,不需要有人在场。
- **凭据在 bypass 之前**(S2 先于 S5);**沙箱兜不住的拒绝在 bypass 与问人之前**(S7),
  免得人批准之后命令才被拒。
- **自含快速通道在 deny、免于 bypass 的路径、模式硬拒、规则 ask 之后**(S12 在 S8–S11 之后),否则
  这些检查碰不到 `Write` 和只读命令;写引擎配置的调用不走它,但挡不住用户自己写的 allow 规则(S13)。
- **无人值守的沙箱放行只作用于 S14 产生的询问**(S15),所以规则 ask、配置目录确认、`dontAsk` 照样先起作用。
- **managed 的询问只有人能答。** managed 层的 ask 标 `Answerable::PersonOnly`,S16 不放行它。

每个结论带它所在的 `Stage`(拒绝与询问)或放行依据 `Basis`(`Rule`、`Contained`、`AcceptEdits`、
`Bypass`、`HookApproved`、`UnattendedConfined`,以及人答了"允许"的 `Person`)。

**放行带什么。** `Permit` 的 `profile`(`ExecProfile`)由依据、是不是命令、在不在场决定,只管命令
怎么被关(§6.3),从不决定能不能跑:

| 依据 | 有人值守 | 无人值守 |
|---|---|---|
| `Contained` / `AcceptEdits`,且是命令(只读通道里的 `Bash`、根内只读的 `Monitor`) | `Strict` | `Strict` |
| `UnattendedConfined` | | `Unattended` |
| `Rule` / `Person` / `HookApproved` / `Bypass`(以及不是命令的调用) | `Session` | `Unattended` |

`obligations.scan_nested_repos` 在 `runs` 且 profile 不是 `Strict` 时为真(§6.3)。`ConfinementReport`
(`ToolContext.confinement`:会话沙箱与 strict policy 各兜到什么程度、网络是否受限、
`sandbox.require`、`sandbox.unattended_needs_network_policy`)是会话建好时由执行层探一次的输入事实,
`Policy` 只读它,从不自己探沙箱。

**子 Agent 与宿主的策略。** 子 Agent 与团队成员的调用经 `DelegateRelay` 转给父会话的 `Policy`,请求里带
`Delegate { session, mode, rules }`:`rules` 是类型级 deny 加委派自己的授权。问人由子会话自己的 runtime 发起
(`ParentElicitation` 用父会话的事件通道),"永久允许"经 `ParentGrants` 记到父会话,`Turn` 档的授权留在
委派本地、它的回合结束就清掉。宿主给的 `Assembly.host_policy` 是 S8 里的一层规则:它的 deny 在任何
模式下都生效,它的 allow 只算一条 allow 规则,它的询问是一条 ask 规则。`Builder` 没给权限时用引擎自己的
`Policy::from_settings`,模式取自设置,缺省 `default`;同时没有问人的渠道时按 S17 拒绝。

**两份敏感路径名单。** 它们都登记在 `base::paths`:凭据在 `sensitive` 的 `CREDENTIALS` 表
(`Scope` × `Secret`),引擎配置在 `layout` 的 `ENGINE_CONFIG_*` 一组常量,配置根由会话的 `Layout` 给出(§8.1);`SensitivePaths` 按会话的状态目录
与项目解析一次,会话内共享。权限判定与文件工具(`tools::security::read_allowed`、`Grep` / `Glob`
的遍历、只读 `Bash` 的路径判断)问它(`is_credential`、`is_engine_settings_file` 等),`PromptFiles`
内部也走它,两个 OS 沙箱后端从它的 `credential_locations` 生成各自的规则,不各抄一份:

- **凭据**(`CREDENTIALS` 表):状态目录与各场景目录下的设置文件(`settings*.json`、
  `mcp.local*.json`,MCP headers 与 env 在里面)和 `mcp/oauth/`,状态目录下
  daemon 的 `daemon/auth/`,项目层目录(缺省 `.agix`;改了名时 `.agix` 也算)下的
  `settings.local.json` / `mcp.local.json`,以及
  任何位置的 `.ssh` / `.aws` / `.gnupg`。状态目录的这几项对本机别的实例的状态目录同样成立
  (外来状态根,§8.1)。调用里有一个字符串(路径工具的路径及其解析后的各种
  写法,shell 命令的每个词与赋值,`--flag=value` 的值)落在这里,`Policy` 就拒绝(S2),不问,
  `bypassPermissions` 下也一样;`Grep` / `Glob` 搜它上层的目录时跳过它;沙箱里的命令读不到其中
  引擎自己的那部分(状态目录与各场景目录下的设置文件、`mcp/oauth/`、`daemon/auth/`、项目的
  `.agix/*.local.json`,§6.3),`sandbox.allow_read` 放不开;家目录下的 `.ssh` / `.aws` / `.gnupg`
  在内建的读禁名单里(`sandbox.deny_read` 追加在它后面),沙箱挡,但可由 `allow_read` 放开;别处的这几个目录沙箱不挡,
  只由 `Policy` 挡住写出了路径的调用。没有一个回答能让"把 token 读进模型"成为好主意,所以不询问。
  引擎自己读进提示词或模型上下文的文件也一样:skill 的列表、正文与 `files:`,`AGENTS.md`
  及向上找到的各层,`prompt.instruction_files`,rules 的索引,agent 定义,output style,memory 的
  索引与条目,声明式 scene 的 `prompt.md`,`CLAUDE.md` 等的导入,host 传来的文件附件,都经
  `base::paths::PromptFiles` 读:先解析符号链接,链接本身的写法或它指向的地方落在凭据里就
  不读,记一条 warning。
- **项目内容不跟随指出项目的链接。** 写在项目根下的文件(`PromptFiles` 的 `root`)解析后
  必须仍在项目根下,否则同样不读、记 warning。仓库可以提交指向任何地方的符号链接,它能放到
  模型面前的只该是它自己的文件;凭据名单之外的 `~/.netrc`、别的项目的 `.env` 也靠这一条挡。
  项目内部的链接照常跟随(`CLAUDE.md -> AGENTS.md`、`.claude/skills -> ../.agents/skills`)。
  monorepo 子目录里起的会话,上层目录的 `AGENTS.md` 由向上查找按它自己的路径读到,不需要
  链接;子包里链向仓库别处的 skill / agent 不跟随——在仓库根起会话,或者装成用户层的 skill。
  memory 的条目只认解析后仍在各自 memory 目录里的文件,插件包里的文件只认解析后仍在包里的。
- **引擎配置**(`Layout::config_roots` 给出的配置根,`base::paths` 的 `ENGINE_CONFIG_{FILES,DIRS}` 与
  `PROJECT_ENGINE_CONFIG_DIRS`):
  项目层目录、`.agix`、`.claude` 下除本实例工作区之外的部分,和项目的 `.agents/skills/`(skill 的
  `allowed-tools` 会给自己授权)。写工具写这里要确认(工具自判的放行不算数),沙箱里只读
  (§6.2、§6.3)。读不受限。

项目根的 `AGENTS.md`(以及 `CLAUDE.md`)不在引擎配置里:它每会话注入提示词,但只是文字——
不授予权限、不跑命令、不接 hook 或 MCP,模型写进去的东西和写进 README、代码注释的一样,只能
影响模型自己读到什么;它在仓库里,改动看得见;让 agent 维护它是常见的请求(`init`、`import`
两个内建 skill 就写它)。保护它会给日常编辑加询问,挡不住同一类"持久的提示"(仓库里任何会被
读到的文件都是)。

规则这一步有五条约束:

1. **deny 永远优先。** 一次调用命中的规则里有一条 deny 就拒绝,否则有 ask 就问,否则
   才 allow(`bypassPermissions` 只看部署方 managed 层的规则,见本节末)。哪一层写的、写得多具体,都不参与裁决——项目仓库里提交的一条 allow 推不翻
   用户自己设置里的 deny,插件贡献的、客户端经 RPC 传进来的 allow 也一样。要"大范围
   禁止、小范围例外",写法是 `dontAsk` 模式加只列允许范围的 allow,而不是一条宽 deny
   加一条窄 allow。
2. **各层的规则并存。** 全局、scene、项目三层(以及各自的 `settings.local.json`)、`cli` 层
   (`agixd --set`,§3.2.1)、会话的 `options.settings` 与部署方的 managed 层的 `permissions` 规则列表拼在一起生效,不像普通
   设置项那样后层盖前层。每条规则记着自己来自哪一层——全局与 scene 层是 `UserSettings`,项目层是
   `ProjectSettings`,local 是 `LocalSettings`,`cli` 层是 `CliArg`,会话的是 `Session`,managed 层是 `PolicySettings`——只用来报出命中的是哪一条
   (几条同样具体时先报 managed 层的)。
3. **路径先规整再匹配。** 读写路径的工具,路径按会话 cwd 解析成绝对路径并去掉 `.` /
   `..`,`~` 在规则与路径两边都展开;符号链接解析后的路径算作别名——按原样写法解析的
   也算,符号链接后面的 `..` 是从链接目标往上走的,`link/../x` 落在哪里,deny 就在哪里
   命中;指向尚不存在之处的链接(仓库可以提交 `notes.md -> ~/.zshenv`)按它指向的地方算,
   不按链接所在的位置算;引擎 worktree 里的路径在主检出里的那一份也算别名(§3.4)。别名上命中 deny / ask 照样生效,
   allow 只认工具真正打开的那个路径。glob 按 gitignore 约定:`*` 不跨
   `/`,`**` 跨。`Read(...)` 规则同样作用于 `Grep` / `Glob`,`Edit(...)` 规则同样作用
   于 `Write` / `NotebookEdit`。
   **大小写跟着卷走。** 路径所在的卷不分大小写时(macOS 缺省的 APFS,按路径存在的最深一级问
   `pathconf(_PC_CASE_SENSITIVE)`,`permissions::path_rule::is_case_insensitive`),规则与路径两边都按
   `base::paths::fold_case` 折叠再比:`Edit(secret/**)` 盖得住磁盘上的 `Secret/`,也盖得住
   模型写的 `SECRET/x`。这对 deny、ask、allow 一样——那里 `SRC/x` 就是 `src/x`,是同一个文件。
   探测只在 macOS 上做(其它平台一律当作区分大小写),区分大小写的卷(区分大小写的 APFS)照原样比,
   allow 不会因此变宽。
   插件命令的 `writes` 与 skill 的路径收窄不折叠:只会更严。
4. **shell 命令逐段匹配。** `Bash(...)` 规则对命令行里的每一条简单命令分别匹配:认
   引号,切 `&&` `||` `;` `|` `&` 与换行,`$(...)`、`<(...)`、反引号里的命令单独算一段。
   任一段命中 deny 即拒;allow 要每一段都被放行,或者整行原样等于一条不带通配的 allow
   ("永久允许"记下的就是那一整行)。deny / ask 另按每段**实际启动的程序**再比一遍
   (`permissions::invocation`):去掉引号与转义,展开花括号,丢掉重定向,程序路径取
   名字(`/bin/rm` 是 `rm`),剥掉 `env` / `command` / `exec` / `nice` / `time` /
   `sudo` / `timeout` / `xargs` / `busybox` / `flock` / `ionice` / `pkexec` 等 wrapper 连同
   它们的选项,`sh -c`、`eval`、`find -exec`、`su -c`、`runuser -- `、`watch`、`script -c`、
   `parallel` 里的命令也算。程序名从
   文本里读不出来的段(`$CMD x`、`r?`、看不懂的 wrapper 选项、`case`/`function` 块、读 stdin 的
   裸 `sh`)在该工具有带内容的 deny / ask 规则时按 ask 处理(`dontAsk` 下拒绝),永远不会因此
   被放行。allow 只认原文——规整只让裁决更严。这些 `Bash(...)` 的 deny / ask 同样管住别的
   跑 shell 的工具(`Monitor`),allow 不跨工具。但 shell 上的 deny 规则不是沙箱
   (`python3 -c 'os.remove(...)'` 里没有 `rm`),边界是 OS 沙箱。点名具体路径的 `Read(...)`
   deny 同时进沙箱(§6.3),带通配的不进。
   只读命令(`Bash` 免询问的那一类)用同一套调用解析判:行里启动的每个程序都要按名字(不带
   路径——`./ls` 跑的是那个文件)在只读名单里,读不出程序的段不算只读;包在外面的 wrapper
   只能是不改变跑什么、在哪跑的那几个(`nice`、`timeout`、`nohup`、`stdbuf`、`command`、
   `builtin`、`exec`、不写报告文件的 `time`、只清除或传递变量的 `env`),`xargs`、`sudo`、
   `flock`、`env -C` / `-P` 都不算。只读名单只收"不管带什么参数都不会写文件、不会执行别的
   程序"的命令:`rg --pre`、`fd -x`、`git grep -O`、`man -P`、`sort --compress-program`、
   `cargo build/check`(跑 build.rs)这类会拉起程序的,`sort -o`、`uniq IN OUT`、`yq -i`、
   `git config k v`、`git branch -D`、`printf -v` 这类会写文件 / 改仓库 / 设变量的,写文件的
   重定向(含 `>&file`),以及设 `PATH` / `LD_PRELOAD` / `CDPATH`、任何 `*PAGER` /
   `*BROWSER` / `*EDITOR`、`GIT_*`、`SSH_*`(`SSH_ASKPASS`)、`HOMEBREW_*`、`XDG_*`、
   `*_CONFIG_PATH` 等变量的(`permissions::shell::is_dangerous_variable`),都不免询问;长选项按 GNU 与 git 的缩写规则认
   (`sort --outp=f` 就是 `--output`)。项目自己的文件能把程序换掉的也不在名单里:
   `cargo metadata` / `cargo tree` 要问 rustc(`.cargo/config.toml` 的
   `build.rustc-wrapper`),`cargo` / `rustc --version` 与 `rustup show` 跑
   `rust-toolchain.toml` 指定的工具链(可以是一个路径);`node` / `npm` / `pip` 连
   `--version`、`list` 也不收——装了 asdf / nvm / volta / pyenv 时它们是按项目里的
   `.tool-versions` / `.nvmrc` / `.python-version` 选二进制的 shim。`brew` 整个不收(按
   `HOMEBREW_*` 拉起浏览器、编辑器)。`git` 只以 `git <子命令>` 的形式、
   只在会话自己所在的仓库里只读:`cd` 进项目带来的裸仓库(`fixtures/x.git`)或嵌套的 `.git`
   再跑,那个仓库的配置(`core.fsmonitor`、`diff.external`)由项目作者写,git 会照着拉起
   程序。会向参数里的主机发请求的 `dig`、`ping`、`nslookup`、`traceroute`、`npm view`
   (`--registry`)、`git remote show <URL>` 也不在名单里(主机名本身就能经 DNS 带出数据);
   `git remote` 只认不联网的光秃秃、`-v`、`get-url`。
   只读命令的自动放行还只限路径参数都落在 cwd 与附加目录之内的;指向外面的,和 `Read`
   读项目外的文件一样交给模式分派。参数里有 shell 还要展开的东西(`$VAR`、`~`、替换)
   的,`cd` / `pushd` 到不是项目内字面路径的地方(或 `cd -` / `popd` / `pushd +N` 落到
   命令没写出的目录)之后再按相对路径读的,都不算在内;glob 按它此刻在磁盘上匹配到的
   路径查(`.*/x` 在 macOS 的 bash 3.2 下能匹配到 `../x`)。
5. **URL 拆开再匹配。** `WebFetch(...)` / `Ping(...)` 的规则内容是 `scheme://host[:port][/path]`
   形状的 URL 模式(`base::url_rule`)。两侧都规整成请求真正发出去的样子:调用的 URL 去掉
   userinfo、query、fragment,主机名小写(Unicode 主机是它的 punycode)、去掉末尾的 `.`、默认端口
   省略,`ws` / `wss` 当作 `http` / `https`;模式的主机逐个标签过同一套 IDNA,`https://bücher.example`
   就是 `https://xn--bcher-kva.example`;两侧路径里非保留字符(字母、数字、`-` `.` `_` `~`)的百分号
   转义解码、其余转义统一成大写——服务器把 `/%61dmin` 读成 `/admin`,`%2F` 却不是 `/`。然后与模式的
   scheme、主机、端口、路径**分别**匹配:主机里的通配只跨 `.`,到不了端口和路径,所以
   `https://*.example.com/**` 不被 `https://evil.net/.example.com/` 骗过;不写端口只认缺省端口,
   `:*` 认任何端口;不写路径只认 `/`,路径的 `*` 不跨 `/`,`**` 跨。模式里不能有 `?` 与 `#`(在 URL
   里它们开始 query 与 fragment,`?` 不当通配)。与调用的 URL 原样相等的规则也命中;"永久允许"记下
   的是原样的 URL,其中的 `*`、`[`、`{` 先按字面转义(`url_rule::literal_pattern`),带 query 的写不成
   模式、只认原样相等,所以记下的规则只放行那一个 URL。
   规整让两个方向的匹配都更准,而 deny / ask 不比按整串 glob 匹配时窄:deny / ask 除了按部分匹配,
   还按整串 glob(`*` 跨 `/`)匹配调用原样的 URL,任一命中就生效——这些规则写下时就是这么读的,
   `https://evil.example/*` 拒绝 `https://evil.example/a/b`。allow 只按部分匹配。写不成这种形状的
   内容(`*example.com*`)作 allow 只命中原样相等的 URL,作 deny / ask 只按整串 glob 匹配。MCP 策略的
   `url` 与 `allowed_authorization_servers` 规则(§6.4)用的是同一个按部分匹配的函数(不带整串 glob 那
   一层;`allowed_authorization_servers` 的模式与 issuer 一样去掉末尾的 `/` 再比)。`web.allowed_domains`
   不是这种规则:它列的是域名(含子域),网络层按主机执行,只共用 URL 的规整取主机这一步。

场景的工具白名单不在这里:它在会话注册表上就把工具拿掉了(§3.5),被拿掉的工具根本
到不了权限这一步。

`bypassPermissions` 只看部署方 managed 层的 deny 与 ask(见下面"用户规则与部署方规则"),
用户自己各层的规则一条都不看:deny、ask、免于 bypass 的路径(`/etc/passwd`、
`rm -rf /` 一类)、配置目录的确认、工具自己的判定(含 §6.2 第 2 道的拒写名单)
全部跳过,输入合法就放行——凭据除外,它在任何模式下都拒绝(本节开头)。仍然生效的是输入校验——读后再写的过期检查、大小上限、§6.2
第 1 道的误读路径、`Bash` 的超时上限与长 `sleep` 拦截——和 OS 沙箱:`Bash` 照样被关在
可写根之内(`sandbox.enabled: false` 除外),无人值守会话也一样;`sandbox.require`
打开而沙箱兜不住命令时,命令照样被拒(S7 在 bypass 之前)。`agixd` 的
`bypassPermissions` 会话同样走 `Policy`,运行中进了计划模式也照样只读;宿主给的
`Assembly.host_policy` 是规则里的一层,bypass 下只有它的 deny 还算数。要 deny 规则仍然算数,用 `acceptEdits`,
或 `dontAsk` 加 allow 规则。

这样设计是因为 bypass 是给"边界在引擎之外"的部署用的——一次性容器、VM、CI 机器,
真正挡东西的是那一层与 OS 沙箱。一个"跳过大部分规则"的模式会让人以为规则还守着,
其实 deny 本来就不是沙箱(§6.1 第 4 条),它在 bypass 下留下的只是错觉;所以 bypass
要么全跳,要么不用,中间态交给 `dontAsk`。

**用户规则与部署方规则。** 上面的规则都是用户自己写的(或项目、插件、客户端替用户写的),
bypass 是用户对自己规则的放弃。部署方——企业 IT、无人值守的托管方——要的是另一回事:
一层用户改不了、用户选的模式也绕不过的设置。这就是 managed 层(`settings::managed`):

- **来源。** 系统路径上的一个文件:macOS `/Library/Application Support/Agix/managed-settings.json`,
  Linux 等 `/etc/agix/managed-settings.json`,Windows `%ProgramData%\Agix\managed-settings.json`
  (嵌入方改了产品名就是那个名字下的同一位置,只读那一份,§8.1)。
  `agixd --managed-settings <路径>`(嵌入方:`DaemonConfig::managed_settings_path`)换成别的
  文件,这个文件**必须存在**。用户数据目录、项目、模型都改不了这条路径:它只来自系统路径
  和启动参数,不从任何设置文件读。嵌入方也可以在 `ConfigSource` 里给一层
  `Tier::Managed`;装配时有文件就以文件为准。读一次,改了要重启 daemon(`config.reload` 的
  `restart_required` 会列出 `managed`)。
- **能写什么。** 任何设置键,和设置文件同一格式,外加 `permissions.disable_bypass`(`true` / `false`)
  与编辑器的 `$schema`。
- **最高的一层。** 除规则列表外,它写的每个键都是会话拿到的值:压过三层文件、`cli` 层(`agixd --set`)、
  宿主在内存里改的值(`HostOverrides`)和会话自带的 `options.settings`——即使设置开了
  `permissions.client_may_loosen`;会话要的值被它压住时报在 `clamped_options`。唯一的例外是键表信任列
  给了松紧次序的键:下层写得比它更严时下层的值留下(会话要更小的预算、项目要更严的沙箱),managed 定的是最宽能到哪。`LoadedSettings::with_managed`
  是唯一套它的地方:读盘(`settings_source::read_pair`,它也作为 `Tier::Managed` 一层进加载器)、宿主覆盖与
  会话覆盖层(`LoadedSettings::overlay`)之后都重新套一遍。`app` 也照收:它的 `app.trust_all_projects`
  与 `app.trusted_projects`(整张表替换,不与用户的并)在加载时就决定项目信任;`app.session_cap`、
  `app.worker_threads` 等进程级的键在启动时读(`process_settings` 也读它)。provider 按 id 排在各层之前,
  同 id 以它的为准。
- **规则压过下层。** managed 的 deny / ask 与其他层的规则一起按第 1 条裁决,下层任何 allow
  都推不翻;在 `bypassPermissions` 下,`Policy` 仍然只拿 managed 层的规则
  判一遍:命中 deny 拒绝,命中 ask(或程序名读不出来的 shell 段)询问,其余照 bypass 放行。
  询问在无人值守会话里就是拒绝。managed 层的询问只有人能答:`PreToolUse` hook 的 `approve` 答得了
  别的询问,答不了它(`Answerable::PersonOnly`;同一次调用下层也有 ask 命中时,报出的
  仍是 managed 那条)。hook 的 approve 在无人值守转换之前生效——它是预先给的回答,不需要有人在场;
  managed 的询问不吃它,无人值守时照样拒绝。
- **managed 的 allow 不比别层的 allow 多什么。** 它和用户层的 allow 一样参与裁决:推不翻
  下层的 deny,也不把 bypass 之外的模式变宽。理由是第 1 条:deny 是硬边界,用户给自己写的
  deny 也应当守住;部署方要放宽的只是"少问",那用普通 allow 就够,而要"禁止"就写 deny。
  让 managed allow 盖过用户 deny,等于让部署方替用户撤销用户自己的边界,这不是 managed 层
  要解决的问题。
- **关掉 bypass。** `permissions.disable_bypass: true` 时,`bypassPermissions` 不管从
  哪来(设置文件、`options.settings.permissions.mode`、agent 类型 frontmatter、团队)都按 `default`
  判;会话创建时报出的模式也是 `default`。
- **读不了就全拒。** 文件读不了、不是合法 JSON、有没有设置读的键或改了名的旧键(`permisions`
  这样的拼写错误不会被当成"没写")、值放不进它的设置、规则写不成规则、写了引擎专用的权限模式——一律当作
  managed 层在、但一个值都不套,拒绝所有工具调用,任何模式都一样,`daemon.doctor` 的 `settings.managed`
  报 `failing`。
  文件不存在(且没用 `--managed-settings` 点名)= 没有 managed 层,一切照旧。
- **看得见。** `daemon.doctor` 的 `settings.managed` 检查报出查找的路径、是否存在、来源与
  内容(`details.settings`,文件的形状);`config.get` 的 `effective` 回答多一个 `managed` 字段,
  `effective` 本身已经套了它。

managed 层管的是权限,不是沙箱:它挡得住模型发起的调用,挡不住能改 daemon 启动参数或
换掉二进制的人。要防机器的使用者本人,启动方式与二进制也得由部署方控制。

`acceptEdits` 只多放行只读调用(S14):项目内的 `Edit` / `Write` 本来就陈述 `Inside`、由自含通道
(S12)放行,项目外的、`.git` / `.agix` / `.agents/skills` 下要确认的,在 `acceptEdits` 下照样询问——无人值守的会话里就是拒绝。
只读调用里够得比"读"更远的也不放行,要求 `reach` 是 `SessionOnly`、`Inside` 或 `Host { listed: true }`:`WebFetch` / `Ping`
只放行 `web.allowed_domains` 列出的主机(含子域),URL 本身就能把数据带到主机那里;
MCP 工具不管 server 报不报 `readOnlyHint` 都询问——那是 server 对自己的说法,要免询问就写 allow
规则(`mcp__<server>` 或具体工具名);
进了会话所在仓库之外的仓库的只读 `git`(`Bash` / `Monitor`)照样询问。读项目外的文件也与
`default` 一样询问:能免询问读的范围是 `security::readable_roots`——会话的工作目录、它能写的
附加目录(`files.extra_dirs` 与实例状态目录)和 `files.read_dirs`(其中有 `"/"` 时不限)。`Read` / `Glob` / `Grep` 按目标路径判,只读的 `Bash` /
`Monitor` 命令按 `bash::paths` 对路径参数的判断(含 `cd` 之后的相对路径、glob 在磁盘上的
匹配;`~`、`$VAR`、命令替换展开不了,一律当作项目外)。这些判断与工具在 `default` 下
陈述 `Inside` 用的是同一个函数,所以这几个工具的读在 `acceptEdits` 下不比 `default` 多放行。

`plan` 拒绝一切非只读工具(S10),唯独离开计划模式的那一个(`ExitPlanMode`,
`ModeStance::LeavesPlan`)照常走规则与询问:宿主批准才退出,回到进入前的模式。判定或询问期间模式
变过(宿主切了模式),批准作废、成拒绝,模式留在切到的那个(`SessionState::permission_mode_changes`)。除此之外
与 `default` 相同:只读但没有陈述自含的调用(读项目外的文件、`WebFetch`、只读的 MCP 工具)
照样询问。

`Monitor` 算不算只读看它要跑的命令,与 `Bash` 用同一套分类:`tail -f log` 是只读,
`npm run dev`、`python3 watch.py` 不是——`plan` 下拒绝,`acceptEdits` 下询问。

子 Agent 与团队成员的每次调用都交给父会话的权限策略判
(`PermissionPolicy::decide`,请求的 `delegate` 是子 Agent 的会话),父会话的规则与宿主的回答都作用于它们,输入校验看的是
子 Agent 自己的会话状态(它读过哪些文件)。判的模式是子 Agent 自己的
(`Delegate::mode_under`):父会话在 `plan` 或 `dontAsk` 时取两者中更严的那个,父会话是
`bypassPermissions` 时随父会话(`PermissionMode::for_delegate`)。只能收紧的 agent 类型(插件带来的、
未信任项目里的)自己写了 `permission_mode` 的,每次调用都取它与父会话此刻模式中更严的那个,父会话
`bypassPermissions` 也不带它走(`Delegate::tightens_only`,经一层层转发的父会话都照此判)。子 Agent 自己的模式起点是父会话的设置给的模式;宿主经
`session.setPermissionMode` 给父会话切过 `plan` 以外的模式之后,起点是切到的那个(`Session::host_permission_mode`),
已经在跑的子 Agent 不变。agent 类型 frontmatter 的 `permission_mode` 可以比父会话
宽也可以严,插件带来的、未信任项目里的类型只能收紧(§6.6);模型在 `Agent` 调用、`TeamCreate` /
`Agent(team_name)` 里自己要的 `permission_mode` 只能收紧。子 Agent 的询问经父会话的通道到宿主,等待上限就是会话的
`permissions.prompt_timeout_secs`;在那里答"永久允许",记下的是这一条命令或路径,不是整个工具。

skill 正文里的 `` !`cmd` `` 在送给模型之前执行,同样先过会话的 `PreToolUse` hook 与权限策略
(按一次 `Bash` 调用判,hook 收到的 `tool_name` 是 `Bash`、`tool_input` 是 `{"command": …}`):
hook 拦下的、deny 规则拒掉的不跑,要询问的也不跑——预处理没有人可问,hook 的批准也不替人
答应:命令是仓库作者写的原文,中间没有模型,批准它的 hook 是在替这段文字背书——只有规则放行的、
或走自含通道放行的(项目内的只读命令)才执行,hook 改写过的命令照同样的规矩判,不跑的替换成一行
写明原因的文字。跑完的输出与 `Bash` 的结果一样过 `PostToolUse` hook,被拦下的不进正文。
判定和执行用的都是会话注册表里的 `Bash`;场景的工具白名单里没有 `Bash` 的会话,命令一条都不跑。
skill 自己的 `allowed-tools` 这时还没生效,不能给自己的命令开门。未信任项目自己的 skill
(`.agents/skills/`)里的命令一条都不跑(§6.6)。

模型调 `Skill`、`context: fork` 的 skill 交给子 Agent、用户打 `/name`,三条路径展开 skill
用的是同一个函数(`skills::expand_for_invocation`,内核经 `SkillCatalog::expand` 调到):
去掉 frontmatter 的正文 → `` !`cmd` `` → 参数替换 → 正文截到 8000 字符 → 接上 `files:`
声明的参考文件。`allowed-tools` 授权到这一轮结束(fork 的子 Agent 有自己的 `PermissionSet`,
不受它影响);只写工具名的整类授权只认用户层 skill,项目层与插件包带来的 skill 必须写成
`Tool(内容)`,而且内容要真正收窄——够得和整类授权一样远的与整类授权一样不给
(`permissions::dangerous::is_unscoped_grant_in`,拿不准的都算):

- 路径类(`Read` / `Edit` / `Write` / `Grep` / `Glob` / `NotebookEdit`):模式按第 3 条的同一套解析
  (`path_rule::resolve_pattern`,相对会话的工作目录)得出它钉住的目录——第一个带通配的组件之前的
  部分——这个目录要在项目根(会话的工作目录)之内且不是项目根本身,按原样写法与解析符号链接之后
  都要。所以 `Write(**)`、`Edit(./**)`、`Edit(*.rs)`(任何深度)、`Write(x/../../../**)`(就是 `/**`)、
  `Edit(/U*/**)`、`Write(/tmp/build/**)`、`Edit(~/notes/**)`、指向项目外的符号链接下的路径都不给,
  `Write(src/**)` 给。
- shell 类(`Bash` / `Monitor`):规则的 glob 里 `*` 跨空格,第一个通配之前必须是一个完整的程序名
  加空格(`git *`),`程序名 …:*` 的前缀也从一个完整的程序名开始;`Bash(b*)`、`Bash(* --version)`、
  `Bash(/opt/x/*)` 不给。程序是 shell 或 wrapper 的(`bash`、`sh`、`env`、`sudo`、`xargs`、`timeout`、
  `find`、`eval`,`permissions::invocation::runs_commands_it_is_given`)也不给——它们跑参数里写的任何命令。
- URL 类(`WebFetch` / `Ping`):主机里的通配要落在一个可注册的域名之内——最后一个带通配的标签后面
  写出的标签不能只是公共后缀(按 Public Suffix List 判,`base::public_suffix`)或数字。
  `https://*.example.com/**` 给;`https://*/**`、`https://*.com/**`、`https://*.co.uk/**`、
  `https://*.github.io/**`(后两个下面的子域谁都能注册)不给。

公共后缀列表随源码带着(`crates/core/src/public_suffix_list.dat`,只留两段及以上的规则),每次发布前
更新;把 `psl` 这类把列表编成代码的 crate 引进来,release 的 `agixd` 要多出约 800 KB,列表原文只占约
140 KB(§1 的体积取舍)。列表过期时漏掉的是新加的后缀,那样的模式会被当成钉住了域名——所以要更新。
危险规则告警(`is_broad_allow`)用的是不看项目根的那一半(`is_unscoped_grant`):用户自己设置里
这类 allow 照旧生效,启动时记一条 critical。未信任项目自己的 skill 什么都不授予(§6.6)。

### 6.2 写路径的管控

写文件走两道:

1. **路径请求本身**——未展开的 `~`、UNC 路径、shell 替换、归一化与显示不一致的名字。
   这些是关于"模型递过来的那个字符串"的。前三样会让路径被读错,算输入不合法,在工具的
   `validate_input` 里就拒掉,任何模式都生效。
2. **它解析成的目标**——三份名单加上项目边界与系统目录前缀。目标按会话 cwd 解析;
   路径里有 `..` 而且跟在符号链接后面时,取文件系统解析出的真实位置,检查、读后再写的
   比对和真正的写入用的是同一个路径。存在的最深一级前缀交给文件系统解析,其余部分逐级接上,
   遇到指向尚不存在之处的链接照它的目标接着走——写入会跟着它落到那里。`Write` / `Edit` /
   `NotebookEdit` 写的就是权限判定时这样解析出的位置(`PermissionFacts::lands_at`,经
   `Approval.landing` 交给 `call`,不在 `call` 里重新解析),`Edit` 与 `NotebookEdit` 也从那里读;
   写的时候从 `/` 起逐级打开目录,整条路径上哪一级都不跟随符号链接
   (`FileSystem::write_resolved`,本机用 `openat` + `O_NOFOLLOW`)。判定之后才换上去的链接——
   在目标处还是在上级目录——让写入失败,而不是把它带到别处:
   - 凭据(`.env*` / `id_rsa` / `.ssh` / `.aws` / `.netrc` …)
   - 不该被随手改的生成物与会被别的工具照着执行的文件(`.gitignore` / `Cargo.lock` /
     `package-lock.json` / `.envrc`)
   - 引擎自己的配置(`settings.json` / `settings.local.json` / `mcp.local.json`,且在 `.agix` /
     `.claude` 下)

   最后一条**按"文件名 + 所在目录"匹配,不按目录**。`.agix` 同时是子代理 git worktree
   的落脚点,把整个目录设为拒写会否掉子代理被创建出来要做的每一次写入。`.agix` / `.claude`
   下的配置目录(`hooks`、`commands`、`agents`、`skills` …)和项目的 `.agents/skills/` 对写工具是"要确认",
   不是"拒写";沙箱里的命令没法逐次确认,所以对它们是拒写(§6.3);外来配置根里一切都算配置,
   沙箱里整个拒写(§8.1)。两边用同一份名单
   (`base::paths::sensitive` 的 `ENGINE_CONFIG_{ROOTS,FILES,DIRS}`、`PROJECT_ENGINE_CONFIG_DIRS`,
   §6.1)。
   另外,凭据名单(§6.1)上的路径在 `Policy` 的 S2 就拒绝了,任何模式都到不了这里。

   **这个不对称是有意的。** 写工具改引擎配置目录是确认,沙箱里的 Bash 写同一处是硬拒,
   理由有两条:无人值守时确认本来就会变成拒绝,所以硬拒只是把这个结果提前;交互时用户
   看得见 Write / Edit 具体要改哪个文件、改成什么,而 Bash 的写入藏在一条命令里,
   用户批准的是命令,看不见它最后写了什么。

`sandbox.allow_write` 给拒绝规则开例外,`sandbox.deny_write` 加自己的名字,
与读侧的 `allow_read` / `deny_read` 对称。**例外只解除拒绝规则,不解除项目边界**——
放宽可写范围是 `files.extra_dirs` 的事,两者分开,不然一个配置会悄悄干另一个的活。

### 6.3 项目之外

可读写的根:项目目录、实例状态目录(`~/.agix`)下的 `scratchpad` 与 `memory`、
`files.extra_dirs`。只读的根再加整个状态目录(凭据除外,§6.1)与
`files.read_dirs`。无项目会话没有项目目录,第一项换成它自己在
`scratchpad/sessions/` 下的工作目录(§3.2);状态目录所在的家目录不在任何根里。
`Read` / `Glob` / `Grep` 与只读 shell 命令在这些根之内直接放行,之外要人批准;
`files.read_dirs` 里写 `"/"` 把读放开到任意位置。这些都是实例的设置——嵌入方创建实例时定。

无人值守的会话没人批准,所以根之外的读写一律拒绝。授权只走设置(上面几项,或
`permissions.allow` 的规则),不从任务文本里读:任务文本常常带着外部内容,让模型照着
一句话给自己开权限,等于让注入的内容给自己开权限。

`Bash` 不能事先知道一条命令会写哪里,所以无人值守时它靠 OS 沙箱:沙箱完整生效
(`Enforcement::Full`)就放行(S15,依据 `UnattendedConfined`,profile 是 `Unattended`),写入被限制在可写根之内;沙箱不可用(关掉了,或 Linux 上没有
bubblewrap)就和别的未授权调用一样被拒。沙箱只限写不限网,网络缺省是 `Unrestricted`,一条被关在
项目目录里的命令照样能往外连;`sandbox.unattended_needs_network_policy: true` 时自动放行另外
要求 `sandbox.network.mode` 是 `deny_all` / `allowlist`,否则回到 allow 规则。这个开关 0.3.x 缺省
关(保持原来的行为),计划在后续 0.3.x 改为缺省开。沙箱在 `assembly` / `daemon` 缺省编译进来。会话的 profile 只限写,不限读——读的限制落在文件工具上(只读通道里的命令另按 strict profile 限读,见下),
`Bash` 从环境变量里读得到什么由 §5 第 5 条决定,设置里点名的密钥都不在其中;读项目外的文件只受凭据黑名单约束:内建的家目录凭据清单(`base::interface::exec::default_deny_read`)加上 `sandbox.deny_read`,
减去 `sandbox.allow_read`,再加上权限规则里点名具体路径的
`Read` deny(下一段)。`deny_read` 里的目录,里面的符号链接指到目录外的,链接指向的地方也一并拒读。
Linux 上被拒读的目录盖一层只读的空 tmpfs,命令在里面既读不到也写不进(写返回 `EROFS`,不会写进一层
没人看得见的 tmpfs);macOS 只拒读,写照样落到真目录里。另外不论怎么配,
§6.1 凭据名单上引擎自己的那部分——状态目录与各场景目录下的设置文件(`settings*.json`、
`mcp.local.json`)和 `mcp/oauth/`,状态目录下的 `daemon/auth/`,以及项目自己的
`.agix/settings.local.json`、`.agix/mcp.local.json`(项目这一层的 key 写在这里)——都读不到,
`allow_read` 也放不开。状态目录里别的东西(skills、memory、scratchpad、会话记录、`scene.json`、
`daemon/instances.d/`)照常可读。`Monitor` 的脚本进同一个沙箱,
`sandbox.require` 对它同样生效,无人值守下的自动放行也走上面这同一条件。

**`Read` deny 规则也进沙箱。** 权限层的 `Read(...)` deny 只管得住文件工具和对命令文本的静态
分析,`python3 -c "open('.env').read()"` 绕得过去。所以会话的命令进沙箱时,它判调用用的那一套规则
(各层设置、会话选项、委派加的 deny)里点名具体路径的 `Read` deny 也进沙箱,放在 `SandboxPolicy` 的
`deny_read_exact`:一个文件或目录(`Read(./.env)`、`Read(/srv/secret)`、`Read(~/.aws)`),或目录后面
只跟 `/**`(按那个目录算)。路径按权限层判这条规则时的锚解析(`permissions::path_rule::named_path`):
相对的按 `cwd`,worktree 会话另按项目根(`refusal_rule_roots`)再解析一份,主检出里的路径另给出
worktree 里的那一份(`main_checkout_copies` 的反方向)。

它们按写出的那个路径拒:macOS 拒这个路径本身与它解析掉符号链接后的写法,Linux 遮住这个路径本身;
与 `deny_read` 不同,目录里的符号链接不跟——`Read(./node_modules/**)` 不会把 workspace 里
`node_modules` 链过去的包也拒掉,目录里一个指回项目的链接也不会把项目拒掉,与权限层判 `Read` 工具
一致。`allow_read` 换什么写法(`/var/...` 对 `/private/var/...`)都放不开它们:macOS 把它们写在
`allow_read` 的放开之后,Linux 不看 `allow_read`。

不翻译的:带别的通配的(`**/*.pem`、`*.key`、`secrets/*.json`、`secrets/*`——`*` 不跨 `/`,整目录拒掉
比规则严、花括号与字符类),要沙箱挡就写进 `sandbox.deny_read`,或写一条 `Bash(...)` 的 deny;ask,
沙箱不会问。命令启动就要读的也不翻译,跳过并告警,那一条仍只由权限层管:规则的路径**是或包含**会话
的工作目录、项目根、可写目录(`additional_writable_dirs`、缓存目录、会话的 `TMPDIR`)、状态目录、
后端隐含可写的根(`/dev`、macOS 的 `DARWIN_USER_TEMP_DIR`)之一的(`Read(.)`、`Read(./**)`、祖先
目录、`/`),或**是、包含或落在**程序加载用的系统目录(`platform::sandbox::system_read_roots`,即
strict profile 总能读的那张表:`/usr`、`/bin`、`/System`、`/opt/homebrew`、`/etc`……)里的——否则
shell 本身起不来。

还不存在的路径照样加:macOS 的 `subpath` 认它,Linux 只遮得住命令启动时已存在的。被拒的目录同
`deny_read`(上一段):Linux 上命令写不进,macOS 上写得进。macOS 按路径匹配,命令写得了被拒文件所在
的目录(项目里的文件都是)时,先改名再读挡不住(`platform::sandbox` 已知逃逸第 7 条);Linux 的遮挡
是挂载,改名带不走。沙箱关着或不可用时什么都不加。规则知识留在
`permissions`(`PathDenials::denied_read_paths`),平台层只见路径,两者在 `tools/src/bash/sandbox.rs`
的 `session_policy` 接上,三种 profile 都从它出发。

**三种 profile。** 同一个沙箱后端按结论带出的 `ExecProfile`(§6.1 末的表)收到三份不同的
`SandboxPolicy`,由 `tools/src/bash/sandbox.rs` 的 `policy_for_profile` 选;没有 `approval` 的调用
(没有谁判过它)按 `Session`:

- **`Session`**:本节其余部分描述的就是它——可写根之内可写,读不限(凭据除外),网络按 `sandbox.network.mode`,
  环境按 `sandbox.env`。
- **`Unattended`**:`Session` 再加 `deny_ipc_escapes`(下面"沙箱只管命令自己的进程树"一段)。
- **`Strict`**:命令靠自己的话放行(`Contained` / `AcceptEdits`:`Bash` 与 `Monitor` 的只读通道)。这类命令
  没有人看过,分类器一旦判错就没有别的兜底,所以跑在更紧的 policy 里(`SandboxPolicy::strict`):
  网络 `DenyAll`;只有会话的 `TMPDIR` 可写(`only_additional_writable`,工作目录也不可写);
  读 `ReadScope::Only`——可读根(`security::readable_roots`)、`sandbox.cache_dirs` 与会话的
  `TMPDIR`,加后端自己的一张表:系统目录(macOS 的 `/usr /bin /sbin /System /Library/Developer/CommandLineTools
  /Applications/Xcode.app/Contents/Developer /private/etc /private/var/db/timezone /opt/homebrew /usr/local /dev`,Linux 的
  `/usr /bin /sbin /lib* /etc /opt /nix/store`)与用户的 `~/.gitconfig`,以及 `~/.config/git/`、`$XDG_CONFIG_HOME/git/`
  下的 `config ignore attributes` 三个文件(同目录的 `credentials` 不在其中;`HOME`、`XDG_CONFIG_HOME` 按命令自己的环境取,最小环境不继承 daemon 的 `XDG_CONFIG_HOME`),其余路径命令只看得到名字与元数据、
  读不到内容,`deny_read` 与引擎的凭据在范围之内照样读不到;环境 `EnvScope::Minimal`,只留
  `PATH HOME USER LOGNAME SHELL TERM LANG LC_* TZ TMPDIR`,`sandbox.env.allow` 加不进去,引擎的凭据照剔;
  `deny_ipc_escapes`。macOS 写成 `(deny file-read*)` 加 `(allow file-read-metadata)` 再按根逐个放开;Linux 不再
  `--ro-bind / /`,改成逐根 `--ro-bind-try`。仓库配置让只读通道里的 git 起 `diff.external` 一类程序时,
  那个程序因此也断网、只写得了 `TMPDIR`、只读得了项目。

strict policy 兑现不了时(没有后端,Windows,Linux 上没有 bubblewrap):`sandbox.require` 关着,
通道照常放行,命令尽力而为;打开时通道关闭(S12、S14 都不放行,`strict_unheld`),命令改为询问,批准后按
`Session` 跑。**未信任项目**(§6.6)里 strict 不是 `Full` 时,会话自己仓库里的 `git` 也退出只读通道、改为询问:
仓库配置是项目作者写的,没有 strict 沙箱兜着,它起的程序就跑在沙箱外。

沙箱里的命令能写的:工作目录、`files.extra_dirs` 与会话授予的目录(worktree 里的
子 Agent 还有仓库 git 目录里提交要写的几处,§3.4)、
`sandbox.cache_dirs`(见下)、状态目录下的 `scratchpad` 与 `memory`、`/dev`,以及
会话自己的临时目录——它作为 `TMPDIR` 交给命令(`<状态目录>/tmp/<会话 id>`),别的会话的
写不到;7 天没动过的,进程里第一次准备临时目录时清掉。
无项目会话的工作目录在状态目录的 `scratchpad` 下,那里本来就可写(别的会话的命令也写得到),
所以状态目录的其余部分照样不可写,其中的 socket 照样连不上——下文"项目就在状态目录里时除外"
只指工作目录落在 `scratchpad`、`memory` 之外的情形。
`/tmp`、`/var/tmp` 这类共享的临时目录写不到;macOS 上另外放开当前用户的
`DARWIN_USER_TEMP_DIR`,因为系统的 `mktemp` 不看 `TMPDIR`、只往那里建文件——同一用户的
会话在这里彼此可写,所以会话的 `TMPDIR` 不放在这里。Linux 上
`/tmp` 是每条命令私有、用完即弃的 tmpfs。写规则之前每个路径都先解析符号链接(macOS 的
`/tmp` 实为 `/private/tmp`),否则规则落不到内核检查的那个路径上。

可写范围之内,引擎或 git 之后会在沙箱外照着执行的东西不可写(工作目录是 worktree 时,下面
`.git` 下的各项指引擎建 worktree 时得到的那个 git 目录,§3.4):项目的 `.git`、`.agix`、
`.claude` 这几个目录项本身(不能改名挪走或换成符号链接,不存在时也建不出来)、`.git/config`、
`.git/config.worktree`、`.git/commondir`、`.git/hooks/`,每个子模块的 git 目录(`.git/modules/`
下,嵌套的子模块在上一层的 `modules/` 下)里的同样几项以及通向它的各级目录(不能挪走换一个;
查找不跟符号链接,深度和个数有上限)、`.agix` 与 `.claude` 下的配置
(`settings.json`、`settings.local.json`、`mcp.local.json`,以及 `mcp`、`hooks`、`agents`、
`skills`、`plugins`、`rules`、`scenes`、`policy`、`scripts`、`output-styles` 目录)、项目的
`.agents/skills/` 连同它上面的 `.agents` 目录项(macOS 上不存在时也建不出来;`.agents` 下别的
东西照常可写)、各链接 worktree 在 `.git/worktrees/` 下那一项的 `config`、`config.worktree`、
`commondir`、`gitdir`,引擎 worktree 的 `.git` 链接——这两类对命令起来之后才建的 worktree 也成立,
免得前一条命令留在后台的进程改写之后某个子 Agent 的 worktree:macOS 用正则,Linux 把
`.git/worktrees/`(不存在时先建一个空的)与 `.agix/worktrees/` 整个只读、已有的 worktree 逐个重新
打开,所以沙箱里的命令自己 `git worktree add` 会失败(worktree 由引擎建:它的 git 跑在只写指定根的
策略下,不受这两条模式约束,只受已有 worktree 的那几项约束)。worktree 子 Agent 能写的那一项里
不存在的控制文件也建不出来(Linux 上以 `/dev/null` 挡住,留下一个空文件),`objects/info/` 不可写
(`alternates` 会让 git 去别处读对象);它分支的 ref 在 Linux 上只能连同所在目录一起放开——git
以改名 `.lock` 的方式改写 ref,改名到挂载点上会失败——这个目录里已有的其余条目逐个只读,于是能在
`refs/heads/agix/` 下新建 ref,改不了已有的;状态目录除
`scratchpad`、`memory` 与本会话的 `tmp/<会话 id>` 外整个不可写。`git add` / `git commit`、在 `.agix` 下写草稿照常可用;
`git config`、`git remote add` 这类改仓库配置的命令在沙箱里会失败。Linux 上 `.agix`(以及已经
存在的 `.claude`)整个只读、已有的工作目录逐个重新打开,所以其下直接新建条目也会失败;Linux
上不存在的 `.claude` 不加挡(挡它会在每个项目里留下一个空挂载点),`.agents/skills/` 同理只在
`.agents` 已经存在时挡(`.agents` 绑定到自身,挪不走;`skills` 只读,不存在时留一个只读的空挂载点),
不存在的 `.agents` 在 Linux 上建得出来;没有 `.git` 的项目、没有
`commondir` 的仓库在 Linux 上补不上这一道(见 `sandbox/mod.rs` 模块头第 3 条),所以有下一段。

读遮挡的写法两个后端不同,来源相同:都是 `SensitivePaths::credential_locations` 给出的位置。macOS 把
每条翻成一条读拒绝正则,所以 profile 写出之后才新建的场景目录也在内;内核按解析后的路径匹配,所以
链进 `scenes/` 的场景目录(`Located::linked_dirs_resolved`)按解析后的位置再各加一条。写保护同理,
状态目录与各 `scenes/<场景>/` 下的 `settings.json`、`scene.json`、`prompt.md` 是一条正则。Linux 的状态目录遮挡由
`platform::sandbox::mask::mask_plan` 生成 tmpfs 与只读 bind 的序列(不分操作系统,可在 macOS 上单测):
每个含凭据或通向凭据的目录先盖一层 tmpfs,再把其余条目只读绑回;之后写进去的凭据落在 tmpfs 里,
读不到。通向凭据的条目若不是真目录(符号链接的场景、`scenes/` 里的普通文件)保持遮住。项目在状态
目录里、或策略只写指定的根时,整块照样盖 tmpfs,项目与可写根挖洞挂回可写;项目里本身有凭据的(把某个
场景目录当项目打开),洞里的凭据按 `Located::existing()` 逐个遮挡。项目就是状态目录本身时根不能盖
(盖了项目根下新建的条目都落进 tmpfs),只把本来就只读、又含凭据的配置目录(`scenes/`、`mcp/`)各自
整块盖上并重新挂成只读,其余凭据逐个遮挡;这种情况下状态目录也按配置根保护:它的 `ENGINE_CONFIG_FILES`
与 `ENGINE_CONFIG_DIRS` 对沙箱命令只读(cwd 所在的那个配置目录除外)。项目自己的 `.agix` 是真目录时
同样按 `mask_plan` 盖上(tmpfs 挂成只读,配置条目只读绑回、工作条目可写绑回),之后写进来的
`settings.local.json`、`mcp.local.json` 读不到。

逐个遮挡的 `/dev/null` 绑定钉在挂载那一刻的文件上:引擎之后新建、或用临时文件 rename 覆盖的密钥文件,
命令留在后台的进程读得到。这是 Linux 上的已知缺口(`sandbox/mod.rs` 模块头"已知出路"第 6 条),只剩可写
目录里直接放着凭据的几种布局——项目就是状态目录时它顶层的密钥文件与尚不存在的 `daemon/auth`,项目是
某个场景目录时那个场景的密钥文件与尚不存在的 `mcp/oauth`,以及 `.agix` 是符号链接的项目的密钥文件;
要堵就得把项目目录本身盖住,项目就没法用了。macOS 的读拒绝按路径正则匹配,没有这个缺口。

保护只到项目自己的 `.git`。沙箱里的命令可以在子目录里 `git init` 一个新仓库、写它的
`.git/config`(`core.fsmonitor` 之类),再 `git add` 成一个 gitlink 登记进项目的 index;
用户之后在项目里跑 git(沙箱外)会进这个嵌套仓库、照它的配置执行,这一步不受沙箱约束。
两个后端都挡不住:禁止新建名为 `.git` 的目录项会连 `git init`、`git clone`、包管理器拉
git 依赖、测试里建临时仓库一起弄坏,而且在别处建好再挪进项目照样绕过。引擎自己跑的 git
带 `-c core.fsmonitor=false` 等选项,不受影响。

所以由命令结束之后的检查补上(`obligations.scan_nested_repos`,§6.1)。凡不是 `Strict` profile 的命令——
人批准的、规则或 hook 放行的、无人值守放行的、bypass 下的——在 `call` 返回之后、`PostToolUse` 之前(后台命令
在它的任务结束时)由 runtime 调 `tools::nested_repos::scan`:

1. 用会话沙箱里的引擎 git 跑 `git ls-files -s -z`,取 mode 160000 的 gitlink;用户的 `git status` 会进
   嵌套仓库再进它自己的 gitlink,所以对每个嵌套仓库同样列一次,逐层往下(最多 8 层、256 个仓库);
2. 按 git 的方式解析每个 gitlink 的 `.git`:目录本身、符号链接指向的目录、`gitdir:` 文件指向的目录(相对
   路径按 `.git` 所在目录算,包括指到 `.git/modules/<名>` 的),在解析出的 git 目录里查会让 git 起程序的
   东西——`config` 与 `config.worktree` 里的 `core.fsmonitor`(非布尔值)、`core.hooksPath`、`core.sshCommand`、
   `core.pager`、`core.editor`、`core.askPass`、`core.gitProxy`、`core.alternateRefsCommand`、`diff.external`、
   `diff.*.{command,textconv}`、`merge.*.driver`、`pager.*`(非布尔值)、`sequence.editor`、`credential.helper`、
   `include.path`、`includeIf.*`、`gpg.program`、`uploadpack.packObjectsHook`、`remote.*.{uploadpack,receivepack}`、
   `filter.*.{clean,smudge,process}`,以及 `hooks/` 下任何不是 `*.sample` 的文件。配置按字节读,开头的
   UTF-8 BOM 跳过。`.gitmodules` 是命令写得到的文件,声明与否只决定无害的子模块提不提示,不豁免检查;
3. 另外经 `exec.filesystem` 有界遍历工作树(深度 8、条目 5 万;`node_modules`、`target`、`.venv` 这类依赖、
   构建与缓存目录不进),找新冒出来的 `.git`,只作提示。

要防的是命令造出来或改过的仓库,不是用户自己的:会话第一次工具调用执行之前(`tools::nested_repos::take_baseline`,
每个会话一次),runtime 按第 1、2 步把当时已有的嵌套仓库连同其配置摘要(config、config.worktree、各 hook 的
名字与内容)记进会话 ledger。之后有危险键、但摘要与这份基线相同的,不问、不移,未声明的只提示一次;新出现
的仓库、摘要变了的,照下面处理。

有危险键的:有人值守时问"保留还是移开",没答、超时都按移开;无人值守直接移开——把 `sub/.git`(目录、符号
链接或 gitdir 文件本身,不动它指向的目录)改名为 `sub/.git.agix-quarantine-<UTC 时间>`,由引擎在沙箱外执行,
改回名字即还原。两种情况都在工具结果后面附一行说明并发 `Notification`。没有危险键的只附说明。同一个仓库
每个会话只报一次,被保留的要配置变了才再报。
`Strict` 下的命令除 `TMPDIR` 外什么都写不了,不扫。这道检查挡不住的是命令起了个比它活得久的进程、之后才埋仓库。
见 `sandbox/mod.rs` 模块头已知逃逸。

引擎自己跑的 git(每轮的 `git status`、worktree 的创建与清理)经 `base::engine_exec::EngineExec` 起,走会话的
`exec.process`:会话的命令进沙箱(没有
`sandbox.enabled: false`)时,它也进同一个沙箱,不论有人值守与否——仓库配置让 git 去
跑的 filter driver 之类因此同样受约束。这时它只能写会话的 `TMPDIR`,创建 worktree 时再加
git 目录与仓库根下的 worktree 目录(缺省 `.wkts/`),工作目录本身不可写,网络不通。沙箱兑现不了时照
`sandbox.require` 办:要求就不跑,当作没有 git 信息(状态不显示,worktree 创建报错),
不要求就和 `Bash` 一样在沙箱外跑——未信任项目(§6.6)除外:它的仓库配置是项目作者写的,在沙箱外跑
就是让那些程序不受约束地起来,`admit_git` 对它拒绝,会话状态里的分支名改为直接读 `.git/HEAD`(`read_head`),
不问 git。删除 worktree 时目录与 `.git/worktrees/` 下的那一项由
引擎直接删,不交给 git——沙箱不许写已有 worktree 的 `.git` 链接与 `commondir`。沙箱的 git
只给 git 目录就在项目根下 `.git` 的仓库建 worktree,别处的 git 目录沙箱护不住它的配置。
不论进不进沙箱,这些 git 都带 `-c core.fsmonitor=false -c core.hooksPath=/dev/null`,都领一个
自己的会话(§5 第 4 条),它跑起来的 filter driver 因此也没有控制终端;超时就整组停掉。
`/resume @repo` 找仓库根不跑 git,只向上找 `.git`。

沙箱只管命令自己的进程树,管不到它请系统服务代办的事。macOS 上实测:`open` 让
LaunchServices 在沙箱外起一个应用,`defaults write` 让 `cfprefsd` 写任意偏好域,Apple event
能让沙箱外的应用替它做事(Finder 本身拒绝来自沙箱进程的事件,别的应用未必);`launchctl
submit` / `bootstrap` 则被 launchd 拒绝。无人值守会话的 `Bash` / `Monitor` 另外拒绝这几条:
macOS profile 加 `(deny lsopen)`、`(deny user-preference-write)`、`(deny appleevent-send)`,
拒绝 `com.apple.SecurityServer` 的 mach-lookup(`security find-generic-password` 经它读钥匙串),
并拒绝连 tmux(`/private/tmp/tmux-<uid>`、`$TMUX_TMPDIR`、`$TMUX` 指的 socket)与 screen
(`/private/tmp/screens`、`$SCREENDIR`)的 socket——往一个 tmux 会话里敲命令就是在沙箱外跑;
Linux 上 bwrap 加 `--unshare-ipc` 并遮住 `$XDG_RUNTIME_DIR/bus`、`$XDG_RUNTIME_DIR/systemd`、
`DBUS_SESSION_BUS_ADDRESS` 指向的路径与 screen 的 socket 目录(`/run/screen`;tmux 的在 `/tmp`,
本来就是命令私有的;抽象 socket 的总线只有断网才断得掉;Linux 这一半已在 Debian 12 aarch64 的真机
bubblewrap 上跑过,CI 的 `sandbox-linux` job 也在真 bubblewrap 上跑)。`tmux -S` 自己选的 socket 找不到。有人值守的会话不加:命令经人确认,`open` 打开浏览器
之类照常可用;钥匙串也照常可用——`git push` 走 osxkeychain 凭据助手、`codesign`、
`xcrun notarytool` 都要它,而钥匙串对不在 ACL 里的程序还会弹窗再问一次。
网络敞开时,命令能连到的本机服务(sshd、容器守护进程)在任何策略下都是出口。

daemon 自己是最近的那个本机服务:连上它的 socket 就是宿主,能 `mcp.addServer` 让 daemon 在沙箱
外起程序、`project.trust` 自己的项目、`session.respondToPrompt` 自己批准自己的询问。所以不论
哪种网络策略、有没有人值守,沙箱里的命令都连不上它:macOS profile 拒绝连状态目录下的任何 unix
socket(项目就在状态目录里时除外)和这个进程监听的 socket(`--socket` 放在别处的也算),按解析后
的路径比,符号链接、相对路径绕不过;这个进程的 TCP / WebSocket 端口同样拒绝。Linux 上 bwrap
拒绝不了 connect,只能把 `/dev/null` 挂到状态目录与各场景目录下的每个 socket 文件、以及这个进程
监听的 socket 上(目录本身照常可读);TCP 端口在 Linux 上只有断网才断得掉(Linux 这一半同样已在真机 bubblewrap
上跑过,CI 的 `sandbox-linux` job 覆盖)。daemon 这一侧另有一道:unix socket 上对端进程是 daemon 自己起的
(它的子孙,或者还在 daemon 起的某个进程组里——父进程退出后被 init 收养的后台进程也算),连接
直接以 `UNAUTHORIZED` 拒绝,不论命令进没进沙箱;`setsid` 离开进程组的孤儿认不出来,这时靠沙箱。
TCP / WebSocket 靠 token:子进程环境里没有 `AGIX_DAEMON_TOKEN`,但 `--token` 写在命令行上时
`ps` 里谁都看得见,Linux 上这就是一条出口,所以用环境变量传 token。

这个进程监听的 socket、端口和下面的 token 文件登记在 `platform::sandbox::fence_socket` /
`fence_port` / `fence_secret` 返回的 `Fence` 上,跟它们护着的东西同生共死:监听器关了、token
文件删了就注销,之后别的服务用到同一个端口或路径不再被挡。token 文件先删、后注销,存在期间
始终挡着。

**Linux 围不了单个端口。** bwrap 只有 `--unshare-net` 一个网络开关,没有按地址、端口过滤的
原语,所以 `fence_port` 在 Linux 上什么也不做:网络开着(`sandbox.network.mode: unrestricted`,缺省)时,
沙箱里的命令连得上 daemon 的 TCP / WebSocket 端口,也连得上本机任何监听回环的服务。挡住它的只有
token——子进程环境里没有 `AGIX_DAEMON_TOKEN`。要在 Linux 上把这条路也断掉,用
`sandbox.network.mode: deny_all`、`allowlist` 或 `proxy`(三者在 bwrap 上都是整个断网,`proxy` 只经中继
连到代理),或者不开 TCP / WebSocket、只用 unix socket。

**沙箱不按域名放行。** `sandbox.network.mode: allowlist` 对 `WebFetch` / `Ping` 是"只去
`web.allowed_domains` 列出的主机",对沙箱里的 `Bash` / `Monitor` 命令则两个后端都做不到:
sandbox-exec 的 profile 里远端主机只能写 `*` 或 `localhost`,写了别的域名 profile 编译不过,
每条命令都起不来;bwrap 只有 `--unshare-net`。所以 macOS 上 allowlist 是 `(deny network*)`,
名单里有 `localhost` 或回环地址时再放开回环(连得到本机所有监听回环的服务,daemon 自己的端口
除外);Linux 上整个断网,命令只有自己网络命名空间里的回环。名单里的域名两边都连不上,后端报
`Partial`,`unmet` 里列出这些域名:`sandbox.require` 打开时每条沙箱命令都被拒,
无人值守的自动放行要求 `Full`,也不放行。`daemon.doctor` 的 `sandbox.network_allowlist` 在名单里
有回环以外的条目时报 degraded。按域名放行用 `sandbox.network.mode: proxy`(§5):命令只能连宿主起的
代理,域名由代理过滤。

命令没有控制终端:本机 provider 让它领一个新会话(§5 第 4 条),所以它打不开 `/dev/tty`,
也就没法用 `TIOCSTI` 往 daemon 所在的终端里塞按键(CVE-2017-5226 那一类)。引擎自己跑的 git
(连同仓库配置让它跑的 filter driver,沙箱里的命令写得出这样的配置)和 `Read` 调的 `pdftotext`
经会话的 `exec.process` 起,同样如此;不经本机 provider 起的 MCP stdio server 也领一个会话。bwrap 不加
`--new-session`:它会把 bwrap 的 PID 1 连同整个 PID 命名空间带出命令的进程组,停组就停不到
命令留下的后台进程了。

**已知的缺口。** 上面两道合起来,只有"命令在沙箱外、又 `setsid` 离开了 daemon 起的进程组"这一种
情况挡不住:lineage 按进程组认,`setsid` 之后认不出来;沙箱本来能拦 connect,可这时根本没有沙箱。
没有沙箱的情况有三种:Linux 上没装 bwrap、Windows、设置里写了 `sandbox.enabled: false`。
这时一条用户批准过的命令(比如跑仓库脚本的 `npm test`)起的后台进程能连上 unix socket,当宿主用。
这是接受下来的限制,不是疏漏:按对端 pid 往上找祖先在孤儿身上同样断链,文件权限分不出同一个用户
的两个进程。

缓解办法是 `--unix-socket-auth`(缺省关):unix socket 也要先 `daemon.auth`。token 是
`--token` / `AGIX_DAEMON_TOKEN` 给的那个;没给就每次启动新生成一个,写进
`~/.agix/daemon/auth/<instance>.token`(`0600`,目录 `0700`),实例文件的 `token_file` 指向它,
退出时删掉。这个文件登记进沙箱的读禁止名单(`sandbox::fence_secret`):macOS profile 末尾
`(deny file-read* (literal …))`,`sandbox.allow_read` 放不开;Linux 上 `/dev/null` 挂在它上面。
要说清这个开关挡住的是什么:上面三种没有沙箱的情况里,命令同样能读这个文件,所以 token 文件
在那里挡不住;真正起作用的写法是宿主自己起 daemon、经 `AGIX_DAEMON_TOKEN` 传 token、不落盘
——这样连 socket 还得先拿到另一个进程的环境(Linux 上同一用户可以读 `/proc/<pid>/environ`,
门槛高了,但不是没有)。打开它之后每个宿主都要在 unix socket 上握手,所以先做成可选,
之后再考虑缺省开。

包管理与构建工具要写自己的缓存(`pip`、`npm`、`cargo`、`go` …),所以沙箱里的 `Bash`
另外可写 `sandbox.cache_dirs`:按工具链分组的缓存目录,缺省列好了 python / node /
rust / go / java / ruby / dotnet / swift 的常见位置,开箱即用。设置文件里写一个组名就整组替换
(`"python": []` 去掉这一组),写新组名就是加一组,没提到的组保持缺省。这些目录只对 `Bash`
放开,文件工具写不到。

`.agix` 下的路径缺省要人确认(它是 agent 的配置),只有 `scratchpad`、`memory` 两处除外:
它们是提示词让模型去写的工作区。`EnterWorktree` 与 `Agent` 建的工作树不在 `.agix` 里,在仓库根的
worktree 目录(缺省 `.wkts/`,§8.1),是项目本身的工作(工作树里自己的 `.agix`、`.claude`、`.git`
照旧要确认)。项目的
`.agents/skills/` 下的写入同样要确认,`.agents` 下别的路径不要。

### 6.4 MCP 策略

`mcp.servers` 是用户和项目自己写的,插件包也会带(`<插件>-mcp-<名字>`)。企业部署要的是
另一回事:不管这些配置写了什么,只有管理员认可的 MCP server 才连得上。这由 MCP 策略决定,
它只看部署方控制的来源,**从不读用户数据目录或项目里的文件**。

**来源**,只取一个,先到先得:

1. 宿主传入 `Assembly { mcp: McpOptions { policy: Some(policy), .. }, .. }`(`mcp::policy::McpPolicy`)。
   这时不读任何文件。
2. `agixd --mcp-policy <路径>`(嵌入方:`DaemonConfig::mcp_policy_path`)指定的文件。
   这个文件**必须存在**:写了路径而文件不在,按读不了处理。
3. 系统管理路径:macOS `/Library/Application Support/Agix/mcp-policy.json`,Linux 等
   `/etc/agix/mcp-policy.json`,Windows `%ProgramData%\Agix\mcp-policy.json`(随产品名,§8.1)。不存在 =
   没有策略,一切照旧。

策略在装配时读一次,改了要重启 daemon。

**读不了就全拒。** 文件读不了、不是合法 JSON、有不认识的键(`allow_servers` 这样的拼写
错误不会被当成"没写"而放行一切)、一条规则写了两个键、glob 写错——一律当作策略在、但
拒绝所有 MCP server,并在 `daemon.doctor` 的 `mcp.policy` 检查里报 `failing`。

**格式**:

```jsonc
{
  "allowed_servers": [              // 可省;省略 = 没被拒的都放行;[] = 全部拒绝
    { "command": "/opt/mcp/bin/github-mcp" },
    { "command": "/opt/mcp/bin/*" },
    { "url": "https://*.corp.example/**" }
  ],
  "denied_servers": [               // 可省
    { "command": "/tmp/**" },
    { "name": "shell" }
  ],
  "allowed_authorization_servers": [ // 可省;省略 = 任何授权服务器;[] = 全部拒绝
    "https://login.corp.example",
    "https://corp.okta.com/oauth2/default"  // 写具体租户;*.okta.com 放进任何人的租户
  ],
  "enterprise_idp": {               // 可省;企业托管授权换身份的 IdP(mcp_auth.md §4)
    "issuer": "https://login.corp.example",
    "token_endpoint": "https://login.corp.example/oauth2/token",
    "client_id": "agix"
  }
}
```

`allowed_authorization_servers` 管 OAuth(§6.5):远端 server 的授权服务器 issuer 规整成
`scheme://host[:port][/path]`(末尾不带 `/`)后按 §6.1 第 5 条的 URL 模式拆开匹配,不匹配的在任何跳转、注册、令牌
请求之前被拒绝,`mcp.auth.start` 报 `MCP_SERVER_DENIED`。`enterprise_idp` 给出
`"grant": "enterprise"` 的 server 用来交换用户身份的 IdP;宿主经 `Assembly::mcp.enterprise_idp`
给了的话以宿主的为准。策略读不了时两者都不生效:没有授权服务器被允许,也没有 IdP。
issuer 的 glob 不要通配托管 IdP 的域名:`https://*.okta.com/**`、`https://login.microsoftonline.com/**`
放进的是这家服务上**任何**租户,攻击者自己开一个就能当上"允许的"授权服务器;写到具体租户
(`https://corp.okta.com/oauth2/default`、带租户 ID 的路径)为止。

每条规则恰好一个键,按它匹配:

| 键 | 匹配对象 | 规则 |
|---|---|---|
| `name` | server 的名字(`mcp.servers` 的键;插件的是 `<插件>-mcp-<名字>`) | 完全相等;**只在 `denied_servers` 里起作用** |
| `command` | stdio server 的 `command`,`$VAR` / `${VAR}` / `${VAR:-默认}` 展开之后 | glob;不带通配就是完全相等 |
| `url` | 远端 server 的 `url`,同样先展开,再规整成 `scheme://host[:port]/path` | URL 模式(§6.1 第 5 条):scheme、主机、端口、路径分别匹配 |

**`name` 不放行。** 名字是写 `mcp.servers` 的人自己起的键,谁都能把任意命令或 URL 起名叫
`github`,所以 `allowed_servers` 里的 `name` 规则不放行任何 server——放行要落在实际跑的
`command` 或实际连的 `url` 上。这样的规则不让策略失效(其余规则照常),`daemon.doctor` 的
`mcp.policy` 报 `degraded` 并在 `inert_allowed_rules` 里列出它们;被它"命中"却因此被拒的
server,拒绝原因会说明。`denied_servers` 里的 `name` 照常生效:按名字拒绝不会放进不该放的。

glob 按 §6.1 的约定:`*` 不跨 `/`,`**` 跨。所以 `{"command": "npx"}` 只放行写成 `npx`
的命令,不放行 `/tmp/evil-npx`,也不放行 `/usr/local/bin/npx`;`/opt/mcp/bin/*` 不放行
`/opt/mcp/bin/sub/x`。匹配前 `command` 先按字面规整(去掉 `.` 段与重复的 `/`,`..` 按上一级
折算),含 `..` 的命令不命中任何 `allowed_servers` 规则——`/opt/mcp/**` 不放行
`/opt/mcp/../../tmp/evil`——但照样按折算后的路径去比 `denied_servers`。macOS 的文件系统不分
大小写,`command` 规则在那里也不分(`/tmp/**` 拦得住 `/TMP/evil`)。`command` 规则不匹配远端 server,`url` 规则不匹配 stdio server。
规则不看 `args`:放行一个解释器(`/usr/bin/python3`、`node`)就放行了它能跑的任何脚本。
不带 `/` 的命令名在起进程时按 `PATH` 找,而 `PATH` 用户改得了(daemon 的环境、server 配置
的 `env`),所以要钉死一个二进制,放行它的绝对路径。

URL 规整去掉了 userinfo、query、fragment,主机名小写、去掉末尾的 `.`、默认端口省略,
`ws://` / `wss://` 当作 `http://` / `https://`(WebSocket server 连不上时会回退到同一主机的
HTTP;规则里写的 `ws://` / `wss://` 同样换过来):`https://*.corp.example/**`
不会被 `https://evil.example?.corp.example/` 或 `https://evil.example/.corp.example/` 骗过,
`denied_servers` 里的 `https://evil.example/**` 也拦得住 `wss://evil.example/` 和
`https://evil.example./`;它不匹配显式写了非默认端口的 `https://mcp.corp.example:8443/`
(要放行就写进模式里,或写 `:*`)。主机里的通配只在主机里:`https://**` 是任何主机的根路径,不是任何 URL。
路径要用 `/**`:`https://h/*` 只匹配一层路径。`url` 规则与 issuer 写不成 `scheme://host[/path]` 的
(`**`、`*.corp.example`、带 `?` 或 `#` 的)算规则写错,整份策略按读不了处理。

**deny 是尽力而为,allow 才可靠。** 匹配只看规整后的字面:同一台主机换个 IP 字面量
(`https://203.0.113.7/`)、换个 DNS 别名、换个装着同一个二进制的路径,`denied_servers` 都拦
不住。要可靠地限制连什么,写 `allowed_servers`。

**裁决**:命中任一条 `denied_servers` 就拒;否则有 `allowed_servers` 时必须命中其中一条;
都没写就放行。deny 永远优先,和 §6.1 一样。

**作用在哪里**:会话池连 MCP 的每个入口都先过同一道 `SessionPool::admit_mcp`——启动时的
连接、`config.reload`(以及设置文件变化触发的重载)后的重连、另一个项目第一次起会话时的
连接、`mcp.addServer` / `updateServer` / `removeServer` / `setEnabled` / `reconnect` 之后对单个 server
的对账(`reconcile_mcp_server`)。插件声明的 server 在这些地方都和 `mcp.servers` 合在一起、一视同仁;
`mcp.servers.<名>.enabled` 为 `false` 的在准入之前就略过,不连接、不发通知。被拒的 server **不起进程、不发请求**;启动与重连时发一条
`mcp_connect_failed`(`error` 写明是哪份策略、哪条规则),在 `mcp.status` 里以
`"state": "denied"` 列出;`mcp.addServer` / `mcp.updateServer` 在写 settings.json **之前**
就以 `MCP_SERVER_DENIED` 拒绝。见 `daemon_rpc_protocol.md` §6.5、§8、§9。

**不受管的**:宿主经 `customize_session` 或自己的 `Builder` 装进会话的 `McpModule`。它们是
宿主的代码,不是用户能改指向的配置;要管,宿主自己管。

**项目层的 `$VAR` 只从 server 自己的 `env` 块取。** 项目 `.agix/settings.json`(以及
`settings.local.json`)里声明或改写过的 server——`mcp.addServer` / `mcp.updateServer` 以 `tier:"project"`(缺省)写的
就是这一层,其中 `headers` 与 `env` 落在 `settings.local.json`——`command` / `args` / `url` / `headers` 里的 `$VAR` 只用它自己 `env` 块里的值
展开,`env` 块的值本身不展开,一律不读 daemon 的进程环境;它的 `oauth` 也不能用
`client_secret_env` / `private_key_jwt.key_env`(读的是 daemon 的环境)、
`private_key_jwt.key_file`(读本机的私钥文件)和 `"grant": "enterprise"`(花宿主交来的企业身份),
写了就以 `mcp_connect_failed` 拒绝连接。全局层、场景层和插件声明的 server 照旧用 daemon 的环境展开。
这条对已经信任的项目同样成立:克隆来的仓库不应该能把 `$GITHUB_TOKEN` 发到它指定的 URL。
项目层只改了全局 server 的一部分(比如只换了 `url`)也算项目层的 server。未信任的项目,
它自己的 server 根本不启动(§6.6)。

**server 向用户的询问。** 连接按 2026-07-28 协商(`server/discover`),server 答不上来就回落到
2025-11-25 的 `initialize`。server 在工具调用中要问用户时有两条来路,出口是同一个:

- 2026-07-28 的 server 把 `tools/call` 答成 `input_required`。`mcp::elicit::call_with_rounds`
  自己驱动轮次(不用 rmcp 内置的重试,那条路只会拿连接级的 handler 作答),每个询问经这次调用的
  `McpCallScope` 交给它的 `McpElicitor`,答完带着 `input_responses` 再发下一轮;一轮里的几个
  询问一个一个问。工具时限按轮算。
- 老版本的 server 自己发 `elicitation/create`,落在连接级的 `NotifyingHandler` 上,而连接是几个
  会话共用的。`ElicitRoute` 记着这条连接上在途的调用,**恰好一个**时交给它,否则回 `cancel`:
  猜错了就是把一个会话的 server 的问题拿去问另一个会话的宿主。

`McpElicitor` 的实现是适配器里的 `ToolCallElicitor`:没人值守回 `cancel`;否则以
`ElicitKind::McpServer` 经这次调用的 `Elicitation` 问出去(子 Agent 的就是父会话的那个),
`Elicitation` / `ElicitationResult` 两个 hook 由运行时给 `Elicitation` 包的一层
(`runtime::elicitation_hooks`)触发,所以不管经哪个 `Elicitation` 问都会经过它们。daemon 上它推成
`prompt_type:"mcp_elicitation"` 的帧,宿主没在 `prompt_types` 里列它就回 `cancel`。

server 写的 `message`、`requested_schema`、`url` 都是不受信的:`url` 只有 `https` 和本机的 `http`
推给宿主,别的回 `decline`;Agix 从不打开它。宿主的 form 答案在 daemon 收下时
(`respondToPrompt`)按 `requested_schema` 校验,不合就以 `INVALID_PARAMS` 退回、提问不撤 ——
只有那里还能叫宿主重答。`decline` 只表示人或 hook 明确说了不,其余没得到回答的都是 `cancel`。
答案只回给 server,不进转录、不进遥测。

**server 把调用转成任务。** 客户端声明 Tasks 扩展(SEP-2663,协议里的 `mcp_tasks` 能力);server 回任务时调用在原地等:登记成
`mcp_task`(只经 `TaskLease`)、轮询 `tasks/get`、任务里的询问走同一个 `McpElicitor`,完成的结果就是
这次调用的结果。`tasks/cancel` 只从调用 future 的 `Drop` 发出(为什么见 `crates/mcp/src/tasks.rs`
文件头)。

### 6.5 MCP 授权

远端 MCP server 要求 OAuth 时(MCP Authorization 规范 2026-07-28),登录由宿主驱动,令牌由
daemon 管。设计与用法在 [`mcp_auth.md`](mcp_auth.md);这里只记它在整体里的位置:

- **每个会话池一个 `mcp::oauth::McpAuth`**:各 server 的授权状态、进行中的登录、凭据存储
  (默认 `scenes/<id>/mcp/oauth/`,一个 server 一个 0600 文件;宿主可经
  `Assembly::mcp.credential_store` 换成自己的)。同一进程里的两个池互不相见。
- **连接时**,能用 OAuth 的 server(`streamable_http` / `sse`,没配固定 `Authorization` 头)
  经 rmcp 的 `AuthClient` 发请求:有令牌就带上,快到期先刷新,被 401 拒了先刷新再重试一次;
  没有令牌就裸发——不要求授权的 server 照旧能用。
- **要求登录的 server 不算连接失败**:它不重试,以 `auth_required` 留在这个项目的 MCP 管理器
  里,不提供工具;模型在系统提示里看到一句"这个 server 要用户先登录"。
- **状态变化回到会话池**:`McpAuth` 的监听器发 `mcp_auth_*` 通知;令牌失效时会话池把这个
  server 从所有缓存的连接里摘下,登录完成后只重连它,受影响的会话在下一轮之前重建。
- **MCP 策略在授权之前**:被 §6.4 拒绝的 server 不连接,也就走不到 OAuth;授权服务器在任何
  跳转之前对照 `allowed_authorization_servers`。
- **回调先过 `state` 再过 `iss`**(RFC 9207,rmcp 的 `handle_callback_url` 校验):`iss` 不是发现时的
  issuer,或元数据声明了会带却没带,在换令牌之前拒掉,登录记为 `failed`,什么也不存。
- **无人值守的 server**(`client_credentials`、企业托管授权)不经过登录:令牌在每次请求之前
  检查,快到期或被拒时重新取。企业身份由宿主经 `mcp.auth.setIdentity` 交来,只在内存里。
- **地址过滤与代理**:OAuth 请求与非回环 server 的 MCP 连接都过 `mcp::oauth::http` 的地址
  过滤,判定与模型发起的请求共用 `is_internal_address`。配了 `HTTP(S)_PROXY` 时代理替我们解析
  DNS,过滤只看得到主机名与 IP 字面量(`mcp_auth.md` §7)。
- **工具名的 server 段在一个管理器里唯一**:`mcp__<段>__<工具>` 的段由 server 名确定地造出
  (`mcp::naming`),本身合规的名字原样用,别的清洗后接哈希,两个不同的名字可能落到同一段
  (`my_srv_<哈希>` 与 `my.srv`)。连接前查重:同时配置的几个撞了,一个都不连,`mcp.status`
  里都是 `failed`,`error` 点名撞上的那个;后来经 `mcp.addServer` 加进来的撞上已有的,只拒
  新来的。不给撞的加哈希,是因为那会改掉一个合规名字的工具名,写好的权限规则跟着失效;都拒
  而不是挑一个留下,是因为两边谁是"本来的"无从判断,留错一个就让权限规则落到别人的工具上。

### 6.6 项目信任

项目层——`<项目>/.agix/settings.json` 与 `settings.local.json`、`.agix/agents/`、
`.agix/hooks/`、`.agents/skills/`——是仓库内容,克隆来的仓库里就是作者写的东西。§6.1 到
§6.4 的那些防线都假设项目可能是敌意的;没有信任这一层,同一个仓库只要在自己的设置里写
`permissions.mode: "bypassPermissions"`、一条 hook 或一个 MCP server,就绕过了它们。

**未信任的项目层按插件级信任处理**——复用插件包已有的那条"网上来的东西只能新增、只能收紧"
的规则(`plugin_packages.md` §信任),不另造一套权限体系。

**这是白名单,不是扣一份名单。** 设置键的性质都在一张键表里:`settings::keys::KEYS` 每行一个点分路径
(`*` 代一个 map 键),列是作用域(`Scope`:进程、`(项目, 场景)` 对、会话,§3.2.1)、信任(`Trust`)、
是否只在启动时读(`read_at_start`,`restart_required` 按它报)、哪些层能写(`Writers`:`app` 是
`GlobalOnly`,加载器把它从场景层、项目层的文件里丢掉,`config.set` 也只许写全局层)。每一列各自按最长前缀取:
一个键取它开头的、设了这一列的最长那行。信任是三种之一:`Free`(照常)、`Tighten(次序)`(只在比下面几层合
出来的值一样紧或更紧时保留,次序按键的类型:布尔取真或取假为紧、数值取小为紧、列表取子集或超集为紧、模式与
网络模式按宽松度排……)、`Withheld`(整个扣下)。**没有信任的键一律扣下**,并记入 `requires_trust`;比它管的
键还短的行,信任只能是 `Withheld`。模块自己的设置段(`team`、`plugins`、`scripts`、`checkpoints`)由定义它的
crate 在 `Section::KEYS` 里声明自己的行(路径相对段名,`""` 是段本身),加载时并进同一张表;编译掉的模块
(`SectionSpec::inert`)整段 `Withheld`。新加一个设置字段忘了归类,`settings` 与 `assembly` 里从
`settings_json_schema()` 枚举叶子路径的测试(`every_settings_key_has_a_row`,调 `keys::key_gaps`)就过不了:
每个叶子都要有作用域与信任,每行都要对上一个键——白名单因此由构造保证,而不是靠人记得。`permissions` 的规则
列表在表里是 `Free`,因为 `allow` 与 `allow_exact` 由加载器自己丢弃。

| 项目层内容 | 未信任时 |
|---|---|
| `permissions.mode` | 只能比它下面几层合出来的更严 |
| `permissions.allow`、`permissions.allow_exact`、`permissions.ask`、`permissions.deny` | `deny` / `ask` 照常,`allow` 与 `allow_exact` 丢弃(含 `settings.local.json` 里"永久允许,本项目"记下的) |
| `hooks`、`scripts` | 整段忽略;`.agix/hooks/` 也不放进 hook 命令的 `PATH`,继承的 `PATH` 里的相对条目去掉,用户层 hook 写的裸脚本名不会落到项目里的同名文件;用户层 command hook 以相对路径起的程序(`./scripts/check.sh`)、或命令里哪个相对路径的词正是项目里的文件或目录(`python3 scripts/check.py`、`node .`、`bash x.sh`、`< x.sh`)的,不跑,算 hook 失败——工作目录仍是项目,那个路径指的是仓库里的文件(`hooks.md`) |
| `mcp.servers.<名>` | 不启动;`mcp.status` 里是 `"state": "untrusted"`;`mcp.addServer` / `mcp.updateServer` 以 `PROJECT_UNTRUSTED` 拒绝,什么都不写 |
| `mcp.servers.<名>.enabled` | 只能关(`false`),打开下面几层关掉的(`true`)丢弃;`mcp.setEnabled` 在未信任项目里打开以 `PROJECT_UNTRUSTED` 拒绝 |
| `permissions.client_may_loosen: true`、`skills.allow_shell: true` | 比下面几层松就丢弃 |
| `sandbox.enabled: false`,`sandbox.require` / `sandbox.unattended_needs_network_policy` / `sandbox.reap_on_close` 设为假,`sandbox.network.mode` 更松 | 丢弃 |
| `files.extra_dirs` / `files.read_dirs`(含 `"/"`)/ `files.allow_write` / `web.allowed_domains`,`sandbox.allow_read`,`sandbox.cache_dirs` 的某一组,`sandbox.env.allow` | 只要多出下面几层没有的条目就整项丢弃;只删不加的保留 |
| `files.deny_write`、`sandbox.deny_read`、`sandbox.env.mode` | 少了下面几层有的条目(或从 `allowlist` 改回 `inherit`)就丢弃;多写的保留(`deny_read` 本来就是追加到内建的凭据清单上) |
| `sandbox.network.proxy` | 丢弃:它决定沙箱里的命令能连到哪个地址 |
| `providers` | 整段不要:改一个已有 provider 的 `base_url`,用户全局层的密钥就会跟着发过去;新 provider 的 `api_key_env` 能把 daemon 的任意环境变量当 key 发出去 |
| `model.order` | 只能排下面几层已经定义的 provider,排了项目自己的 provider 就丢弃;`model.tasks.<任务>` 只写档位,照常 |
| `telemetry.enabled: false` | 丢弃:项目不能关掉审计记录 |
| `telemetry.otlp_endpoint`、`recorder` | 丢弃:一个把遥测送到项目指定的地址,一个把宿主请求的录像写到项目指定的目录 |
| `prompt.override` | 丢弃:它整个替换系统提示词(追加用的 `prompt.append` 照常) |
| `turn`、`session`、`goal`、`agents` 的各项上限(`turn.max_model_calls`、`turn.token_budget`、`turn.parallel_tools`、`session.token_budget`、`goal.max_rounds`、`schedule.max_model_jobs`、`agents.max_depth`、`agents.max_concurrent`);`schedule.min_model_interval_minutes` 是下限,只能更大 | 只能比下面几层更小;`permissions.prompt_timeout_secs` 只能更小且不能是 0(0 是不限);`team.enabled` 只能设为假 |
| `prompt.instruction_files` | 丢弃:它按路径直接读进系统提示词,不过沙箱也不过 Read 的 `deny_read` |
| `compaction.inline_result_chars` | 丢弃:项目不能改大结果外置的阈值 |
| `app` | 整块丢弃:信任、历史位置、代理、worktree 目录都只认全局层 |
| `plugins` | `enabled: true`(下面几层是 `false`)、`enable_on_install: true` 丢弃;`allow` 放进下面几层没有的名字、或清空下面几层的清单,丢弃;只收窄的保留 |
| `.agix/agents/*.md` | 来源是 `AgentTypeSource::Project { trusted: false }`:`permission_mode` / `max_turns` 只能收紧,与内建、插件、用户层同名的不替换它们 |
| `.agents/skills/` 里的 `` !`cmd` `` | 不执行,换成一行说明(与 `skills.allow_shell: false` 同一条路) |
| `.agents/skills/` 里的 `allowed-tools` | 什么都不授予:模型调用这个 skill 之后,它列的工具照常询问 |
| 其余(`prompt.append`、`rules`、记忆、`prompt.language`、`compaction` 的其余项、`model.tasks`、在下面几层已有的 provider 之间选择……) | 照常 |

文本到达模型本来就要靠人读(插件同一判断),所以提示词一类照常;决定对话、凭据、遥测送到哪里,
或者把用户的哪个文件读进提示词的键,按上表扣下。全局层、场景层里写的脚本绑定若是相对路径,
在未信任的项目里不绑定——它会按项目根解析,读到仓库放在那里的文件。stdio MCP server 不论项目
信任与否,都以声明它的那一层的目录为工作目录启动:全局层的在全局配置目录、场景层的在场景配置
目录、插件的在插件包根目录、项目自己的在项目根;相对的 `command` 与参数里的相对路径都在那里解析,
从不落到 daemon 的工作目录(它可能是任何一个项目)。哪一层算"声明它的",看哪一层给了它的
`command`:项目层只补了 `env` 的全局 server 仍在全局目录启动。信任之后项目层恢复全部行为。

一个值类型不对(`prompt.language: 5`、`sandbox.network.mode: 42`)时,`LoadedSettings::load` 只丢掉这个值并
warn,`daemon.doctor` 的 `settings.tiers` 报 `degraded`、在那一层的 `ignored_values` 里列出;
同一个文件里其余的值和别的层照常。键是被丢的那个值自己的键:对象里的值都不对时逐个报,这里根本放不下
对象时才报这个对象本身。只能由程序设的 `permissions.mode`(`bubble`)也这样丢掉并列出。一处拼错不会让全局层的 hook、
沙箱设置跟着失效。未受信的项目层例外:受限的键写成不对的类型(`mcp.servers.<名>.enabled: null`)按放宽算,
在 `requires_trust` 里列出而不是进 `ignored_values`。`permissions` 的规则串逐条读:一条写错的规则只丢它
自己(`ignored_values` 里的键是 `permissions.allow[0]` 这样带下标),同一层的 deny 照常生效。

**信任是项目根的属性,存在全局层,项目自己改不了**:

```jsonc
// ~/.agix/settings.json 或 settings.local.json
{ "app": {
    "trust_all_projects": false,                // 缺省 false:没有哪条 trusted_projects 点名的项目不信任
    "trusted_projects": ["/Users/x/work/**", "/srv/ipd/lines/*"] } }
```

判定:没有自己的项目层(无项目会话,或项目根就是放全局 / 场景目录的那个目录,如 `$HOME`)→ 不涉及信任;
项目根是别的目录、它的 `.agix` 却解析到全局 / 场景目录(仓库里提交一个指过去的符号链接)→ 设置文件
就是全局 / 场景那一份、只读一次,但仓库仍是别人写的:按未信任处理,`trust_all_projects` 不起作用,只有
`trusted_projects` 里命中的一条能信任它;
这样借来的全局 / 场景层不是项目自己的层,`mcp.addServer`、`config.setProvider` 一类要把配置写进"项目层"的
方法拒绝(`INVALID_PARAMS`,说明原因),不会把项目的写入落进对所有项目生效的那一份;
`trusted_projects` 里有一条命中项目根(路径相等,或 glob:`*` 不跨 `/`,`**` 跨,`~/` 是家目录;
符号链接两种写法都认)→ 信任;否则取 `trust_all_projects`。比的是项目根解析之后的真实目录,原样给出的
写法只在不含 `.` / `..` 时也拿来比,所以 `/home/u/work/**` 不会命中
`/home/u/work/../../tmp/x`。`app` 块只从全局层读,场景层、项目层里写的忽略并记一条
warn。全局层 `settings.json` 与 `settings.local.json` 的
`trusted_projects` 清单相加,不是后者盖前者。合并时的判定结果在 `LoadedSettings::trust`、`LoadedSettings::requires_trust`:
信任状态、命中的那条、以及项目层里哪些东西要信任才生效(`requires_trust`;`withheld()` 另带每一条所在的文件)。

`LoadedSettings` 是加载的结果,不是 `Settings` 的别名:`values` 是合并后的值(`Settings`),`paths` 是本场景
数据的位置,`rules` 带层归属的权限规则,`managed` 是部署方的托管层,`ignored` 是被丢掉的值;项目层的
判定结果是私有字段,只经 `trust()` / `requires_trust()` / `withheld()` / `project_mcp_servers()` 读,装配层写不出对它的访问。
`LoadedSettings` 没有 `Deref`:只读值的函数收 `&Settings`,调用方传 `&loaded.values`;持有方读
`loaded.values.x` 与 `loaded.paths.x`。

宿主经 `project.status` 读状态与 `requires_trust`(用来画"信任这个项目吗"的对话框),经
`project.trust` 把项目根加进或移出全局 `settings.local.json` 的 `app.trusted_projects`,之后这个项目的
绑定与 MCP 连接重建,发 `project_trust_changed` 通知。`project.trust` 只收绝对路径,记下的是
解析之后的真实目录,其中的 glob 元字符(`*`、`?`、`[` 等)转义掉——`Project [v2]` 这一条只
命中它自己。往未信任项目的项目层里写配置的方法(`tier:"project"` 的 `mcp.addServer` / `mcp.updateServer`、
`config.setProvider`)以 `PROJECT_UNTRUSTED` 拒绝,什么都不写:写进去也不会生效;`config.set` 与
`mcp.setEnabled` 逐键判,只拒会被扣下的键。它们的 `tier:"global"` / `"scene"` 写用户自己的层,不看项目信任。
`session.create` / `session.resume` 的结果带 `project_trust` 与 `trust_source`(`default`:由
`app.trust_all_projects` 决定;`user`:全局 `settings.json` 里的条目;`host`:全局 `settings.local.json` 里的条目,
`project.trust` 写在这里),`project.status` 也带 `trust_source`,宿主不必另查。
`daemon.doctor` 的 `project.trust` 检查报 daemon 自己的项目:未信任且有东西被扣下,或只靠
`trust_all_projects: true` 才信任而有东西要信任才生效,都是 `degraded`,并列出这些键。确认对话框是宿主的事;无人值守部署用 `trusted_projects` glob 预先信任
它们自己创建的项目目录。

**缺省 `untrusted`**:克隆来的仓库在宿主经 `project.trust`、或用户在全局层 `app.trusted_projects` 里确认之前,
项目层只能收紧——项目的 hook、MCP server、可放宽的 agent 类型、skill 的 `` !`cmd` `` 与
`allowed-tools`、放宽的 `permissions.mode` 与沙箱都不生效。要信任所有项目,在全局层写
`"app": {"trust_all_projects": true}`;`daemon.doctor` 会把因此生效的东西列出来。

按键写设置只有一条路径:`config.set`、`config.setProvider`、`mcp.addServer` / `updateServer` 都翻成
`settings::edit` 的改动(键路径上的设值、删除、合并;凭据字段——`settings::edit::SECRET_PATHS`——只落
`settings.local.json`),由池里的 `write_settings_tier` 在那一层的锁下读出两个文件、套用、把写成的样子交给
加载器合并整对设置、逐键判拒绝(加载器的 `ignored` 与 `withheld()` 里落在这次写的文件、这个键上的条目),通过才落盘;写完让监视器认领自己写下
的内容,再走与监视器相同的"这一层变了"(`settings_tier_changed`),不会被监视器再重载一遍。

引擎自己改设置文件的地方——`project.trust`、`config.set`、`config.setProvider`、`mcp.addServer` /
`updateServer`、权限询问里的"永久允许"——都走 `settings::file`:读改写期间锁住那一层
的目录(进程内一把锁,unix 上跨进程靠目录上的 `flock`),写进同目录的临时文件、落盘之后改名替换,
读的一方只会看到旧文件或新文件。等锁有上限(`base::dir_lock::DIR_LOCK_WAIT`,10 秒):超时 RPC 回 `SETTINGS_LOCKED`、
什么都不写,"永久允许"则只放行当前会话、不落盘并发 notification;记忆目录的锁同一上限。`settings.local.json` 每次写都是 0600,别的文件保留原来的
权限。设置文件是符号链接的,引擎一律不写:项目层里的链接是仓库内容,顺着它写等于让仓库挑一个
用户的文件来改写,把链接换成普通文件又会悄悄拆掉用户自己的 dotfile 链接。读取时项目层(含项目的
`settings.local.json`)的链接不读;全局层与场景层的链接是用户自己(常是 dotfile 管理工具)放的,
解析后落在状态目录或家目录下就读,落在别处不读、记 warning。项目根就是 `$HOME` 时,那一层按全局层算。

---

## 7. 锁

两把锁,都靠 `flock`,进程无论怎么退出内核都会释放:

- **daemon 实例锁** `daemon.lock`:拿到 `flock` 之后,再看文件里记的上一个持有者——不持
  `flock` 的旧版本 daemon 也挡得住。这一步由 `platform::process_identity::decide_stale_lock` 判。
- **会话日志的单写者锁** `sessions/<id>/writer.lock`(§1.3):只有 `flock`,不记持有者。

`decide_stale_lock` 的判定依据是 **pid + 进程启动时间**,不是文件 mtime,也不是超时:

| 存活性 | 决策 | 理由 |
|---|---|---|
| `Alive` | 保留 | 持有者还在 |
| `Unknown` | 保留 | 无法确认时保守——宁可拒绝启动,不可双开 |
| `Reused` | 回收 | pid 被复用,原持有者已死 |
| `Dead` | 回收 | 进程不存在 |

**记启动时间是为了防 pid 复用。** 只比 pid 会把碰巧复用了旧 pid 的无关进程误判为持有者,
从而永久拒绝启动。

---

## 8. 磁盘布局

`~/.agix/` 下按**索引维度**分目录,不按产品名或场景分:

| 目录 | 索引键 | 内容 |
|---|---|---|
| `projects/<sanitized-cwd>/` | 项目 | `<session-id>.jsonl` transcript |
| `projects/_no-project/` | ——(保留) | 无项目会话的 transcript |
| `sessions/<session-id>/` | 会话 | 会话记忆、metadata、输入历史、`writer.lock`(§1.3);放进项目的会话(§8.2)这里只有 `index.json` 与 `writer.lock`;根会话的 `checkpoints/objects/<sha256>`:文件工具写之前的内容(目录 0700、文件 0600,子 Agent 的写也在这里),随会话删除一起删;`checkpoints` 设置段限单文件(`max_bytes_per_file`)与总量(`max_total_bytes`),超总量按最旧清并在转录里记 `pruned` |
| `<项目>/.agix/state/tool-results/<根会话 id>/` | 会话 | 超长工具结果的完整文本(`<写它的会话 id>-<tool_use_id>.txt`,同名已在时加 `-<n>`),子 Agent 的也在这里;无项目的会话在 `<状态目录>/tool-results/`;不在沙箱可写的 `scratchpad/` 下(引擎在沙箱外写它,不能让沙箱内的命令把目录换成符号链接);目录 0700、文件 0600,随 `state/` 不进仓库,带一个内容为 `*` 的 `.ignore` 让 `Grep` / `Glob` 不扫进去;随 `session.delete` 一起删;`session.resume` 清无引用的,`session.fork` 硬链接复制,`session.export` / `session.import` 带过去 |
| `memory/` | 全局 | 跨会话记忆,模型照提示词写在这里;项目级的(抽取出的 `type: project`)在 `<项目>/.agix/state/memory/`。两处只由 `MemoryStore::for_paths` 推出,提示词里的索引是两处合并的;写入按目录加锁(`base::dir_lock`:进程内一把锁,unix 上再对目录 `flock`,别的 daemon / CLI 进程也排队)、先写临时文件再改名;存到 200 条(`MAX_MEMORIES`)就不再抽取新的,不自动删,`daemon.doctor` 的 `memory` 检查报 `degraded`(报的是磁盘上的实数);`Write` 工具直接写进目录的不经这道检查,所以读取时也只读最近的 200 条、索引只列 200 条,多出的打一条 warn |
| `scenes/<id>/` | 场景 | 该场景的设置层(`settings.json`、skills、agents);声明式场景在这里放 `scene.json` + `prompt.md` |
| `scenes/<id>/mcp/oauth/` | MCP server URL | 远端 MCP server 的 OAuth 凭据,一个 server 一个 0600 文件(§6.5) |
| `daemon/groups/<instance>.json` | 实例 | 在跑命令的进程组,供崩溃后下次启动清理;嵌入时 `EmbedConfig.groups_file` 可另给位置;拥有者是另一个活着的进程时 `start` 拒绝(`GroupsHeld`),`shutdown`/drop 删文件并停止登记 |
| `daemon/crashes/` | 进程 | 崩溃记录 |
| `quota/<provider id>.json` | provider | 这个 provider 在当前日、周、月已用的 token(`settings::quota::FileQuotaLedger`);在目录的锁下读改写,同一台机器上的所有实例共用;周期按本机时区,周从周一、月从 1 号 |

项目里的 `<项目>/.agix/state/teams/<team_id>/` 是一个 `TeamCreate` 团队的全部状态:`team.json`、
`state.json`、`events.jsonl`、`SCRATCHPAD.md`、`mailbox/`、`memory/`(团队共享记忆,
格式同个人记忆)。每个阶段开始时成员读到 `memory/` 的索引;`TeamDelete` 删掉整个目录。

项目的 `.agix/` 在用户的仓库里。其中只有协作者共用的东西随仓库提交(`settings.json`、agents、
hooks、commands、rules、output-styles 等配置);引擎为这台机器、这个用户写的都不提交:本机数据与
过程数据全在 `.agix/state/` 下(`Layout::project_state_dir`:`memory/`、`scratchpad/`、`teams/`、
`telemetry/`、`tool-results/`、导入标记 `.imported.json`),个人配置 `settings.local.json` /
`mcp.local.json` 留在 `.agix/` 里、与 `settings.json` 并列。有项目的会话建起来时,以及引擎往一个
设置目录写 `settings.local.json` / `mcp.local.json`(里面是 key 与 MCP headers)、往项目
`state/telemetry/` 录遥测、写导入标记时,往那个目录的 `.gitignore` 里补上缺的引擎条目
(`base::paths::local_data_gitignore`:`state/`、`settings.local.json`、`mcp.local.json`),没有就新建。
已有的 `.gitignore` 是用户的,只追加缺的行,别的不动;它是符号链接时不动,也不动它指向的文件。会话建起来与
写导入标记时,项目层目录本身是符号链接或落在项目之外,也不写(`ensure_project_data_gitignore`)——克隆来的
仓库能把它们指到别处。状态目录与场景目录写 `settings.local.json`
时同样会生成,无害。`scratchpad/` 自己另有一份 `*`。引擎往项目里新加本地的东西放进 `state/`,
`.gitignore` 不加行。没有项目的会话没有 `state/` 这一层,本机数据就在状态目录下
(`ConfigPaths::local_state_dir`)。

`state/<产品名的小写>/` 归嵌入方(`Layout::host_state_dir`,如 `.codix/state/codix/`):引擎不读不写,
与 `state/` 的其余部分一样受保护(§8.1)。产品名小写后不能是引擎在 `state/` 下用的名字
(`STATE_RESERVED_NAMES`,其中 `history` 是按项目存放的会话历史,§8.2),`Layout::with_product_name` 在启动时拒。

**旧位置。** 读一个项目的设置就是打开它(`settings_source::read_pair`,在一切用到本机数据之前):直接放在
`.agix/` 下的 `PROJECT_STATE_ENTRIES`(上面那六项)一次性改名进 `state/`,之后只读新位置。项目层目录是
符号链接、落在项目之外,或 `state/` 不是自己的目录时什么都不挪;旧项是符号链接的不挪;`state/` 里已有同名的,
旧的留着、不再读(记一条 info)。挪不动的记 warn,不影响打开。`.gitignore` 里旧的条目不替人删。
项目层目录就是状态根或场景目录、包含它或落在它里面时也不挪(在 `$HOME` 里启动,`~/.agix` 既是项目层又是
状态根):那里直下的记忆、scratchpad、tool-results 是无项目会话的,不是旧引擎留下的项目数据。这种项目的
`state/` 照样是它自己的本机数据,与状态根直下的那份各归各。

**会话进哪个项目目录由它自己的 `Meta.project_root` 决定**,不是由 daemon 的 cwd
决定 —— 一个 daemon 同时服务多个项目时,后者会把所有项目的 transcript 堆进一个
目录。侧链的 `Meta` 记下父会话的项目(`project_root`),按它进项目目录;没记的(父会话无项目,
或字段出现之前写的日志)跟父会话进同一个目录(级联删除按那里找)。

sidecar 不能塞进 `projects/<cwd>/`,因为按 session id 查找时没有项目信息可用。
两者是并列的两个维度,不是两层嵌套。**反过来,"这个会话属于哪个项目"也只有
sidecar 答得出**:`Meta` 在 transcript 里,而要读 transcript 先得知道它在哪个项目
目录下。所以 `Meta` 落盘前先写 sidecar,顺序与计数器那类"从已有日志算出来的"
附带信息相反。

文件 checkpoint 的副本放在 sidecar 而不是项目的 `.agix/` 里:那是用户的仓库,快照里是项目文件
的历史内容,不该出现在仓库目录树里,也不该进 `git status`。按会话 id 找得到,随 `session.delete`
一起删;不跨会话去重,所以删一个会话就是删一个目录。分叉不带它们(分叉和源会话共用工作区)。
`session.rewind` 的 `force` 覆盖了会话之外对文件的改动时,在 `rewound` 帧与结果的 `overwrote_external` 里列出;被配额清掉的轮次文件回不去(`session.checkpoints` 的 `pruned_before_turn`)。

区分两类目录靠**目录名能否解析成 `SessionId`**:sanitize 后的 cwd 必然以 `-` 开头
(绝对路径首字符是分隔符),`_no-project` 以 `_` 开头,都解析不成 base58 id。

### 8.1 引擎目录与布局

上表里的 `~/.agix` 是状态根,项目里的 `.agix` 是**项目层目录**;嵌入方可以给后者另起名字
(`EmbedConfig.project_dir`,如 `.codix`,`docs/agix-sdk.md` §10)。名字只有一个来源,派生路径与
安全判定各只有一个出口,代码里不写目录名:

| 类型(`base::paths`) | 管什么 | 谁持有 |
|---|---|---|
| `Layout` | 项目层目录名(缺省 `.agix`,`Layout::new` 校验)与 worktree 目录名(缺省 `.wkts`,`Layout::with_worktrees_dir` 校验);由名字算出、不碰磁盘的东西:`project_dir(root)`、`project_state_dir(root)`、`host_state_dir(root)`、`worktrees_dir`、`teams_dir`、仓库 `.gitignore` 条目、配置根(`config_roots`)、worktree 别名(`main_checkout_copies`、`worktree_of`) | 按值传:`EmbedConfig` → `StaticDaemonPaths::with_layout` → `DaemonPaths::layout()` → `settings_source::read_pair` → `LoadedSettings::load_from`(按全局 / 场景层的 `worktree_dir` 定 worktree 目录)→ `PathSettings.layout`,再到 `EngineConfig`、`PermissionSettings`、`SandboxPolicy`、`SensitivePaths`。没有进程级的值:一个进程里两个池可以用两个名字,一个池里两个会话也可以有两个 worktree 目录 |
| `ConfigPaths` | 一个会话的三层根(状态根、场景目录、项目)与其下全部派生路径:各层的 `settings*.json`、skills / agents / hooks / rules / output-styles、本机数据的目录(`local_state_dir`:有项目是项目层的 `state/`,没有是状态根)与其中的 memory、telemetry、`tool-results`、scratchpad、导入标记、`scheduled_tasks.json`、`mcp/oauth` | 会话的 `PathSettings::config_paths()`;池里没有会话时 `DaemonPaths::paths_for` |
| `StateRoot` | 状态根下不随项目层名字变的目录:`scenes/`、`plugins/`、`projects/`、`sessions/`、`daemon/`(`instances.d`、`auth`、`groups`、`crashes`) | 池与 agixd 的启动代码 |
| `SensitivePaths` | **唯一的安全判定点**:凭据(`is_credential`)、拒写的引擎设置文件(`is_engine_settings_file`)、不走静默放行的配置(`names_configuration`)、沙箱要保护的目录(`config_roots_in`、`engine_dirs_in`、`worktree_dirs_in`)、提示词读文件(`PromptFiles`) | 会话的 `SandboxSettings.paths`;门禁、写路径检查、规则别名、两个沙箱后端、hook 执行器、rewind、记忆、插件包的路径解析都问它,不各认目录名 |

文件名与子目录名(`settings.json`、`hooks`、`rules`…)是 `base::paths::entry` 的常量,只在
`layout.rs` 里写一次;守卫 `tests/guards/tests/engine_dirs.rs` 不许生产代码与提示词模板里再出现
`.agix` / `.claude` / `.wkts` 路径分量、引擎文件名或 `.join("<子目录>")` 字面量。提示词与文案里的目录按实例的
`Layout` 渲染。

**大小写。** macOS 缺省的 APFS 不分大小写,`.CODIX/Settings.json` 就是 `.codix/settings.json`。`SensitivePaths`
与 `Layout::main_checkout_copies` 的名字判定一律不分大小写(项目层目录名本身只许小写),写路径检查的
文件名黑名单也一样。"不分大小写"按 APFS 的完整大小写折叠比(`base::paths::fold_case`),不是 `to_lowercase`:
APFS 把 `ſ` 当 `s`、`ß` 当 `ss`、`ﬁ` 当 `fi`,`.agentſ/ſkills` 就是 `.agents/skills`。
已存在的路径工具会先解析成磁盘上的真实名字,还不存在的那一段没有真实名字可取,只能这样比。代价是区分大小写的文件系统上 `.AGIX/settings.json` 也按引擎设置拒写。沙箱后端不用改:
Seatbelt 在不分大小写的卷上按卷的语义匹配规则(实测,`macos_live` 有用例),Linux 常见的文件系统区分大小写。

**外来配置根。** 配置根是本实例的项目层目录,加上 `FOREIGN_CONFIG_ROOTS`(`.agix`、`.claude`)里不是它的
那些。名字改成 `.codix` 后,`.agix` 不再被读,但与 `.claude` 一样受保护:写工具拒写其中的设置文件,写其中别的
东西要问(门禁不走静默放行);沙箱里的命令整个写不了它(`ConfigRootAt::shut_to_writes`,与门禁同一个判定;
策略点名的可写根在其中时照旧可写;Linux 上它不存在时挂一层空的只读 tmpfs,不让命令建出它,命令的命名空间
没了(bwrap 随命令退出,它的 PID 1 要等命令留在后台的进程都结束;被停的命令要等 `SIGKILL`)、项目里也没有别的
沙箱命令的命名空间时再把 bwrap 留下的空挂载点删掉,项目里不留痕迹——删早了,还活着的那个命名空间里的进程就能
建出它);其中的 `settings.local.json` / `mcp.local.json` 仍算凭据。理由有二:漏改的读配置点仍读 `.agix/hooks` 时,保护把"模型给自己装 hook"降成
"功能读错目录";同一项目也可能被缺省名字的 agixd 打开,改 `.agix/settings.json` 等于给它的下一个会话授权。
门禁上只有本实例自己的目录有工作区豁免:项目层目录里是 `state/scratchpad/`、`state/memory/`,会话的状态根里
是直下的 `scratchpad/`、`memory/`,写入不问;外来根里一切都算配置。直接放在项目层目录下的 `scratchpad/`、
`memory/` 不是工作区。沙箱与门禁用同一个判定(`ConfigRootAt::writable_state_entry`):本实例目录里沙箱命令
也只写得了 `state/` 的这两项;`state/` 本身与其中的 `teams/`、`telemetry/`、`tool-results/`、`history/`、
导入标记由引擎自己(不在沙箱里)写,`state/<产品名的小写>/` 归嵌入方,写工具写它们要问,沙箱命令写不了;
引擎从中导出、分叉、恢复的 `tool-results/`、`history/`(`PROJECT_STATE_ENGINE_DIRS`)与嵌入方的目录,
策略点名的可写根在 `state/` 里时 macOS 上也写不了。门禁按文本认状态根,命令里写成 `~/.agix/memory`
或 `$HOME/...` 的认不出,按配置要问。
缺省名字下自己与外来去重,行为与改名之前相同。

**worktree 目录。** 引擎建的 worktree 在仓库根的 `<worktree 目录>/<名>`,缺省 `.wkts`;第一次建
worktree 时往仓库根的 `.gitignore` 加的只有 `<worktree 目录>/` 这一行(它是符号链接时不加),不是项目层目录——项目层的
`settings.json`、agents、rules、hooks 与嵌入方放在那里的文件照常能提交。名字由设置的 `app.worktree_dir`
改,只从全局层与场景层读:项目自己的设置文件改不了它(加载时丢掉并告警,`config.set` 写项目层以
`not_at_tier` 拒绝),克隆来的仓库不能把引擎的 worktree 引到别处。名字是一个以 `.` 开头的路径分量,
不能是配置根或别的工具的目录(`with_worktrees_dir`,不分大小写);不合法的值丢掉、记进
`LoadedSettings::ignored`,用下面一层的或缺省的。认 worktree 的地方(规则别名、rewind、hook 执行器、
恢复侧链、沙箱)只认两个名字:会话当前的目录与缺省的 `.wkts`。从缺省改成别的名字,`.wkts` 下已有的
worktree 照样受保护;从一个自定义名字改成另一个,旧名字下的 worktree 不再被认作引擎的(不受保护,也不算
主检出的别名),改名之前先把它们删掉或 `git worktree move` 到新目录。它在项目
空间里,不在受保护的配置根里,所以沙箱另外把它钉住:macOS 上目录本身和其中直属的每个 worktree
都不许被沙箱里的命令改名、删除或新建;Linux 上它只读、已有的 worktree 再绑回可写,还不存在时与不存在的
外来配置根一样挂一层空的只读 tmpfs,命令建不出它,也就写不了引擎之后在其中建的第一个 worktree,事后删掉
空挂载点。引擎自己的 git(只写点名的可写根)不受这些限制。引擎建 worktree 前它是符号链接就拒绝。项目层目录里的 `worktrees/` 不是 worktree 目录,是普通的配置。

**外来状态根。** 状态根不是 `~/.agix` 的实例(嵌入方的 `data_root`,或设了 `AGIX_CONFIG_HOME`)
把 `~/.agix` 与 `~/.<引擎名>`(`known_state_roots(&layout)`,`Layout::state_dir_names`)当外来状态根,与自己的状态根一样保护:其中的设置文件、
`mcp/oauth/`、`daemon/auth/` 算凭据(工具在任何模式下都拒读拒写,沙箱里读不到),沙箱里的命令写不了
其中任何东西(工作目录在它里面时除外),也连不上其中的 unix socket——否则一个 Codix 会话的命令能拿
agixd 的令牌、连上它的 socket 当宿主。本机别的实例的状态根没有地方登记(`instances.d` 只记同一状态根
下的 daemon),所以只认这两个:agixd 缺省的,和同名引擎的实例缺省会用的。

**已知边界。** 两个都不用缺省名的宿主(`.codix` 与 `.foo`)开同一个项目时,彼此不保护对方的目录;需要时
再加一份要保护的名字表。`.agents/skills` 是跨工具约定,不随项目层目录名变。

**引擎名。** `Layout` 里还有引擎名(缺省 `agix`,`EmbedConfig.engine`,`Layout::with_engine` 校验:
`[a-z][a-z0-9]*`、至多 16 个字符、能当脚本的新全局——`base::interface::script::check_global_name`,即
`js_lib::check_global_name`)。它是三处 JS 载体里基础库唯一的全局名(`js_lib::install_named`,
`ptc::Library::for_engine`,`script_host::bindings::engine_for`)、基础库错误名 `<Engine>Error`、`RunCode`
说明里的 `<engine>.*`、项目层目录的缺省(`with_engine` 同时把它设成 `.<engine>`,`EmbedConfig.project_dir`
给了就再 `with_project_dir`)和上面那个外来状态根 `~/.<engine>`。校验放在 base 而不是 js-lib,是因为没有
JavaScript 引擎的构建也要在 `start` 时拒掉非法名字;js-lib 依赖 base 取同一份规则。`agix.*` RPC 命名空间、
`AGIX_*`、transcript 命名空间、OTel scope 与指标名、agixd 与 crate 名不随它。

**产品名。** `Layout` 里的另一个值是产品名(缺省 `Agix`,`EmbedConfig.product`,`Layout::with_product_name`
校验:ASCII 字母开头,其后至多 31 个字母、数字、`-`、`_`),与目录名同一条路传、同样没有进程级的值。展示用的
`product_name()` 进提示词(场景的身份句、`ScenePromptContext.layout`)、工具指南(`PromptContext.layout`)、报错、
MCP 动态注册的 `client_name`、桌面通知、内建场景的展示名(`<名> Code` / `<名> Mini`,`scene::builtin` 与
`SceneRegistry::register_builtin` 按 `Layout` 起名,场景 id 不变);小写的 `product_id()` 派生出 `user_agent()`(`<id>/<版本>`)、
`worktree_branch_prefix()`(`<id>/worktree-`)、`quarantine_prefix()`(`.git.<id>-quarantine-`)、OpenTelemetry 的 `service.name`(标识服务,所以是宿主;
instrumentation scope 与 `agix.*` 指标名标识库,仍是 agix;导出器是进程级的,第一个起它的池的名字生效)与
`machine_dir()`。`User-Agent` 不放在进程共用的 reqwest 客户端上,而是每个 `LocalNetwork` 一个、请求没带才补上:
会话的出口(`runtime::exec`)、hook、插件包下载、`config.listModels`,以及经 `ModelFactoryRegistry::with_network`
交给模型工厂的出口都从池的 `Layout` 建;MCP OAuth 自己的客户端从 `McpAuth::set_user_agent` 取。守卫
`engine_dirs.rs` 的 NAME-1/2/3 不许生产代码的字符串、提示词模板和工具输入类型的文档注释里出现展示用的产品名
(协议的 `agix.*`、`AGIX_*`、内部 `agix-` 标识不算),例外逐条列明理由。

**managed 层随产品名走,不放宽。** 平台路径只有一处:`Layout::machine_dir()`(macOS
`/Library/Application Support/<名>/`,Windows `%ProgramData%\<名>\`,其余 `/etc/<id>/`;读 `ProgramData` 是 base
里带 `#[expect]` 的机器事实),`managed_settings_file()` / `mcp_policy_file()` 在它下面,settings 与 mcp 都从这里
取。只拼本产品的目录,改了名字就不再看 `Agix` 那一份,也不会读到任何别的路径:名字的字符集里没有路径分隔符和 `.`,拼不出
`..` 或绝对路径。那里没有文件,与缺省名字下没有 managed 文件是同一种状态——没有 managed 层,不是一个出错的层,
门禁照常按用户、项目各层判断;文件在但读不了或不合法仍按 §6.1 拒绝一切工具调用,MCP 策略文件读不了仍拒绝一切
server。宿主点名的 `managed_settings` / `mcp_policy` 与产品名无关,必须存在。所以改名不会让一份本该生效的
managed 文件被悄悄跳过而变宽,除非部署方把它放在了旧名字下——这一点写在 `agix-sdk.md` 的迁移说明里。

### 8.2 会话历史放进项目(`app.history`)

全局设置 `app.history`(只认全局层、`cli` 层与 managed 层,场景层、项目层写了忽略并告警;嵌入时
写在 `EmbedConfig.settings`,`Assembly.history` 优先)缺省 `global`,布局就是上表。设成 `project` 后,**受信项目**的新会话
整份放进项目:`<主仓库>/.agix/state/history/<id>/` 下是 `<id>.jsonl`、`metadata.json`、
`session_memory.md`、输入历史与 `checkpoints/`,文件名与全局 `sessions/<id>/` 里的相同;worktree 里
的会话放进主仓库的这个目录(`Layout::main_checkout_copies`)。全局 `sessions/<id>/` 只留
`index.json`(`history::placement::SessionIndex`:项目根、会话目录,父会话与会话种类,项目搬过或拼写不同时另记日志里
的那个根)和 `writer.lock`——锁在追加知道会话去哪之前就要拿到。索引先于 sidecar 与转录落盘,理由同上:
它说明日志在哪。不受信的项目、无项目会话照旧全局;侧链与同一项目的分叉跟父会话放在一处,一条谱系总在一个目录里;导入到别的项目的分叉按它自己的项目算。
`state/history` 和 `state/` 的其余部分一样受保护(§8.1),模型写不进去。项目层目录、`state/`、
`state/history` 任何一级是符号链接、不是目录或解析到项目外,新会话就放全局。`state/history` 里引擎
自己写一份 `.gitignore` 与 `.ignore`(内容都是 `*`,不跟随符号链接):前者让上层仓库的取反规则管不到
里面,后者让 Grep/Glob 不扫历史;写不成也放全局。

**一个解析点。** 会话的文件在哪只由 `JsonlHistoryStore` 内部的 `Homes::locate` 回答:先看全局
索引,再看全局 metadata(老会话),都没有就是存储自己的项目(那时一个存储只服务一个项目)。读写、
列表、跨项目搜索、谱系、级联删除、会话记忆(`HistoryStore::sidecar_locator`)、checkpoint 副本
都经它;导入导出的根检查用设置里的全局历史根加上 `HistoryStore::history_dirs_for` 答的目录,存储不答也挡得住全局根。项目的布局、新会话进不进项目、项目
此刻是否受信,是宿主经 `ProjectHistory` 回答的(装配层的 `ProjectHistoryPolicy`,信任取自全局
层的 `app` 块,两个设置文件任一变了就重读,一次列表不再每个会话各读一遍)。

**防植入。** 克隆来的仓库可以自带 `.agix/state/history/`。只有全局索引登记过、且索引指向的正是
那个目录的会话才被认:项目里多出来的 id 不进列表、搜索、谱系,resume 找不到,它的会话记忆与
checkpoint 也不会被当成谁的。新会话落进项目时自己建 `<id>/`(`create_dir`,不带 `_all`):这个目录
已经在了(仓库带来的、另一个全局根的存储先建的),会话就放全局,不接着里面的东西写。项目里的转录、
metadata、会话记忆与 checkpoint 副本打开时不跟随符号链接。读项目里的历史每次都看项目是否受信;信任撤掉后不读,错误是
`ProjectUntrusted`(RPC 上 `PROJECT_UNTRUSTED`)而不是"不存在"。写不看信任:已经放进去的会话
继续写在原处;删除与级联删除也不看:项目不受信时,侧链按索引里记的谱系找到,不读项目里的日志。

**搬家。** 索引记的项目根和会话目录都不在了(项目根要是从仍在的上级目录里消失;卷没挂上、网络盘连不上、目录读不了都不算搬家,索引不动),而本进程打开过的某个受信项目(存储自己的项目,或列表、
新会话碰到的)的 `state/history/` 下有这个 id,就把索引改写到新位置,并把 metadata 里的项目路径一起
改掉;只认索引里已有的 id。之后读出的 `Meta` 把旧根下的路径换成新根,会话按新位置的项目重建。索引的 `dir` 不是 `<project_root>` 那个项目的 `state/history/<id>` 就不认;删除前核对会话目录解析后仍在项目内,`state/` 事后被换成链接时只删全局那份。

**兼容。** 不迁移:已在全局的历史原地照读,两种位置可以并存,以索引为准;`app.history` 从
`project` 改回 `global` 后,项目里的会话照样经索引读。改设置在 daemon 启动时生效。

---

## 9. 扩展面

引擎有 47 个扩展点,分三类:**契约** 31 个(换掉一个子系统)、**拦截** 10 个
(坐在某件事的路径上)、**注册** 6 个(往一个集合里贡献)。

扩展只有两种:**Rust**——契约直接 `impl` 那个 trait 交给 `runtime::agent::Builder`,
注册与拦截打包成一个 `runtime::plugin::Plugin`,全部扩展点可用;**QuickJS 脚本**——给不
重新编译引擎的人,挂到其中九个点上,本身也是一个 `Plugin`(`script_host::ScriptPlugin`,
`Layer::QuickJs`)。两者落在同一个点上时按层排:Rust 先(包裹时在内),
QuickJS 后(包裹时在外),与交给 `Builder` 的先后无关(`extension_points.md`「叠加顺序」)。脚本和 `RunCode`
跑在同一套内嵌 QuickJS 里,限额限的是开销,不是对恶意代码的隔离,所以脚本能做什么按它
从哪来定。引擎不带 WebAssembly 运行时;需要的上层在 Rust 接口上自己做。

宿主不写 Rust 也有一条加工具的路:在 `session.create` 里声明 `options.host_tools`,调用经
`session.event` 的 `host_tool_call` 帧交给宿主、由 `session.respondToolCall` 回答(契约
`host_calls.call`,`daemon_rpc_protocol.md` §7.4)。不必另起一个 MCP server,也不进设置文件;
工具只活在这一个会话里。

完整清单、各自的触发时机与信任级别在 **`extension_points.md`**——那张表是从
`base::interface::catalog` 生成的,有测试保证它和代码一致。

```
cargo build -p daemon                                          # QuickJS + 插件包(默认)
cargo build -p daemon --no-default-features --features full    # 两者皆无
```

怎么写脚本见 `extending_quickjs.md`;插件包的格式见 `plugin_packages.md`;两种插件的开发
指南与样例见 `plugin_development.md`。

hook 的协议与二十九个事件见 [`hooks.md`](hooks.md);遥测的文件、信封与事件清单见
[`telemetry.md`](telemetry.md)。
