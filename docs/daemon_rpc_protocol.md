# Agix Daemon RPC 接口

daemon(`agixd`)把一个多会话 Agent 引擎池暴露成 JSON-RPC 2.0 服务。
本文回答"怎么用它":怎么连上、有哪些方法、事件怎么读、错误怎么分辨。

**协议版本:5**(`daemon.auth`、`daemon.ping`、`daemon.status` 的 `protocol_version`,以及发现文件里的同名字段)。

机器可读的接口描述:[`schemas/rpc.schema.json`](schemas/rpc.schema.json)(每个方法的 params 和 result,可选模块的方法带 `module` 字段;`daemon.event` 每个 `kind` 的字段;`session.event` 的 `kind` 名字,其字段只标注"未类型化",以 §7 为准;错误码),由 `cargo test -p assembly write_rpc_schema -- --ignored` 生成。

场景、会话、侧链这些概念本身是什么,以及它们在磁盘上怎么落,见
[`ARCHITECTURE.md`](ARCHITECTURE.md);本文只讲它们在 RPC 上长什么样。

## 怎么读本文里的例子

方法一节里的代码块有两种标记:

```jsonc
// → params        请求的 params 对象
// ← result        成功响应的 result 对象
```

示例中的 `"…"` 表示**该字段一定存在,但值随机器或时刻而变**(路径、id、时间戳、
计数)。`{}` / `[]` 表示该字段是对象/数组,内容随情况而定。

带 `// ← result` 的块由 `daemon/tests/protocol_doc_examples.rs` 读回来,跟一个真实
daemon 的真实响应逐字段比对;`daemon.doctor` 的例子由 `crates/assembly/src/doctor.rs` 的
`documented_shape_tests` 比对。文档里写了的方法必须可调用、可调用的方法必须写进
§6,由 `daemon/tests/protocol_doc_matches_dispatch.rs` 保证。

带 `// → params` 的请求示例会被 `daemon/tests/protocol_doc_wire.rs` 原样(只把 `"…"` 的
id 和路径换成真的)发给一个真实 daemon,不得被拒为 `INVALID_PARAMS` /
`METHOD_NOT_FOUND`;§7、§8 表里每个 `kind` 的字段集合,以及 §9.1 的 `error.data`,也由它
跟真实发出的帧、真实返回的错误比对。

---

## 1. 传输与编码

**帧格式**:每行一个 JSON 对象,`\n` 分隔(NDJSON)。不使用 Content-Length 头,
`tail -f` / `nc` 可以直接调试。WebSocket 例外:它本身就是消息分帧的,
**一条 text 消息 = 一帧**,不加 `\n`。

| 传输 | 怎么开 | 认证 |
|---|---|---|
| Unix socket | 默认开(`--socket <path>` 可改) | 文件权限:socket 为 `0600`,daemon 为它新建的目录为 `0700`;daemon 自己起的进程连不上,见 §5.1。`--unix-socket-auth` 时另要 `daemon.auth` 握手 |
| TCP | `--listen <addr>` | `daemon.auth` 握手,见 §5.1 |
| WebSocket | `--listen-ws <addr>` | `daemon.auth` 握手 + Origin 校验,见 §5.1 |

TCP 与 WebSocket 都要求 `--token` 或 `AGIX_DAEMON_TOKEN`;没有 token 时监听器
**启动即失败**,不会先起来再拒绝所有人。**推荐用环境变量**:命令行参数在 `ps` 里
别的用户也看得见,daemon 自己起的进程(模型调用的 Bash)也看得见。daemon 起子进程
(Bash、hook、stdio MCP 服务器)时会把这个变量剥掉,不会传下去。

**TCP 与 WebSocket 都是明文,daemon 不做 TLS。** token 和之后的每一帧(提示、工具结果、
`mcp.addServer` 的 `headers`)都按原样过网,所以只把它们开在回环地址、可信网络上,
或者经 SSH 隧道 / 自己的 TLS 终结代理转出去。

所有监听器都 bind 好之后,daemon 才写 `instances.d` 里自己的那一项(§2):宿主看到这个
文件出现,就可以马上连上。

**分帧之上的一切都与传输无关** —— 方法、参数、事件帧、权限提问三条路完全一致,
换传输不需要改协议层代码。把引擎嵌进自己进程的宿主经 `agix::embed`(`agix-sdk.md` §10)在任意
字节流上服务一条连接(进程内就是 `tokio::io::duplex`),分帧、帧上限、在途上限、回复与事件同一条
有序的流,都与 unix socket 上的一条连接相同,只是不要 `daemon.auth`。

编码 UTF-8。**单帧上限 16 MiB**,三条传输都强制。超限的连接被**关闭**而不是跳过
该帧:超限之后服务端已经不知道下一个帧边界在哪。

**请求**:

```jsonc
{ "jsonrpc": "2.0", "method": "session.create", "params": { }, "id": 1 }
```

`id` 省略 = 通知,服务端不回响应,出错(包括 `TOO_MANY_IN_FLIGHT`)也不回;流式帧照发。
`params` 省略等价于 `{}`。

**响应**:

```jsonc
{ "jsonrpc": "2.0", "id": 1, "result": { } }
{ "jsonrpc": "2.0", "id": 1, "error": { "code": -32004, "message": "…" } }
```

`result` 与 `error` 互斥,恰有其一。

**一行不是合法 JSON**:服务端回一帧
`{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}`
(`PARSE_ERROR`;`id` 是 `null`,因为读不出请求的 id),连接不断,继续读下一行。
**合法 JSON 但不是合法请求**(缺 `method`,或字段类型不对,如 `"jsonrpc":2.0`、`"method":42`):
帧里带着字符串或数字的 `id` 时,回
`{"jsonrpc":"2.0","id":7,"error":{"code":-32600,"message":"Invalid Request: …"}}`
(`INVALID_REQUEST`,`id` 原样带回,`message` 里是读不出来的原因);没有 `id`、`id` 是 `null`
或别的类型、或者帧里有 `result` / `error`(形状是响应,不回是为了不和对端互相回错误)的,
静默丢弃,不回任何响应。两种情况连接都不断。WebSocket 的文本消息同理。

---

## 2. 发现:找到 daemon

daemon 启动时在 `~/.agix/daemon/instances.d/` 下写自己那一份实例文件,正常退出时
删除。**每个实例一个文件,没有共享写点**,多个 daemon 同时启动不会因为并发
read-modify-write 同一个索引文件而丢更新。读取方 `read_dir` 汇总。

```
~/.agix/daemon/instances.d/desktop.json
~/.agix/daemon/instances.d/ci-runner.json
```

```jsonc
{ "instance": "desktop",
  "pid": 4210,
  "pid_start_time": 1754899200,
  "socket": "/Users/x/.agix/daemon/desktop.sock",
  "tcp": "127.0.0.1:7878",              // 仅在 --listen 时出现
  "ws": "127.0.0.1:7879",               // 仅在 --listen-ws 时出现
  "unix_socket_auth": true,             // 仅在 --unix-socket-auth 时出现:socket 也要先 daemon.auth
  "token_file": "/Users/x/.agix/daemon/auth/desktop.token", // 仅在 daemon 自己生成了 token 时出现
  "scenes": ["code", "harnix", "mini"],
  "protocol_version": 5,
  "started_at": "2026-08-11T10:00:00Z" }
```

写入是原子的(临时文件 + rename,`0600`),读取方不会读到半截 JSON。
`tcp` / `ws` 缺省表示那条传输没有监听。**token 不在这里**,它是凭据,不是地址;`token_file`
只是它所在的路径(见 §5.1)。

**陈旧条目**:daemon 崩溃时文件会残留。读取方必须按 `(pid, pid_start_time)`
**两项一起**判定进程是否仍存活 —— 只比 `pid` 会在 pid 被系统复用时把已死的 daemon
误判为存活。判定为陈旧的条目应忽略。

集成的第一步永远是:`read_dir` → 联合判活 → 按 `scenes` / `instance` 挑一个 →
连它的 `socket`。**不要自己拼 socket 路径去猜**;daemon 换启动参数重启时,读实例
文件的客户端不用改代码。

daemon 另外还写一份 `~/.agix/scenes/<scene>/daemon.lock`(`{pid, pid_start_time,
socket_path}`)。它是单实例锁,不是发现文件,客户端不要读它,用 `instances.d/`。daemon
运行期间对它持有 `flock`,同一场景的第二个 daemon 拿不到就拒绝启动;退出时删掉它。

**socket 路径**(daemon 自己启动时的推导规则,列出仅供排查):

```
--socket <path> 指定        → 用它
否则                        → $HOME/.agix/scenes/<scene>/daemon.sock
```

(Windows 上没有 Unix socket,daemon 改用命名管道 `\\.\pipe\agix-daemon`。)

路径上已经有一个还有人在听的 socket(另一个 daemon,或嵌进别的进程的引擎)时,daemon 不替换它,
以 `AddrInUse` 拒绝启动;没人听的旧 socket 文件照常换掉。「有人在听」要连续约一秒都连得上才算:
旧引擎关掉监听时,它刚 fork、还没 exec 的子进程手里还有一份监听描述符,这一小段时间里连得上却没人服务。

`--instance <name>` 决定 `instances.d/<name>.json` 的文件名,默认是把所有激活场景
(`--scene` 加 `--scenes`)去重排序后用 `,` 连接。多 daemon 并存时 instance 名必须
不同 —— 重名不会被拦下,两个进程会互相覆盖对方的发现条目。

经 `agix::embed` 嵌进宿主进程的引擎,`EmbedConfig.instance` 给了名字才写实例文件、拿
`daemon.lock`,路径在它的数据根下;没给就两样都不写,`instances.d` 里找不到它,由宿主自己告诉
客户端怎么连。一个进程里至多一个给了名字的引擎(`agix-sdk.md` §10)。

---

## 3. 通用约定

### 3.1 定位参数

| 参数 | 类型 | 说明 |
|---|---|---|
| `scene` | string | 场景 id。`session.create` 省略 = daemon 的默认场景(`--scene`) |
| `project_root` | string \| null | 绝对路径。`null` = 无项目会话;**省略** = daemon 的默认项目(它的 cwd) |
| `session_id` | string | 会话 id,跨重启稳定 |
| `turn_id` | string | 一轮对话的 id,客户端可自带,不传则服务端生成 |
| `prompt_id` | string | 权限提问的 id,进程内有效 |

`session.create` 的 `project_root` **三态**是有意的:省略、显式 `null`、给路径,
分别是"用 daemon 的默认项目"、"这是一个无项目会话"、"绑到这个项目"。给路径时目录
必须存在,否则 `PROJECT_NOT_FOUND`。

无项目会话的工具在会话自己的工作目录里干活:`<状态目录>/scratchpad/sessions/<session_id>`,
相对路径都按它解析,`Bash` 也在这里起。这个目录、状态目录下的 `scratchpad` 与 `memory`
不用询问就能写;别处(包括状态目录所在的家目录)都是"项目之外",写要询问,无人值守时拒绝,
读同理(ARCHITECTURE §3.2、§6.3)。它没有项目层:不读项目设置、skills、agents、rules、hooks,
没有 git 信息,也不找 `AGENTS.md`。`session.close` 不删这个目录,恢复同一个会话还在原处;
`session.delete` 连它一起删掉。

桌面端同时开多个项目时应始终显式传值(含显式 `null`)—— 依赖 daemon 的 cwd 会让
所有会话共享同一份项目层配置。

### 3.2 场景能力

每个场景声明两个能力位,`scene.list` / `scene.describe` 暴露:

| 能力 | 含义 | code | harnix | mini |
|---|---|---|---|---|
| `requires_project` | 会话必须绑定项目根 | ✅ | ❌ | ❌ |
| `supports_team` | 注册 Team 工具 | ✅ | ✅ | ❌ |

宿主据此决定新建会话时要不要先弹"选择项目"。`requires_project` 的场景传
`project_root: null` 会在 `session.create` 就被拒(`PROJECT_REQUIRED`),而不是等到
第一次工具调用才失败。

### 3.3 会话类型

| `session_kind` | 谁发起 | 默认是否出现在 `session.list` |
|---|---|---|
| `primary` | 客户端(`session.create` / `session.run_turn`) | 是 |
| `sidechain` | 模型(`Agent` 工具 / 团队成员) | 否 |

两者存储与查询机制完全一致,差异有三:

- **可见性** —— 侧链默认不进 `session.list`,要传 `include_children: true`,或者用
  `parent_session_id` 只查某个父会话的侧链。
- **可恢复性** —— 侧链一旦写下终止标记就**不可 resume**,列表里 `resumable` 为
  `false`,强行 `session.resume` 返回 `SIDECHAIN_TERMINAL`。一个跑完的一次性任务
  没有续跑语义;被中断的侧链没有这个标记,仍可恢复:回到父会话的项目里,在 worktree 里干活的回到
  那个 worktree,worktree 不在了就拒绝恢复(§6.3 `session.resume`)。
- **寿命** —— 侧链的 transcript 锚定在父会话上:

  | 触发 | 侧链 |
  |---|---|
  | `session.close` | **删除** |
  | `session.delete` | 删除(连同父 transcript) |
  | LRU 驱逐 / 空闲超时 / `daemon.shutdown` | **保留**(只回收运行体) |
  | daemon 崩溃 | 遗留在磁盘上 |

  遗留的侧链**没有任何自动回收**:仓库里没有清理它们的 GC,也没有对应的保留期
  配置。要清理只能手工删 transcript 文件。

  级联删除只删 `session_kind` 为 `sidechain` 的子会话。`session.fork` 出来的会话也记着
  源会话作父,但它是 `primary`,不受上表影响(见 `session.fork`)。

  实际含义:**"打开子 Agent 的会话查 history"只在父会话还没 close 时可用。**
  父会话 close 之后,它自己的 transcript 里仍留有 `Agent` 工具的 `tool_use` 与
  `tool_result`(含子 Agent 的最终输出),丢掉的只是中间步骤。

### 3.4 场景绑定与恢复

每个会话在 transcript 的 `Meta` 行里记着自己属于哪个场景。**恢复时不允许跨场景**:

```
Meta.scene
  ├─ 缺失          → 按当前 daemon 的场景恢复
  ├─ 与 daemon 的场景不符 → SCENE_MISMATCH,拒绝
  └─ 相符                 → 恢复
```

这条检查对 `session.run_turn` / `.resume` / `.fork` / `.close` / `.delete` /
`.subscribe` / `.interrupt` 都生效,错误的 `data` 带 `recorded_scene` 与
`requested_scene`,客户端可以据此改用正确的 daemon(或者先 `scene.activate`)重试。
`run_turn` 传的 `session_id` 在磁盘上没有历史时不受此约束——那是新建,不是恢复。

**恢复出来的会话跑在它自己记录的场景上**,不是这个 daemon 的默认场景:系统提示词、
工具白名单、deferred 策略都跟着 `Meta.scene` 走,与设置层的解析口径一致。

`Meta.project_root` **不参与校验**:项目目录被移动过之后仍然能恢复。

### 3.5 时间、id 与用量

时间戳一律 RFC 3339 UTC(`2026-08-11T10:00:00Z`)。会话未激活时,列表里的
`created_at` / `last_active` 是**空字符串**(见 `session.list`)。

`usage` 四个字段,缓存读写各自成项:

```jsonc
{ "input_tokens": 12345, "output_tokens": 678,
  "cache_creation_input_tokens": 2048, "cache_read_input_tokens": 30720 }
```

四项**互不重叠**,不能只读 `input_tokens` 当作这一轮读了多少:开了提示缓存的
provider 上,命中之后绝大部分输入记在 `cache_read_input_tokens`,`input_tokens`
只剩增量。三项各自的价钱也不同(缓存读是普通输入的一个零头,缓存写是溢价),
所以要算钱就得四项分开算。

缓存两项是后加的。0.2.5 之前的客户端只读前两项,不受影响;读旧录像时这两项缺省为 0。

daemon 按**单轮**如实上报,不做跨轮/跨会话聚合。

**没有游标分页。** `session.history` 用 `offset` / `limit`;`session.list` 不分页,
一次返回全部。

### 3.6 项目信任

一个项目自己的配置(项目层目录 `<项目>/.agix/` 下的设置、agent 类型、hook 脚本,`.agents/skills/`;项目层目录缺省叫 `.agix`,嵌入方可经 `EmbedConfig.project_dir` 改名,下文的 `.agix` 凡指项目里的都是它)
是仓库内容。**没被信任的项目按插件级信任处理**:它的设置只能收紧,不能放宽,而且是白名单——
项目层里没有登记的设置键一律不生效,登记了的要么照常、要么只在不比下面几层更宽时保留、要么整个不生效;
它的 `hooks`、`mcp.servers`、`scripts` 不生效,项目层目录的 `hooks/` 不进 hook 命令的 `PATH`;
决定对话、凭据、遥测送到哪里的键(`providers`、排了项目自己 provider 的 `model.order`、
`sandbox.network.proxy`、`telemetry.otlp_endpoint`、`recorder`)、`prompt.instruction_files` 与
整个替换系统提示词的 `prompt.override` 不生效,`telemetry.enabled: false`(关审计记录)也不生效,
`app` 块整块不认;
它的 agent 类型只能收紧、不替换同名类型;它的 skill 里的 `` !`cmd` `` 不执行,`allowed-tools`
不授予任何东西。完整的表在 `ARCHITECTURE.md` §6.6。

信任是**项目根**的属性,只能写在全局层的 `app` 块里(`trust_all_projects` 加 `trusted_projects` 路径 /
glob 清单),项目自己写的忽略。三态:

| `trust` | 意思 |
|---|---|
| `trusted` | 项目层全部生效 |
| `untrusted` | 项目层按上面收紧 |
| `none` | 没有自己的项目层(无项目会话,或项目根就是放全局目录的那个目录,如 `$HOME`)。项目层目录(缺省 `.agix`)是链到全局 / 场景目录的符号链接时不算,按 `untrusted` |

**缺省 `app.trust_all_projects: false`**:一个谁都没确认过的项目,项目层按上面收紧。
宿主用 `project.status`(§6.8)拿到状态和"信任之后才生效的东西"来画确认对话框,用
`project.trust` 记下用户的选择;无人值守的部署在全局设置里写 `app.trusted_projects` glob 预先信任。
`session.create` / `session.resume` 的结果里有 `project_trust`(同上表的三个值)与
`trust_source`——是谁定的:

| `trust_source` | 意思 |
|---|---|
| `default` | 没有 `app.trusted_projects` 条目命中,由 `app.trust_all_projects` 决定 |
| `user` | 全局 `settings.json` 里的一条 `app.trusted_projects` 命中 |
| `host` | 全局 `settings.local.json` 里的一条命中——`project.trust` 写在这里 |
| `null` | 没有自己的项目层(`project_trust` 是 `none`) |

**会话历史放进项目时也看信任。** 全局设置 `app.history` 为 `project`(嵌入时也可写在
`EmbedConfig.settings`)时,受信项目的会话放在 `<项目>/.agix/state/history/<id>/`(worktree 里的
放在主仓库的这个目录),全局 `sessions/<id>/` 只留索引;不受信项目的照旧放全局。只认全局索引登记过的
项目内会话,项目里多出来的不进 `session.list`、搜索,`session.resume` 报 `SESSION_NOT_FOUND`。
项目后来被取消信任,它里面的历史不读:`session.list` 与搜索里没有这些会话,`session.resume`、
`session.history`、`session.export`、带这个 `session_id` 的 `session.create` 与 `session.run_turn` 等指名它们 → `PROJECT_UNTRUSTED`,消息写明是哪个项目(删除不看信任,侧链照样级联删掉);
重新信任后照常。项目整个搬了家,daemon 在新位置打开它(它是 daemon 的项目,或有会话建在、列在它上面)
时索引跟过去,会话按新位置恢复。布局见 `ARCHITECTURE.md` §8.2。

---

## 4. 数据类型

### 4.1 `ContentBlock` — 消息体

出现在 `session.history` 的 `messages[].content`,以及 `tool_result.content` 的
数组形态里。字段名与取值和 Anthropic Messages API 一致,便于直传。

```jsonc
{ "type": "text", "text": "……" }
{ "type": "image", "source": <ImageSource> }
{ "type": "tool_use", "id": "toolu_…", "name": "Bash", "input": { } }
{ "type": "tool_result", "tool_use_id": "toolu_…",
  "content": <ToolResultContent>, "is_error": false }   // is_error 为 false 时不出现
{ "type": "thinking", "thinking": "……", "signature": "…" }
{ "type": "redacted_thinking", "data": "…" }
```

> **`thinking` / `redacted_thinking` 必须原样保留。** `signature` / `data` 是载荷
> 不是装饰:开了思考且该轮有工具调用时,下一次请求要把上一轮的思考块连同签名回传。
> 纯展示的客户端可以不渲染,但不要丢弃。

还有一个只出现在请求方向的块(列出以免解析器遇到时报错):

```jsonc
{ "type": "cache_edits", "cache_edits": [ { "type": "delete_tool_result", "tool_use_id": "…" } ] }
```

消息本身按 `role` 判别:

```jsonc
{ "role": "user",      "content": [ <ContentBlock> ] }
{ "role": "assistant", "content": [ <ContentBlock> ], "stop_reason": "end_turn", "model": "…" }
{ "role": "system",    "content": "……", "kind": "local_command" }  // local_command|reminder|notice
```

`system` 只进 transcript、不进模型请求 —— 本地斜杠命令的输出、提醒、通知。

### 4.2 `ImageSource`

```jsonc
{ "type": "base64", "media_type": "image/png", "data": "<base64,无 data-URI 前缀>" }
{ "type": "url",    "url": "https://…" }
```

### 4.3 `ToolResultContent`

**无标签联合** —— 按 JSON 类型判别,不是靠 `type` 字段:

```jsonc
"纯文本结果"                          // string 形态
[ <ContentBlock>, <ContentBlock> ]    // 数组形态(工具返回了图片或多段内容)
```

客户端必须同时处理两种。

### 4.4 `Attachment` — `session.run_turn` 的附件

```jsonc
{ "kind": "file",  "path": "/abs/path.png" }
// daemon 读盘。按扩展名判定:图片 → image 块;其余内联为文本

{ "kind": "text",  "path": "selection", "content": "……" }
// 宿主已读好的文本。path 只作展示标签,可以是合成名(如编辑器选区)

{ "kind": "image", "media_type": "image/png", "data": "<base64>" }
// 宿主已解码的图片,磁盘上没有文件(剪贴板粘贴)
```

**形状不对的附件会被丢掉并写一条 warn 日志,turn 照常跑** —— 不会让整次调用失败。
所以客户端别指望用错误码发现自己拼错了字段名。

大小受 §1 的 16 MiB 单帧上限约束;base64 放大约 4/3,单张内联图片实际上限约
12 MiB。超大文件用 `kind: "file"` 让 daemon 自己读盘,不占帧。

### 4.5 `PermissionMode`

`session.create` / `session.run_turn` / `session.resume` 的 `options.settings.permissions.mode`、
`session.setPermissionMode` 的 `mode`,和 settings.json 的 `permissions.mode` 共用这一组取值(camelCase):

| 值 | 含义 |
|---|---|
| `dontAsk` | 不询问,未显式放行的一律拒绝 |
| `plan` | 只允许只读工具 |
| `default` | 未显式放行的工具调用都发 `prompt` 询问(**默认**) |
| `bubble` | 把询问上抛给父 Agent —— 程序专用,写进 settings.json 会被退回 `default` |
| `acceptEdits` | 自动放行项目内的文件写入与只读调用,其余询问。"项目内"指会话的工作目录、`files.extra_dirs`,读还包括 `files.read_dirs`(含 `"/"` 时不限):项目外的写入照样询问,`Read` / `Glob` / `Grep` 读项目外、只读的 `Bash` / `Monitor` 命令参数指向项目外(含 `~`、`$VAR` 这类展开不了的写法)也照样询问,与 `default` 一致(`.ssh` 一类的凭据仍被拒)。项目内的 `.git`、项目层目录(缺省 `.agix`;改了名时 `.agix` 也算)、`.claude`、`.agents/skills` 下的写入也照样询问(改它们等于改 agent 自己的规则)。只读的 `Bash` / `Monitor` 命令只要跑 `git`,就还要求每一次 `git` 找到的仓库就是会话目录自己所在的那个(不能是裸仓库;`cd` 进项目自带的嵌套 `.git`、或 `cd` 到去向不明的目录再跑 `git`,都要询问),因为仓库配置(`core.fsmonitor`、`diff.external` 一类)会让 git 自己去启动程序,会话自己的仓库是用户的,项目里带来的是别人写的。`WebFetch` / `Ping` 只放行 `web.allowed_domains` 列出的主机;MCP 工具一律询问,server 自报的 `readOnlyHint` 不算(有 allow 规则的照常放行) |
| `bypassPermissions` | 不询问、不看用户各层(设置文件、`options.settings.permissions`)的规则 —— 那些规则里的 `deny`、`/etc/passwd` 一类路径、`rm -rf /` 一类命令都放行。凭据(`.ssh` / `.aws` / `.gnupg`、沙箱拒读的家目录凭据库如 `.git-credentials` / `.config/gh` / `.kube`、状态目录里的设置与 token 等,§6.1)任何模式都拒,部署方 managed 层的 `deny` / `ask`、宿主策略的 `deny` 照样生效;`PreToolUse` hook 的拦截、委派带来的 deny 照样拒;`sandbox.require` 打开而沙箱兜不住的命令照样拒(没装 `bwrap` 的 Linux 上即每条 `Bash`);除此之外只有不合法的输入(没读过就改、超出上限、会被读错的路径)被拒,`Bash` 仍在 OS 沙箱里 |

表格是按**宽松程度从严到宽**排的,这个顺序就是会话级取值被夹紧的依据:会话只能
比 settings 更严,不能更宽,除非 settings 里开了 `permissions.client_may_loosen`。
`session.setPermissionMode` 设的模式同样按这个顺序夹紧。

`plan` 模式里调 `ExitPlanMode` 照常询问(`prompt_type: "permission"`、`tool_name:
"ExitPlanMode"`,有命中的规则时按规则),是 `plan` 不拒的唯一一个非只读工具:`permit` 后
退出,`deny` 留在 `plan`。会话中途 `EnterPlanMode` 进的 `plan`,退出后回到进入前的那个
模式;会话一开始就是 `plan` 的,退出后是 `default`。宿主经 `session.setPermissionMode` 切进
`plan` 的同样如此;从 `plan` 切到别的模式就离开了 `plan`。活着的会话,`session.resume` 的
`permission.mode` 报的是它此刻所在的模式。

`plan` 是会话的状态,不随引擎走:在不在 `plan` 里、退出后回到哪个模式,连同 todo 列表,
每轮结束时有变化就记进会话日志(扩展条目 `ns:"agix.session_state"`,ARCHITECTURE §4)。会话被
驱逐(空闲超时、池子满了)后再用、daemon 重启后恢复、原地重建,回来都还在 `plan` 里,退出后
回到进入前的那个模式,但不宽于这时的设置:设置收紧了,或日志是 `session.import` 带进来、`session.fork`
拷过去的,退出后要回到的模式同样按这时的设置夹紧(同 `options.settings.permissions.mode`,managed 层关了
`bypassPermissions` 时得 `default`),只能由程序设的 `bubble` 换成会话这时按设置得到的模式。不在 `plan`
里的会话恢复时取 `session.setPermissionMode` 设的模式,没设过就取 `options` 给的,没给就取设置的,都按
恢复时的设置夹紧。

**`options` 越界不报错,报在 `clamped_options` 里。** `options.settings.permissions.mode` 比
settings 宽的被夹到 settings 允许的最宽处,`session.create` 的结果带 `clamped_options`(§6.3)说
要的是什么、生效的是什么;managed 层关了 `bypassPermissions` 时即使设置开了 `client_may_loosen` 也得
`default`,同样报在那里。程序专用的 `bubble` 不是会话能要的模式,进 `invalid_options`。要确认会话此刻的
模式,读 `session.resume` 响应里的 `permission.mode`。`session.setPermissionMode` 不同:回执里的 `mode` 就是实际生效的模式,`clamped`
说它有没有被夹窄,`bubble` 得 `INVALID_PARAMS`。

### 4.6 `PermissionRule`

`session.startAgent` 与 `plugin.call` 的 `permission_rules[]`(会话的规则写在 `options.settings.permissions`
里,是设置文件的规则串,§6.3):

```jsonc
{ "source": "session",              // 见下表
  "behavior": "allow",              // allow | deny | ask
  "tool_name": "Bash",
  "rule_content": "git push:*" }    // 可选;省略 = 匹配该工具的全部调用
```

`rule_content` 的写法按工具而定:

- `Bash`(以及 `Monitor`)用命令前缀模式(`"git push:*"`),对命令行里的**每一条简单
  命令**分别匹配:`&&` `||` `;` `|` `&`、换行、`$(...)`、反引号里的命令各算一段,引号
  里的不切。任一段命中 `deny` 就拒;`allow` 要每一段都被放行,或者整行原样等于一条
  不带通配的 `allow`。`deny` / `ask` 还按每段实际启动的程序再比一次:引号与转义去掉、
  花括号展开、程序路径取名字(`/bin/rm` 即 `rm`)、`env` / `command` / `exec` / `sudo` /
  `nice` / `time` / `timeout` / `xargs` 这类 wrapper 连同选项剥掉、`sh -c` / `eval` /
  `find -exec` 里的命令也算;程序名读不出来的(`$CMD x`)在有带内容的 `deny` / `ask`
  时按 `ask` 处理。`allow` 只认原文。
- `Bash` 的只读命令在路径参数都落在项目内时不询问;参数里有 `$VAR`、`~` 或替换的,
  `cd` 到不是项目内字面路径之后再读的,照常询问。
- 文件类工具用 glob(`"/etc/**"`)。匹配前两边都规整成绝对路径:相对路径按会话的
  工作目录解析,`.` / `..` 去掉,`~` 展开;符号链接解析后的路径上命中 `deny` / `ask`
  也算,`<目录>/<worktree 目录>/<名>/`(缺省 `.wkts`,设置 `app.worktree_dir`)下的路径在 `<目录>/` 下的同一路径(主检出里的那一份)上
  命中 `deny` / `ask` 也算——锚在项目上的规则对子 Agent worktree 里的拷贝同样成立。`*` 不跨
  `/`,`**` 跨。`Read(...)` 同样约束 `Grep` / `Glob`,`Edit(...)` 同样约束 `Write` /
  `NotebookEdit`。沙箱开着时,点名具体路径的 `Read(...)` `deny`——一个文件或目录,或目录后面只跟
  `/**`——也进沙箱,按写出的路径拒(目录里的符号链接不跟),`sandbox.allow_read` 放不开;沙箱里的
  `Bash` / `Monitor` 命令用 `python3 -c`、变量拼出的文件名也读不到。挡不住的:macOS 上命令写得了
  被拒文件所在的目录时,改名再读;Linux 上命令启动之后才出现的路径。Linux 上被拒的目录命令也写不进,
  macOS 上写得进。带别的通配的(`**/*.pem`、`secrets/*`)不加,要挡就写 `sandbox.deny_read` 或
  `Bash(...)` 的 `deny`;`ask` 不加;是或包含工作目录、项目根、可写目录、状态目录的(`Read(./**)`),
  以及是、包含或落在系统目录(`/usr`、`/bin`、`/etc`……)里的,不加;沙箱关着时不加。细节见
  ARCHITECTURE.md §6.3。
- `WebFetch` / `Ping` 用 URL 模式 `scheme://host[:port][/path]`(`"https://*.example.com/**"`)。
  调用的 URL 先规整(去掉 userinfo、query、fragment,主机名小写、Unicode 主机转成 punycode、去掉
  末尾的 `.`、默认端口省略,`ws` / `wss` 按 `http` / `https` 算),模式的主机同样转成 punycode,两侧
  路径里字母、数字、`-` `.` `_` `~` 的百分号转义解码(`/%61dmin` 即 `/admin`;`%2F` 不解码),再
  **按部分**分别匹配:主机里的通配只跨 `.`,到不了端口和路径(`https://evil.net/.example.com/`
  不命中上面那条);不写端口只认缺省端口,`:*` 认任何端口;不写路径只认 `/`,路径里 `*` 不跨 `/`,
  `**` 跨。模式里不能有 `?` / `#`(`?` 不是通配)。与调用的 URL 原样相等的也命中;"永久允许"记下的
  是原样的 URL,其中的 `*`、`[`、`{` 按字面转义,所以只放行那一个 URL。`deny` / `ask` 另外按整串
  glob(`*` 跨 `/`)匹配原样的 URL,两种读法任一命中就生效——`"https://evil.example/*"` 的 `deny`
  拒绝 `https://evil.example/a/b`;`allow` 只按部分匹配。写不成这种形状的(`"*example.com*"`)作
  `allow` 只命中原样相等的 URL,作 `deny` / `ask` 只按整串 glob 匹配。

裁决只看 `behavior`:一次调用命中的规则里只要有一条 `deny` 就拒绝,否则有 `ask`
就提问,否则才是 `allow`。`source` 和规则写得多具体都不参与裁决——客户端传入的
`allow` 推不翻任何一层设置里的 `deny`。同一行为有多条命中时,按特异性、再按下表
的数值挑一条作为报出的那条规则:

| `source` | 数值 |
|---|---|
| `policySettings` | 70 —— 部署方的 managed 设置(`ARCHITECTURE.md` §6.1) |
| `cliArg` | 60 |
| `session` | 50 —— **客户端经 RPC 传入的规则用这个** |
| `command` | 45 |
| `localSettings` | 40 |
| `projectSettings` | 30 |
| `userSettings` | 20 |
| `plugin` | 5 |

`source` 与 `behavior` 都是 camelCase/小写的既有序列化格式,不是笔误。

`bypassPermissions` 会话不看这些规则,只看 `policySettings` 那一层的
`deny` / `ask`——但只认真正从 managed 设置文件读进来的规则:客户端在这里写
`"source": "policySettings"` 只改变报出时的名字,不会让它在 bypass 下生效。managed 设置关掉了
bypass(`permissions.disable_bypass`)时,请求 `bypassPermissions` 的会话按 `default`
运行,`session.resume` 报出的 `permission.mode` 也是 `default`。

### 4.7 `PermissionDecision` / `QuestionAnswer` / `McpElicitationAnswer` — 回答提问

`prompt_type` 为 `permission` 时,`session.respondToPrompt` 的 `decision`,四种形态:

```jsonc
{ "type": "permit" }
{ "type": "deny", "reason": "…" }
{ "type": "permit_always", "scope": "session" }   // 本会话内一直允许
{ "type": "permit_always", "scope": "local" }     // 另外写进项目层目录(缺省 .agix)的 settings.local.json,之后新建的会话即可见
```

`permit_always` 放行的是这一次调用的原文:`touch *.rs` 不会放行 `touch other.rs`。`local` 写下的是
`{"tool": "Bash(touch *.rs)", "action": "allow", "match": "exact"}`,`match: "exact"` 让之后的会话
同样按原文比对。会话的权限不记放行时(嵌入方给了 `without_grants`),嵌入方的 `Elicitation` 收到的选项里没有 `permit_always`。
ask 规则问的(含 managed 层的、宿主策略问的)和插件命令子调用的提问也不提供:ask 规则排在记下的放行之前判定,
记下了下次也照样问;子调用的放行是那次命令的,不留给模型。这些提问帧上 `permit_always` 为 `false`,答了
`permit_always` 只放行这一次,什么都不记。不看这个字段的宿主照旧会摆出"总是允许",选了也只放行这一次;
以前在 ask 规则底下选 `local` 会写进 `settings.local.json` 的那条精确放行,现在不写——它只在那条 ask 规则
被删掉之后才会生效。
`local` 写不下去时——项目未信任(它的 `settings.local.json` 下次加载会被扣下,先 `project.trust`)、
项目层目录是全局或场景目录、项目层目录是指到项目之外(如另一个项目的 `.agix`)的符号链接(与 `config.set` 等写项目层的
方法同一个判定)、写盘失败——放行只在本会话有效,会话收到一帧 `notification`
(`status: null`)说明原因。

未信任的项目(§3.6)不读它 `settings.local.json` 里的 allow 规则,`local` 写下的规则因此
不会被之后的会话读回;要它持久,先信任这个项目。无项目会话(`project_root: null`)没有
"这个项目",`local` 与 `session` 一样只在本会话内生效,什么也不写。

`prompt_type` 为 `question` 时,`session.respondToPrompt` 的 `answer`,两种形态:

```jsonc
{ "type": "answer", "text": "a" }        // 选项的 key,或者自由文本
{ "type": "decline", "reason": "…" }     // 不回答
```

`text` 等于某个选项的 `key` 时,模型读到的是 `key: label`(如 `a: main`);否则原样交给模型。

`prompt_type` 为 `mcp_elicitation` 时,`session.respondToPrompt` 的 `answer`,三种形态:

```jsonc
{ "type": "accept", "content": { "color": "blue" } }   // form:按那一帧的 requested_schema 填
{ "type": "accept" }                                   // url:人已经去过那个页面,不带 content
{ "type": "decline" }                                  // 人说了不
{ "type": "cancel" }                                   // 没做选择
```

答案原样回给 MCP server,不进转录、不进遥测,模型只看到 server 拿它做出的工具结果。form 的
`content` 先按 server 的 `requested_schema` 校验,不合 → `INVALID_PARAMS`,错误的 `data.schema_errors`
列出每一处问题,**提问不撤**,可以改了再答;url 的 `accept` 带了 `content`、form 的 `accept` 没带,
同样是 `INVALID_PARAMS`。

对 server 的询问有上限:`message` 超过 4 KiB 被截断,那一帧带 `limited: true`;form 的 schema 超过 16 KiB 或属性多于 32 个,不推给宿主,直接回 server `decline`。一次工具调用里老版本 server 同时在途的询问至多 4 个、总共至多 10 个,2026-07-28 的一轮至多问 8 个、至多 10 轮(server 转成任务的调用同样按轮计),超出的回 server `cancel`;总数超了的,这次调用的结果末尾有一行说明被取消了几个。

### 4.8 `stop_reason`

出现在 `turn_complete` 事件和 transcript 的 assistant 消息上:

| 值 | 含义 |
|---|---|
| `end_turn` | 模型正常结束 |
| `max_tokens` | 触到单次输出上限 |
| `tool_use` | 模型请求工具(轮内中间态) |
| `stop_sequence` | 命中停止序列 |
| `pause_turn` | 模型请求暂停。引擎把这条回复原样发回去让它接着写,所以一轮通常不以它结束;只有恢复策略不续(如 `NeverRecover`)时才会 |
| `cancelled` | 这一轮被 `session.interrupt` 取消;已经推过的那段回复留在 transcript 里 |
| `max_turns` | 触到本轮模型调用次数上限(`turn.max_model_calls`,引擎自己发的,不来自 API) |
| `max_structured_output_retries` | 带 `options.output_schema` 的会话,结构化输出调用的重试次数用完仍不合 schema(引擎自己发的) |
| `command_executed` | 这一轮是斜杠命令,在本地执行完,不调模型(§6.3 `session.run_turn`、`commands.*`) |
| `stopped_by_hook` | hook 截停了这一轮(`PostToolUse` / `Stop` hook 要求不再继续),理由在 `turn_complete.message` |
| `context_exceeded` | 上下文超限且压缩也救不回来 |
| `budget_exceeded` | 触到一个 token 预算:这一轮的(`turn.token_budget` 或 `options.settings.turn.token_budget`,见下)、会话的、目标的,或宿主起的子 Agent 自己的(`session.startAgent` 的 `token_budget`,`the task's …`);是谁的在 `turn_complete.message`(如 `this turn's token budget of 2000 is used up`) |
| `unknown` | 反序列化兜底,遇到未知取值时的落点 |

**一轮的 token 预算含子 Agent。** 预算按未命中缓存的输入、缓存写入与输出计(缓存命中不计,同 provider 配额),算的是这一轮的全部模型调用:
会话自己的、它在这一轮里起的子 Agent(前台、后台)与团队成员(`TeamCreate` 的成员、这一轮派了活的持久成员)的,
以及这些 agent 各自在旁边发的调用(压缩写摘要、记忆召回与轮末的记忆抽取、`WebSearch`、`WebFetch` 的抽取)
和它们触发的 `prompt` hook(记在 `usage_breakdown.side`)、`agent` hook 的子 Agent(记在 `delegates`)。
首轮之后的自动命名与手动压缩(`/compact`、`session.compact`)不在任何一轮的模型循环里,不计。每次调用发出前,
先估计这次请求里不会由 provider 缓存供给的输入(上一次调用读到了缓存时只算那之后新加的,没读到时连上一次被计的
输入一起算,见 ARCHITECTURE「调用之前估输入」),从剩余里扣掉,`max_tokens` 压到不超过扣完的数;扣完不到 1024
时这次调用不发,这一轮以 `budget_exceeded` 结束,`turn_complete.message` 写明剩余与估计的输入。一次调用之后合计到达预算时同样结束;
到达 90% 时往对话里加一条提醒,让模型收尾。子 Agent 判的是同一份合计:别的 agent 把预算用完后,在跑的子
Agent 在下一次调用前以 `budget_exceeded` 收尾,交回父会话的结果末尾写明是这个原因和谁的预算。这一轮起的后台子 Agent
在这一轮结束之后仍记在这份合计上,照样会被它截停;下一轮有自己的一份。一次调用中途断开或失败、这一轮
以错误结束时,provider 已经报出的那部分用量同样计入。宿主经库式用法注入的 `BudgetPolicy` 在这之外再判,
两边都要过。

**会话的 token 预算。** 设置 `session.token_budget`(缺省不限)给一个会话从创建起的累计设上限,口径与
算的调用同一轮的预算,只是不按轮清零:会话所有的轮(宿主的、定时任务的、目标的)都在内,跨重建、驱逐与
daemon 重启(累计即 `session.get` 的 `usage`,§6.3)。每次调用同样先从它的剩余里扣掉输入的估计,`max_tokens`
压到不超过扣完的数,扣完不到 1024 时不发;用完时这一轮以 `budget_exceeded` 结束,`turn_complete.message` 为
`this session's token budget of <N> is used up`,到达 90% 时同样提醒收尾。之后每一次 `run_turn` 都在
第一次调用前以 `budget_exceeded` 结束,不调模型;要接着用,宿主调高设置(或换一个会话)。子 Agent 与
团队成员判的是根会话的这份累计。

**宿主起的子 Agent 的预算。** `session.startAgent` 给了 `token_budget` 时,这个子 Agent 另有一份上限,口径同上,
挂在会话的这份下面:它的调用两份都计,判的是剩得最少的那份,用完时只截停它自己。宿主起的子 Agent 被
任何一份预算截停时,任务以 `failed` 结束,`error` 是 `budget_exceeded: ` 加上那一轮 `turn_complete.message`
的原文(§6.3 `session.startAgent`)。

---

## 5. 连接、认证、并发与顺序

### 5.1 认证

TCP 与 WebSocket 需要(开了 `--unix-socket-auth` 时 unix socket 也要),都用**同一个 token、同一段校验代码**。连接建立后的
**第一帧**必须是:

```jsonc
{ "jsonrpc":"2.0", "method":"daemon.auth", "params":{"token":"…"}, "id":0 }
← { "jsonrpc":"2.0", "id":0, "result":{"authenticated":true,
      "version":"…", "protocol_version":5, "capabilities":["…"]} }
```

`version` 是 daemon 二进制的版本,`protocol_version` 是本文描述的协议版本,
`capabilities` 是能力位(§6.1 `daemon.ping`)。宿主应当在握手这一步就拒绝它不认识的
协议版本,而不是用到一半才发现。

第一帧不是 `daemon.auth`、token 不对、或者根本不是合法 JSON → `UNAUTHORIZED
(-32003)`,然后关连接(WebSocket 会先发 close 帧再断,浏览器端拿到的是"被拒绝"
而不是"连接出错")。token 比对是定长时间比较。

认证粒度是 **daemon 级**,不按场景或会话签发 —— 一条通过认证的连接可以访问这个
daemon 的全部场景与会话,而且**没有任何逐方法的授权**。特别地,
`config.setProvider` 能把后续所有模型流量指向调用方给的 `base_url`(`tier:"global"` 时是所有项目的);
把 socket 或 token 暴露出去,等同于暴露模型凭据本身。连接不加密(§1),token 在不可信的
网络上一过就算泄露了。

Unix socket 靠文件系统权限(只有 daemon 的用户能连),缺省不握手,一样是"能连上就全权"。
唯一的例外是 daemon 自己起的进程:对端是 daemon 的子孙(模型调用的 `Bash`、hook、stdio MCP
服务器,以及它们留在后台的进程)时,daemon 先发一帧
`{"jsonrpc":"2.0","id":null,"error":{"code":-32003,…}}` 再关连接,一个请求都不读。它们和宿主
是同一个用户,文件权限分不出来,而模型的命令要是能当宿主,就能自己信任自己的项目、自己批准
自己的询问。沙箱里的命令另外连不上 socket(macOS 上也连不上 TCP / WebSocket 端口),见
`ARCHITECTURE.md` §6.3。宿主要嵌 daemon 又要让自己起的子进程连它的,得另开一个不由 daemon
进程起的客户端。

**`--unix-socket-auth`(缺省关)让 unix socket 也握手**,第一帧同上必须是 `daemon.auth`,拒绝
方式也相同。token 取 `--token` / `AGIX_DAEMON_TOKEN` 给的那个,再没有就是 TCP / WebSocket 监听在用的那个;都没有
就每次启动新生成一个 64 位十六进制串,写进 `~/.agix/daemon/auth/<instance>.token`(`0600`,
目录 `0700`,沙箱里的命令读不到),路径记在实例文件的 `token_file` 里(§2;`unix_socket_auth` 与 `token_file` 两个键只在开了这个开关、或 token 是生成的时才写),daemon 退出时删除。
自己起 daemon 的宿主用环境变量传 token,不落盘;连别人起的 daemon 的客户端读 `token_file`。
实例文件里 `unix_socket_auth` 为真时就要握手。这个开关补的是"命令在沙箱外又 `setsid` 离开了
进程组"这个缺口,它挡得住什么、挡不住什么见 `ARCHITECTURE.md` §6.3。

#### WebSocket 的 Origin 校验

**绑定回环挡不住网页**:WebSocket 不受同源策略约束,用户浏览器打开的任何站点都能
向 `ws://127.0.0.1:<port>` 发起连接。所以升级握手阶段还要看 `Origin`:

| `Origin` | 结果 |
|---|---|
| 缺失(CLI、编辑器等非浏览器客户端) | 放行,进入 token 握手 |
| `http(s)://localhost` / `127.0.0.1` / `[::1]`,任意端口 | 放行,进入 token 握手 |
| 其它 | 升级失败,`403 Forbidden` |

端口不校验 —— 前端跑在哪个端口是部署细节。浏览器不允许页面伪造 `Origin`,这个判断
才成立;非浏览器客户端不发这个头,而缺失不构成任何证据,所以照常走 token 握手。

### 5.2 一条连接上的并发

**允许多个请求同时在途。** 每个请求在自己的任务里处理,读循环从不等待任何一个,
所以响应**可以乱序返回** —— 客户端必须按 `id` 匹配,不能假设先发先回。

**这条对长 turn 同样成立,而且正是它最关键的地方。** `session.run_turn` 一跑就是
几分钟,服务端这期间照常读取并处理同一条连接上的其它请求 —— 所以在跑 turn 的那条
连接上可以回答它自己的权限提问、可以 `session.interrupt` 把它停掉。浏览器一个 tab
只有一条 WS,全部交互都在上面,靠的就是这一条。

流式帧不会占住连接:每个 `session.event` 是独立的一帧,两帧之间随时可以插别的东西。

单条连接同时在途的请求数上限 **64**,超过返回 `TOO_MANY_IN_FLIGHT (-32017)`。
**是拒绝而不是排队**:排队就得让读循环停下来等,而"读循环不停"正是上面那条保证的
全部来源。

`id` 由客户端生成,在同一连接内必须唯一,数字或字符串均可。

**通知(无 `id`)只对无返回值的方法有意义**。对 `session.run_turn`、`session.create`
这类需要返回值的方法发通知是未定义行为:服务端会执行但不回响应。

### 5.3 同一会话的并发 `run_turn`

**一个会话同一时刻只能有一个 turn。** 第二个 `run_turn` 怎么办由它的 `on_busy`
决定:

| `on_busy` | 行为 |
|---|---|
| `"reject"`(缺省) | 立刻失败,消息不投递 |
| `"queue"` | 等正在跑的 turn 结束,再作为新的一轮执行;调用一直挂着,和普通 `run_turn` 一样流式返回 |
| `"steer"` | 把消息交给正在跑的 turn,它在**下一个步骤边界**(一批工具结果之后、下一次调模型之前)加进对话;调用立刻返回 |

`reject` 的错误:

```jsonc
{ "error": { "code": -32015, "message": "session is busy",
             "data": { "session_id": "S1", "reason": "turn_running", "current_turn_id": "t1" } } }
```

会话正在原地重建(`session.setPrompt`、换厂商或推理设置的 `session.setModel`、配置变更后下一轮开始前)时也是这个
错误,`reason` 为 `"rebuilding"`、`current_turn_id` 为 `null`;`queue` 等重建结束,`steer`
没有可以交给的那一轮,同样等重建结束再作为新的一轮执行。`reason` 的取值见 §9.1。

错误发生**在用户消息被投递之前** —— 消息不会被前一个 turn 吃掉。

`steer` 被接下时立刻返回 `{"session_id","turn_id","delivery":"steer","into_turn_id"}`,
`turn_id` 是这条消息自己的 id,`into_turn_id` 是接下它的那一轮。之后:

- 消息真正加进对话时,那一轮推一帧 `user_message_delivered`(§7)。模型已经答完、只差
  收尾时到达的消息也会被读到 —— 那一轮会再调一次模型,不会留到下一轮。
- 那一轮在读到它之前就结束了(被 `session.interrupt`、预算、策略截停或出错),消息
  **不进 transcript**,以一帧 `user_messages_returned` 交还,要不要重发由客户端决定。
- 配了 `UserPromptSubmit` hook 时,消息在加进对话前同样过它(hook 输入里 `delivery` 为 `"steer"`,
  开一轮的消息为 `"turn"`)。改写了的,`user_message_delivered.content` 是改写后的文本;被拒的
  **不进 transcript**,那一轮照常继续,推一帧 `user_message_refused` 带回原文与 hook 的 `message`——
  再发一遍还会被拒,所以不走 `user_messages_returned`。

正在跑的 turn 已经过了最后一个步骤边界时,`steer` 退化成 `queue`。带 `attachments`
的消息不走 `steer`,直接按 `queue` 处理。

### 5.4 事件帧推给谁

`session.event` 帧推给**所有订阅了该会话的连接**。

**连接不是会话的所有者。** 一个会话可以被任意多条连接同时观看,它们收到同一份帧流。
发起 `run_turn` 的那条连接没有特殊地位,它只是被自动加进订阅者集合而已。

| 订阅来路 | 说明 |
|---|---|
| `session.subscribe` | 显式订阅 |
| `session.run_turn` | 发起方自动订阅 |

退订用 `session.unsubscribe`;连接断开时它的全部订阅一起消失,**别的什么都不动**。

**关于慢客户端**:每条连接前面有一个 1024 帧的出站队列。某个会话的帧广播过来时队列
已满,这条连接**对这个会话的订阅被取消**(连接本身和它的其它订阅不动)—— 一个卡住的
tab 不能拖住 turn,也不能拖住其他正在看的 tab。被取消的一方会收到一帧
`subscription_dropped`(§7),它排在取消之前已经入队的全部帧之后;收到之后重新
`session.subscribe` 并靠 `last_seq` 追平(§5.6)。

`daemon.event`(§8)是另一条通道:与会话订阅无关,推给所有
`daemon.subscribeEvents` 的订阅者。

### 5.5 事件顺序保证

| 保证 | 说明 |
|---|---|
| 同一 turn 内 `text_delta` 严格有序 | 按到达顺序拼接即得完整文本 |
| 每个 `tool_result` 必有先行的同 `id` `tool_use` | 反之不然:被取消的工具可能没有 result |
| `turn_complete` 是本 turn 最后一个事件 | 其后只有 `run_turn` 的最终响应 |
| 每个开始了的 turn 都有且只有一个 `turn_complete`(出错时发起者收到 `run_turn` 的错误响应,其他订阅者收到 `error` 帧) | 被 hook 拦下、被中断、被预算或策略截停的 turn 同样以它结束;被 `session.close` / `session.delete` 截断的,其他订阅者收到 `code` 为 `session_closed` 的 `error` 帧,发起者收到 `ENGINE_ERROR`(`message` 以 `session_closed:` 开头) |
| 一个 `host_tool_call` 在它的 `tool_use` 之后、`tool_result` 之前;它的 `host_tool_call_resolved` 在那个 `tool_result` 之前 | 子 Agent 发起的调用经父会话发出,相对于 `subagent_progress` 里镜像的那次 `tool_use` 不保证先后,按 `task_id` 归属 |
| 收到 `persisted` 为 `true` 的 `turn_complete` 时本轮已写进 transcript | 此刻读 `session.history` 能看到这一轮;`persisted` 为 `false` 时见 §7.2 |

> **⚠️ 按 `id` 配对,不要按位置配对。** 并发安全的工具是并行执行的,谁先跑完谁先发
> `tool_result`,所以事件流里 `tool_result` 的顺序与 `tool_use` 的顺序**可以不同**。
> 而 `session.history` 里的块是按 transcript 顺序还原的。按 `id` 配对的客户端在
> 两边都对,按数组下标配对的客户端在事件流里会错。

**子 Agent 事件**以 `subagent_progress` 包裹,其 `event` 字段是嵌套的同构事件:形状与 §7 表中的同名事件一致。
父与子的事件在同一条流上交错出现,靠 `agent_label` / `agent_session_id` 区分归属。

### 5.6 断线与重连

**turn 进行中断线 → 什么都不会被取消。** turn 跑完、落盘,会话按正常的空闲超时
回收。断开只意味着这条连接的订阅没了。

**重连/新开 tab 的追赶顺序(不能颠倒)**:

```
1. session.subscribe {session_id}       → 拿到 last_seq,此刻起的实时帧进本连接
2. session.history  {session_id, ...}   → 读到 last_seq 为止的内容
3. 把 1 之后缓冲的实时帧接在后面
```

**先读历史再订阅会永久丢掉中间那段帧** —— `last_seq` 存在的唯一理由就是让这个接缝
可闭合。`last_seq` 是 transcript 里的**消息数**,和 `session.history` 的
`offset` / `total`、每条消息的 `seq`、`step_end` / `turn_complete` 帧的 `seq` 是同
一套坐标:读到 `offset + n == last_seq` 就追平了,之后的实时帧接着往下。压缩会让消息
数变小(§7.1),所以它是水位,不是只增不减的计数。

**没有"续接流"**:重连不会补发上一个 turn 已经推过的帧。已产生的内容都在 transcript
里,用 `session.history` 取 —— 追赶靠历史,不靠重放。

任务的输出不进 transcript,按字节偏移追赶:`task_progress` 帧带 `from_byte` / `next_byte`,
断线或订阅被取消期间漏掉的,从手上的 `next_byte` 起用 `session.taskOutput {from_byte}` 补齐
(§6.3 `session.taskOutput` 的跟随做法)。

**连接级的东西不跟着会话走。** `daemon.setHostTools` 提供的工具与 `daemon.setHooks` 登记的 hook
属于连接:连接一断就没了,重连之后要重新调用这两个方法(§6.1)。它们悬着的调用在断开时立刻结束。

**心跳**:daemon 不主动发心跳。空闲连接靠 TCP keepalive 或客户端自己的
`daemon.ping` 维持。WebSocket 传输会自动回应 ping 帧,但浏览器的 WebSocket API
发不了 ping —— 网页端要保活就用 `daemon.ping`。

**会话回收**:超过 `app.session_idle_timeout_secs`(`--session-idle-timeout`,默认 3600 秒)没活动的会话,以及池子
满(`app.session_cap`,`--session-cap`,默认 32)时最久未活动的那个,运行体会被回收;两个值在 daemon 启动时读定,`config.reload` 不改,改了的报在它的 `restart_required`。transcript 不
受影响,下次 `run_turn` / `session.resume` 会自动从磁盘恢复,`plan` 模式与 todo 列表也一起
回来(§4.5)。

---

## 6. 方法

### 6.1 `daemon.*`

#### `daemon.ping`

无参。

```jsonc
// ← result
{ "pong": true, "version": "…", "protocol_version": 5,
  "capabilities": ["on_busy_steer", "subscription_dropped_notice", "…"],
  "modules": ["mcp", "agents", "skills", "scripts", "plugin-packages"] }
```

`modules` 列出这个构建编入的模块,按装配顺序:`mcp`、`agents`、`skills`,以及载体与包
方案(`scripts`、`plugin-packages`)。上面是默认构建的;少编一个模块的构建就不列它,只含
内核的构建(`minimal` 档)为空。
属于某个模块的方法在没编入它的构建里返回 `METHOD_NOT_FOUND`,`error.data` 为
`{"module": "<id>"}`;目前这样的方法是 `mcp.*`(模块 `mcp`)与 `session.startAgent`(模块
`agents`)。`plugin.*` 在没有包方案
时仍返回 `PLUGINS_DISABLED`。

`capabilities` 列出协议版本之外、可以单独检查的能力。只会增加;客户端忽略不认识的项。

| 能力 | 意思 |
|---|---|
| `on_busy_steer` | `session.run_turn` 接受 `on_busy:"steer"`(§5.3) |
| `subscription_dropped_notice` | 订阅因为跟不上被取消时会收到 `subscription_dropped`(§5.4) |
| `session_lifecycle_events` | `daemon.event` 报会话进出内存与每一轮的开始结束(§8) |
| `events_lagged_notice` | `daemon.event` 送不到时会收到 `events_lagged`(§8) |
| `schedule` | 有 `schedule.*` 方法与 daemon 内的调度器(§6.7) |
| `session_tasks` | 有 `session.tasks` / `session.stopTask`,任务事件进 `session.event`(§6.3、§7) |
| `context_edits` | 有 `session.inspectContext` / `session.dropMessages` / `session.editMessage`(§6.3) |
| `extension_entries` | 有 `session.appendEntry` / `session.entries`,写入推 `extension_entry`(§6.3、§7) |
| `task_output` | 有 `session.taskOutput`,Shell 命令与监视器的输出边跑边记(§6.3) |
| `tool_progress` | 运行中的工具调用推 `tool_progress`,每次调用每秒至多一帧,带 `dropped`;子 Agent 的包在 `subagent_progress` 里(§7.3) |
| `task_progress` | 任务边跑边推:`task_progress` 推 Shell 命令、监视器与 `RunCode` 程序新增的输出(程序的是 `log()` 行与每次子调用的一行),`task_updated` 推子 Agent 的用量与等待;`subagent_progress` 带 `task_id`(§7) |
| `task_output_offsets` | 任务输出按字节偏移:`session.taskOutput` 接受 `from_byte`,应答带 `from_byte` / `next_byte` / `dropped` / `final`;`task_progress` 帧带 `from_byte` / `next_byte`;被停掉的任务等工作收尾才发 `task_finished`(§6.3、§7) |
| `task_messages` | 有 `session.messageTask`,消息进运行中的后台子 Agent(§6.3) |
| `host_agents` | 有 `session.startAgent`,宿主不经模型在会话下起子 Agent;子 Agent 任务带 `usage`、`structured_output`、`detail.tag`(§6.3) |
| `task_model` | 子 Agent 与团队成员的任务 `detail` 带 `model` / `provider`(登记时是预测,每次调用后按实际改写,经 `task_updated` 推出);`step_end` 带 `served`;侧链的 `session.get` 带 `model` / `provider`(§6.3、§7) |
| `agent_token_budget` | `session.startAgent` 收 `token_budget`:子 Agent 自己的 token 上限,挂在会话的上限下面;用完时任务以 `failed`、`error: "budget_exceeded: …"` 结束(§4.8、§6.3) |
| `subagent_provider` | 子 Agent 可以有自己的 provider:`session.startAgent`、`Agent` 工具与 agent 类型收 `provider`,有确定归属的模型 id 从它的归属起(§6.3) |
| `plugin_call_long` | `plugin.call` 的 `timeout_ms` 至多 600000(缺省仍 120000),这次运行的时限就是宿主给的值;模型调的插件命令仍是两分钟(§6.3) |
| `prompt_task` | 子 Agent 发起的 `prompt` 帧(三种 `prompt_type`)带 `task_id` 与 `agent_session_id`;子 Agent 里的权限询问带 `tool_use_id`(§7.3) |
| `goals` | 有 `session.goal` / `session.setGoal` / `session.updateGoal`,目标变化推 `goal_changed`(§6.3、§7) |
| `question_prompts` | `session.create` 接受 `options.prompt_types`;列了 `question` 的会话把模型的 `AskUserQuestion` 推成 `prompt_type:"question"` 的 `prompt` 帧,用 `session.respondToPrompt` 回答(§6.3、§7) |
| `session_attended` | `options.attended` / `options.auto_name`:会话自己定有没有人值守、起不起名,省略时取场景的;带 `output_schema` 的会话一定有 `StructuredOutput`,场景用 `tools.deny` 拒掉它时 `INVALID_PARAMS`(§6.3) |
| `mcp_elicitation` | `options.prompt_types` 认 `mcp_elicitation`:MCP server 在工具调用中向用户的询问推成 `prompt_type:"mcp_elicitation"` 的 `prompt` 帧,答案按 §4.7 回给 server(§6.3、§7) |
| `mcp_tasks` | MCP server 可以把工具调用转成任务(tasks 扩展):它作为 `kind:"mcp_task"` 列在 `session.tasks`、状态经 `task_progress` / `task_updated` 推出、`session.stopTask` 发 `tasks/cancel`(§6.3、§7) |
| `session_export` | 有 `session.export`,把会话连同侧链、附件打成一个归档写到宿主给的路径(§6.3) |
| `session_import` | 有 `session.import`,把 `session.export` 的归档装成新会话(§6.3) |
| `checkpoints` | 有 `session.rewind` / `session.checkpoints`,文件工具的每次写都记下写前内容,宿主可以把文件、对话或两者退回到之前的一轮;回滚推 `rewound`(§6.3、§7) |
| `session_calls` | 有 `session.calls` / `session.call` / `session.callBlob`,经协议读会话的模型调用录像;录制的 `recorder` 随会话保存(§6.3) |
| `calls_digest` | `session.calls` 的 `input.system[].digest` 与 `input.tools_digest`(§6.3) |
| `calls_preview` | `session.calls` 的 `output.text_preview`、`output.tool_uses[].input_preview` 与 `result_is_error`(§6.3) |
| `session_rerun` | 有 `session.rerunCall`:录像里的一次调用改过系统块或删过消息后重发给真实 provider,答新应答与差异,不写录像、不改会话(§6.3) |
| `session_search` | 有 `session.search`,按内容找会话(§6.3) |
| `session_skill_dirs` | `options.skill_dirs` 与 `session.setSkillDirs`:会话自带的宿主 skill 目录;`session.get` 带 `skill_dirs`,`commands.list` 接受 `session_id`(§6.3) |
| `session_settings` | `options.settings`:按会话给的设置覆盖层(形状同 settings.json,只收按会话决定的键,推理强度、思考开关与提示词都在这里),缺省只能收紧,放宽的被夹住并报在 `session.create` 的 `clamped_options`;`session.setModel` 的 `effort` / `thinking` 与 `session.setPrompt` 改的是同一层,被夹住的报在各自结果的 `clamped_options`;一轮的 token 预算与 `turn_complete.usage` 含子 Agent 与旁路调用,带 `usage_breakdown`(§4.8、§6.3、§7.2) |
| `host_tool_progress` | 有 `session.reportToolProgress`,宿主工具的进度推成 `tool_progress`(§6.3、§7.3) |
| `host_tool_provider` | 有 `daemon.setHostTools`,连接提供的工具,`host_tool_call` 一定发给提供者(§6.1、§7.4) |
| `session_plugins` | `options.settings.plugins.allow` / `enabled`:按会话收窄插件(只能收紧,与池里启用的包取交集再补依赖);被排除的包不对会话贡献脚本、命令、技能、agent 类型、函数、MCP server 与 `plugin:<名>` 场景;`plugin.functions` / `plugin.call` 带 `session_id` 时按会话筛;`session.inspectContext` 带 `plugins`(§6.3) |
| `host_tool_permit` | `options.host_tools` 与 `daemon.setHostTools` 的条目认 `permit`(`"ask"` / `"allow"`):`allow` 只放行这一个宿主工具的调用,规则与模式上限照旧压过它(§6.1、§6.3) |
| `host_hooks` | 有 `daemon.setHooks` / `session.respondHook`,连接登记的 hook,`hook_call` 帧只发给登记者(§6.1、§7.6) |
| `provider_tiers` | `config.setProvider` 接受 `tier`(`project` / `global`)与 `dry_run`,结果带 `tier`、`dry_run`(§6.4) |
| `host_call_context` | `host_tool_call` 与 `hook_call` 帧带 `cwd`(发起调用的会话的工作目录)与 `depth`(根会话 0,子 Agent 逐层加 1);hook 的 payload 带 `depth`(§7.4、§7.6) |
| `config_explain` | 有 `config.explain`:一个设置键为什么是现在这个值,逐层列出值与结局(§6.4) |
| `compact_events` | 每次改写历史的压缩都有首尾两帧:动手前一帧 `compact_started`,压成了以 `compact` 收尾,没压成以 `compact_ended` 收尾;子 Agent 的经 `subagent_progress` 转出(§7.1) |
| `prompt_append_blocks` | `prompt.append`(`options.settings`、`session.setPrompt`、设置文件)也收 `[{"name","text"}]`:每项一个系统块,`tokens` 各算各的。块名与 origin 按给出这个值的那一层:宿主给的(`options.settings`、`session.setPrompt`、`agixd --set` / 嵌入的 `EmbedConfig.settings`)是 `host.<name>`,`session.inspectContext` 的 `origin` 为 `{"host":"<name>"}`,`session.call` 的为 `host:<name>`;设置文件(global / scene / project,含 managed)给的是 `config.<name>`、`{"config":"<name>"}`、`config:<name>`(§6.3) |
| `mcp_oauth_jwt` | 这个构建能用 `oauth.private_key_jwt`(feature `mcp-oauth-jwt`);没有这一项的构建,配了 `private_key_jwt` 的 MCP server 连接时报明确的错(`docs/mcp_auth.md`) |
| `ptc` | 这个构建带 `RunCode`,模型能写程序调本会话的工具(`docs/ptc.md`) |
| `runext` | 这个构建带插件命令:包的 `[[command]]` 经 `RunExt` 与 `/name args` 跑,`plugin.list` 带 `commands` / `command_conflicts`,`commands.list` 有 `kind:"ext"`,任务有 `kind:"ext"`(`docs/runext.md`) |
| `runext_subcall_prompt` | 有人值守的会话里,模型或用户跑的插件命令的子调用要问时照常发 `permission` 提问,帧上带 `origin`(`docs/runext.md` §6);没有这一位的引擎上这类子调用直接失败 |
| `plugin_functions` | 有 `plugin.functions` / `plugin.call` / `plugin.cancel`:宿主按名字调包的 `[[function]]`,拿回结构化结果(§6.5、`docs/runext.md` §12) |
| `plugin_call_hooks` | 不带会话的 `plugin.call` 也问连接登记的 hook:`hook_call` 帧发给登记者,`session_id` 是这次调用的临时会话,帧带 `plugin_call:{function,run_id?}`,用 `session.respondHook` 回答(§6.1 `daemon.setHooks`、§6.5 `plugin.call`、§7.6) |

#### `daemon.status`

无参。

```jsonc
// ← result
{ "version": "…",         // daemon 二进制版本
  "protocol_version": 5,
  "capabilities": ["…"],  // 同 daemon.ping
  "modules": ["…"],       // 同 daemon.ping
  "uptime_secs": "…",     // 数字,秒
  "sessions": 0 }         // 当前内存里的会话数(含侧链)
```

#### `daemon.doctor`

只读自检:三份 settings.json 各自的状态、provider 路由的解析结果、hooks 配置、
记忆存了多少,外加这个进程的几项装配事实。不做任何修复,也不发起任何网络探测 —— `HealthCheck`
契约要求每项检查从已经握着的状态立刻回答。

下面是一个全新装配、三份 settings.json 都还不存在时,引擎自带的十一项检查与顶层字段,逐字段为真
(`path` 因机器而异,`build.panic` 的文字随构建的 panic 策略而变,`network.proxy` 的 `mode` 与 `variables` 随设置与环境而变(`variables` 例中只写了空数组),其余是字面值;daemon 自己的会话池
与插件包、宿主另有检查,见下文):

```jsonc
{ "scene": "code",
  "health": {
    "status": "…",                    // ok | degraded | failing,取所有检查里最差的一项
    "checks": [                       // 每项的 details 与下面同名的顶层字段是同一份数据
      { "name":"settings.tiers",     "status":"ok", "summary":"every settings.json present parses" },
      { "name":"settings.providers", "status":"ok", "summary":"the providers route" },
      { "name":"settings.hooks",     "status":"ok", "summary":"hooks configuration parses" },
      { "name":"memory",             "status":"ok", "summary":"0/200 memories",
        "details": { "enabled":true, "count":0, "cap":200 } },
      { "name":"settings.permissions.mode",  "status":"ok", "summary":"permission mode asks or restricts",
        "details": { "mode":"default" } },
      { "name":"settings.hooks_fail_open",  "status":"ok", "summary":"no hooks configured" },
      { "name":"sandbox.network_allowlist", "status":"ok", "summary":"sandbox.network.mode is not allowlist" },
      { "name":"sandbox.network.proxy",     "status":"ok", "summary":"sandbox.network.mode is not proxy" },
      { "name":"app.system_proxy",          "status":"ok", "summary":"…",
        "details": { "mode":"…", "variables":[] } },
      { "name":"build.panic",               "status":"ok", "summary":"…",
        "details": { "panic":"…" } },
      { "name":"sandbox.strict_profile",    "status":"…",  "summary":"…" } ] },
  "settings_tiers": [
    { "tier":"global",  "path":"…/settings.json",       "exists":false, "parses":true, "error":null, "unknown_keys":[], "renamed_keys":[], "ignored_values":[] },
    { "tier":"scene",   "scene":"code", "path":"…/settings.json", "exists":false, "parses":true, "error":null, "unknown_keys":[], "renamed_keys":[], "ignored_values":[] },
    { "tier":"project", "path":"…/.agix/settings.json", "exists":false, "parses":true, "error":null, "unknown_keys":[], "renamed_keys":[], "ignored_values":[] } ],
  "providers": { "ok":true, "configured":["anthropic"], "order":["anthropic"],
                 "tasks": { "main":"strong", "subagent":"balanced", "compact":"fast",
                            "memory":"fast", "title":"fast", "web_search":"fast" },
                 "error":null },
  "hooks": { "configured":false, "ok":true, "error":null },
  "session_persistence": { "history_store_wired":true },
  "plugins": { "status":"enabled" },
  "permission_rules_count": 0,
  "model": { "model_name":"claude-sonnet-4-6", "provider":"anthropic" },
  "runtime": { "sessions":[], "stopping_engines":0 } }
```

`health` 是 `HealthCheck` 契约(`docs/extension_points.md`)的报告:引擎自带的十一项
(上面例子里的)在前,daemon 自己的会话池、插件包的检查随后,宿主注册的检查排在最后,都在同一个 `checks` 里。状态是三值而不是布尔 ——
`degraded`("还能用,但有人该知道")与 `failing`("这块不能指望了")是两件事。

引擎自带检查之外的每一项(会话池、插件包、宿主注册的)各有 5 秒:超时的那一项报 `degraded`、
summary 为 `did not answer within 5s`,报告不再等它;panic 的检查同样报 `degraded`。

`settings_tiers` / `providers` / `hooks` 是 `health.checks` 里同名检查 `details`
的副本,放在顶层是因为现有客户端读的是它们。

`memory` 数的是会话用的记忆(全局与项目两个目录合起来)。到了上限(200 条)是 `degraded`:
轮末的记忆抽取不再添新的,已有的一条不删,要腾地方得有人删掉一些。`memory.enabled` 为
`false` 时是 `ok`,`details.enabled` 为 `false`。

`settings_tiers` 依次是全局层、每个激活场景的场景层(`scene` 是哪个场景;daemon 启动的场景在前,
其余按 id 排)、项目层,daemon 带着 `cli` 层(`agixd --set`、`EmbedConfig.settings`)时最后还有一项
`{ "tier":"cli", "path":"cli", "exists":true, "parses":true, …, "keys":[ "permissions.mode" ] }`:`keys` 是这一层
设的键(点分),值用 `config.get {tier:"cli"}` 读。场景层里只有启动场景那一项带 `ignored_values`,其余的只报存在、能否解析与
`unknown_keys`;哪一层读不了时 `summary` 写作 `scene (<id>)`。

`settings_tiers[].unknown_keys` 是那份文件里没有任何设置读的顶层键(拼错的、别的工具的),
加载时被忽略,daemon 日志里各有一条 warn。有这样的键时 `settings.tiers` 仍是 `ok`,
`summary` 里写明是哪几层。

`settings_tiers[].renamed_keys` 是那份文件里改过名、不再读的旧键:`{ "old", "new" }`。有这样的键时
`settings.tiers` 是 `degraded`;其中有旧键原本管着一道保护(权限规则与模式、沙箱、技能里的命令、
MCP 开关、项目信任)时是 `failing`——它不再读,那道保护就没了。`summary` 列出 `旧 → 新`。

`settings_tiers[].ignored_values` 是这一层(`settings.json` 与 `settings.local.json`)里类型
不对、因而被丢掉的值:`{ "tier", "origin", "key", "error" }`,`key` 是点分的
(`sandbox.network.mode`),一个对象里的值都不对时逐个报它们自己的键,只有这里根本不该是对象时才报
这个对象本身(`web.allowed_domains` 写成了对象);`permissions.mode` 写了只能由程序设的模式也列在
这里;`permissions` 的规则串逐条读,读不出来的一条是 `permissions.allow[0]` 这样带下标。被丢的只是这个值,同一个文件里
其余的值和别的层照常生效。`cli` 层的值在 daemon 自己那一对上放不进时 daemon 根本起不来,别的项目上放不进的
(与那个项目的设置合起来才不对)照样丢掉,`tier` 与 `origin` 都是 `cli`。`error` 按 `config.explain` 的规则不带凭据。有这样的值时 `settings.tiers` 是 `degraded`,`summary` 列出
`<层> (<键>)`。

`providers.ok` 只说明配置**形状**解析得通(每个 `tiers` 里的模型都在它的 `models` 里、`model.order` 里的每个 id 都指向一个存在
的 provider),不说明真能建出客户端,更不说明那个端点能连上 —— 缺凭据、`base_url`
指向一个没人监听的地址,这里都仍然是 `ok: true`。这项检查从已经握着的设置立刻回
答,不发起任何网络探测。

`runtime` 是此刻内存里每个会话在做什么,只报数,不判阈值(多久算卡住由读的人定):

```jsonc
"runtime": {
  "sessions": [                        // 按 session_id 排序
    { "session_id":"…",
      "phase":"running",                 // 同 session.get 的 turn_state:idle | running | rebuilding | operating
      "generation":3,                    // 第几代引擎;每次原地重建换下一代
      "turn":{ "turn_id":"t2", "running_ms":41250 },   // 只在 running 时有值,否则 null
      "log":{ "behind":false },          // 上一轮写日志失败(turn_complete 报 persisted:false,或以 error 结束时同样没写进去)、还没补写上(§7.2)
      "last_failure":{ "kind":"auth" } } // 最近一次失败的一轮,kind 同 §9.1;没失败过为 null
  ],
  "stopping_engines": 0 }               // 已移出内存、还没停下的引擎数
```

`plugins.status` 是这个二进制**能不能加载插件**:`enabled`、`disabled-by-policy`
(编进来了但配置关掉了)、`compiled-out`(根本没编进来)。

daemon 自己的会话池在 `health.checks` 里还有一项 `project.trust`,`details` 就是
daemon 自己的项目的 `project.status` 结果(§6.8):

```jsonc
{ "name":"project.trust", "status":"degraded",
  "summary":"project not trusted yet (only app.trust_all_projects decided); withheld until the host trusts it (project.trust): hooks, mcp.servers.github",
  "details": { "project_root":"/Users/x/repo", "trust":"untrusted", "matched":null,
               "trust_source":"default", "default":"untrusted",
               "requires_trust":["hooks", "mcp.servers.github"] } }
```

没有东西要信任才生效、或项目由 `app.trusted_projects` 明确信任时是 `ok`;项目未被信任而有东西被扣下,
或只靠 `app.trust_all_projects: true` 信任而有这样的东西时是 `degraded`,`summary` 列出这些键。

daemon 自己的会话池在 `health.checks` 里还有一项 `settings.managed`,说明部署方的 managed
设置(`ARCHITECTURE.md` §6.1"用户规则与部署方规则")在哪里找、找到了什么:

```jsonc
{ "name":"settings.managed", "status":"ok",
  "summary":"managed settings from /etc/agix/managed-settings.json: 1 permission rule(s), sets sandbox",
  "details": { "present":true,
               "path":"/etc/agix/managed-settings.json",   // 查找的路径(--managed-settings 或系统路径)
               "origin":"/etc/agix/managed-settings.json", // 这一层的来源
               "error":null,                               // 不能执行时的原因(URL 密码与像令牌的词遮住)
               "settings":{ "sandbox":{ "require":true },  // 它设的一切,设置文件的形状,按 config.get 的规则脱敏
                            "permissions":{ "deny":["Bash(curl:*)"], "disable_bypass":false } } } }
```

系统路径上没有这个文件时是 `ok`,`details` 只有 `{ "present": false, "path": "…" }`;用 `--managed-settings` 点名的文件不存在时是 `failing`(点名的路径不会悄悄当成没有 managed 设置)。文件读不了、
不是 JSON、有没有设置读的键(或改了名的旧键)、值放不进它的设置、有写不成规则的规则或写了引擎专用的权限模式时是
`failing`:这时它**一个值都不生效、所有工具调用都被拒绝**,任何权限模式都一样。它能设任何设置键,写了的键压过
设置文件、`agixd --set`、宿主在内存里改的值和会话的 `options.settings`(`ARCHITECTURE.md` §6.1)。

编入了 mcp 模块的构建,`health.checks` 里还有一项 `mcp.policy`(排在 `settings.managed` 之后、`project.trust` 之前),说明在执行
哪一份 MCP 策略(`ARCHITECTURE.md` §6.4):

```jsonc
{ "name":"mcp.policy", "status":"ok", "summary":"no MCP policy",
  "details": { "source":"none",         // none | host | file
               "path":null,             // source 是 file 时为那个文件
               "in_force":false,        // 有策略在执行(读不了也算:那时全部拒绝)
               "allowed_servers":null,  // 规则条数;null = 没写 allowed_servers
               "denied_servers":null,
               "allowed_authorization_servers":null, // 条数;null = 没写,任何授权服务器都行
               "enterprise_idp":null,   // 策略指定的企业 IdP 的 issuer
               "error":null } }         // 读不了时的原因
```

`source` 为 `file` 而文件不存在时(系统管理路径上没放策略)是 `ok`、`in_force: false`。
策略文件读不了或解析不了时这项是 `failing`,`health.status` 随之是 `failing`:这时所有
MCP server 都被拒绝。

引擎自带的另外几项是姿态检查,不看运行状态,只读设置与构建:`settings.permissions.mode` 在
`permissions.mode` 是 `bypassPermissions` 时是 `degraded`,否则 `ok`;
`settings.hooks_fail_open` 在有 `PreToolUse` / `UserPromptSubmit` / `Stop` 的 hook 的 `on_error` 是
`allow`(失败时放行)时是 `degraded`,`details.fail_open` 列出事件与条数;`app.system_proxy` 报引擎自己的 HTTP(模型调用、`WebFetch` / `Ping`、MCP over HTTP、OAuth)怎么走:
`details.mode` 是 `system`(跟随环境里的 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`,缺省)或
`direct`(`app.system_proxy` 为 `false`,不经代理),`details.variables` 是环境里已设置的代理变量名
(不含值);总是 `ok`。它与沙箱内命令的出网(`sandbox.network.mode`)是两回事。`build.panic` 说明这个
二进制是 `panic = "abort"`(工具 panic 会结束进程)还是 `unwind`(变成那次调用的错误),总是 `ok`。

插件包开着(`plugins.status` 是 `enabled`)时还有一项 `plugins.enable_state`:两层
`enabled.json` 都读得了(或不存在)是 `ok`;有一层读不了是 `degraded`,`details` 是
`{"tiers": [{"tier": "global", "fault": "…"}]}`——那一层的包这时全部按禁用算。

`plugins.refused`:磁盘上有发现阶段挡下来、因而不会加载的插件包(披露超限、manifest 读不了等)时是
`degraded`,`details.refused` 是 `[{name,version,reason}]`;`plugins.enable_on_install` 说明
`plugins.enable_on_install` 设置是真是假(`plugin.install` 是否装完就启用,缺省否),总是 `ok`。引擎名（`EmbedConfig.engine`）不是 `agix` 时还有一项
`plugins.engine_name`：已启用的包里，命令入口或 `[[script]]` 的源码写了 `agix.` 时是 `degraded`——
基础库在这个引擎里叫引擎名，这些调用跑不通；`details` 是 `{"engine": "…", "plugins": [{plugin, files}]}`。
只看源码文本，不运行。

daemon 自己的会话池还有一项 `sandbox.unattended_network`:这个 daemon 服务的场景里有无人值守的
(没人回答提问与权限询问),`sandbox.network.mode` 是 `unrestricted`,而
`sandbox.unattended_needs_network_policy` 被设成了 `false`(缺省 `true`)——三条同时成立时是
`degraded`:无人值守会话里沙箱完整生效的 `Bash` / `Monitor` 命令不问就跑,网络又是开着的;
`summary` 给出两种改法(去掉这项设置,或把 `sandbox.network.mode` 改成 `deny_all` / `allowlist`)。
其余情况是 `ok`。这一项按场景缺省的 `interactive` 报;`options.attended` 把某个会话改成有人或无人值守,不反映在这里。

```jsonc
{ "name":"sandbox.unattended_network", "status":"degraded", "summary":"unattended scenes (code) run …",
  "details": { "unattended_scenes":["code"], "network_mode":"unrestricted",
               "unattended_needs_network_policy":false } }
```

`sandbox.network_allowlist`:`sandbox.network.mode` 是 `allowlist`,而 `web.allowed_domains` 里有
回环(`localhost`、`127.0.0.1`、`::1`)以外的条目时是 `degraded`——两个 OS 沙箱后端都不按域名放行,
沙箱里的 `Bash` / `Monitor` 命令连不上这些域名(macOS 只放开名单里的回环,Linux 整个断网,
`ARCHITECTURE.md` §6.3);`sandbox.require` 开着时 `summary` 另外说明每条沙箱命令都会
被拒。其余情况是 `ok`。`details` 是 `{"unreachable_from_the_sandbox": [...], "require": false}`,
不是 `allowlist` 时 `details` 为 `null`。

`sandbox.network.proxy`:`sandbox.network.mode` 是 `proxy` 时,用一条探测命令问会话的沙箱能否把命令
只限到代理上:能是 `ok`;只能部分做到(macOS 上代理不在本机、只能按端口限;Linux 上找不到 `socat`、
中继起不来而整个断网;代理端口就是 daemon 自己的端口)是 `degraded`,`summary` 与 `details.unmet`
列出原因;`sandbox.network.proxy` 缺失或读不出(命令整个断网)是 `failing`。设了 `sandbox.network.proxy` 而模式不是
`proxy` 也是 `degraded`(地址被忽略)。`details` 是 `{"network_mode", "http", "socks", "enforcement",
"unmet"}`(后两项只在探测过时有);两者都没设时 `details` 为 `null`。

`sandbox.strict_profile`:自动放行的命令(只读通道)跑在严格 profile 里——断网、只能写会话的
`TMPDIR`、只能读可读根和系统目录、只带最小环境变量。这台机器的沙箱兑现不了它(没有后端、Linux 上没有
bwrap,或 `sandbox.enabled` 为 `false`)时是 `degraded`:通道照常放行,命令按会话自己的
约束跑。`details` 是 `{"enforcement": "full" | "partial" | "none", "sandbox_enabled": true}`。

#### `daemon.subscribeEvents`

无参。把当前连接标记为异步通知订阅者,此后推送 §8 的 `daemon.event` 帧,直到连接
关闭。**不补发过去的事件**(`import_detected` 例外,见 §8)—— 关心启动期通知的客户端要在连上后立刻订阅。同一条连接
再调一次照样回 `subscribed: true`,但每条通知仍只送一份。

```jsonc
// ← result
{ "subscribed": true }
```

#### `daemon.setHostTools`

宿主在**连接**上提供工具:每个会话都拿得到,不必逐个会话写进 `options.host_tools`。`host_tools`
的每一项和 `session.create` 的 `options.host_tools` 条目一模一样(§6.3)。这次调用**整个替换**
这条连接此前提供的全部工具,`[]` 撤回全部。

```jsonc
// → params
{ "host_tools": [
    { "name": "HostEcho",
      "description": "Echo text back from the host.",
      "input_schema": { "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"] },
      "read_only": true } ] }
// ← result
{ "host_tools": ["HostEcho"],     // 这条连接现在提供的工具名
  "refused": [] }                 // 被拒的条目,见下
```

逐条检查,坏一条只丢一条,其余照常采纳。被拒的条目进 `refused`:

```json
{ "host_tools": ["HostEcho"],
  "refused": [ { "index": 1, "name": "Read", "field": "name",
                 "error": "a built-in tool has this name" } ] }
```

`index` 是条目在 `host_tools` 里的下标;`name` 在读得出名字时才有;`field` 指出哪个字段不对,
条目根本读不出来(未知字段等)时没有,`error` 是原因。拒绝的原因:

| 情形 | `field` |
|---|---|
| 读不出来(未知字段、类型不对) | 无 |
| 声明本身不合规:名字格式或 `mcp__` 前缀、`input_schema` 不是对象或编译不了、`timeout_ms` 不在 1000..=600000 | 对应字段 |
| 与内建工具或引擎工具同名(`Read`、`Bash`、`Agent`、`Skill`……) | `name` |
| 同一次调用里同名两次(`declared twice`) | `name` |
| 超过每条连接 64 个(`at most 64 tools per connection`) | 无 |
| 别的连接已经提供了这个名字(先到的保留) | `name` |

语义:

- **只对此后建起来的引擎生效。** 工具在引擎构建时取一份快照,进每个此后才建的会话(新建、
  `session.resume`、重建);已经建好的会话保留它当时有的那份,直到它下一次重建。没有为此专门
  重建空闲会话。
- **不进会话日志,也不进 `options`。** 重建时拿的是那一刻登记表里有的。
- **会话自己的优先。** 会话的 `options.host_tools` 里有同名项时,这条连接的那个不进该会话
  (daemon 日志记一条 warn),会话照常建起来;与插件或场景工具重名也是只丢连接这一个、记 warn,
  不会让构建失败。
- `subagents:true` 的,子 Agent 也拿得到(同 `options.host_tools`);在父会话里因重名没进去的,
  子 Agent 也拿不到。一个会话里连接提供的工具至多 128 个,与 `options.host_tools` 的 64 个互不占用。
- **`permit:"allow"` 作用于这个 daemon 里此后建起的每个会话**,不论会话是哪条连接建的(放行规则同
  `options.host_tools`,§6.3)。它跟着工具走:重名没进会话的,放行也不落到同名的插件或场景工具上。
  改回 `ask` 或撤回工具同样只对此后建起的引擎生效;已经建好的会话和它此后起的子 Agent 沿用建起时
  那份,直到会话下一次重建(`session.setPrompt`、`session.setModel` 等)。要立刻收回,宿主对这些
  调用用 `session.respondToolCall` 回 `error` 即可(工具调用一定发给提供它的连接)。

连接提供的工具被调用时,`host_tool_call` 帧(§7.4)**一定**发给提供它的那条连接——即使它没有订阅
这个会话——同时照常发给该会话的订阅者。**只有提供者的** `session.respondToolCall` 算数,
别的连接回答 → `INVALID_PARAMS`。子 Agent 的调用用父会话的 `session_id` 回答。这些调用也在
`session.subscribe` 的 `pending_host_calls` 里。

**连接断开,它提供的工具就没了,重连后要重新调用本方法。** 断开时还没处理完的本方法也不算数:
连接一关,它就什么都登记不进来(回答发不出去,`refused` 里每一项都是 `this connection is closed`)。
投递帧时发现提供者的连接已经关了,也立即把它从登记表里摘掉。提供者断开时,它还悬着的调用以
`host_tool_call_resolved{withdrawn, reason:"host_gone"}` 结束;已建好的会话里还留着这个工具的,
之后的调用立刻以 `no_host` 结束;此后新建的引擎不再带它。`daemon.setHostTools` 传 `[]`
撤回,对已建好的会话之后的调用是同样的效果(立刻 `no_host`);撤回之前已经发出的调用不受影响,
提供者照样可以回答。

#### `daemon.setHooks`

宿主在**连接**上登记 hook:引擎走到指定的事件时,把那一刻的 payload 作为 `hook_call` 帧
(§7.6)交给这条连接,等它用 `session.respondHook` 回答。`hooks` 的每一项:

| 字段 | 说明 |
|---|---|
| `events` | 必填,非空;只能是 `PreToolUse`、`PostToolUse`、`PostToolUseFailure`、`UserPromptSubmit`、`Stop` |
| `tools` | 可选,工具名模式的列表,至多 64 个、不能有空串;缺省等于每个工具。只对工具事件起作用,`UserPromptSubmit` 与 `Stop` 忽略它。模式:`*`、`Prefix*`、完整名字、`mcp__<server>` / `mcp__<server>__*`、`mcp__*` |
| `timeout_ms` | 可选,1000..=30000,缺省 10000;宿主在这段时间里没回答就算失败 |
| `on_error` | 可选,`"allow"`(缺省)或 `"block"`:失败时放行,还是拦下 |

```jsonc
// → params
{ "hooks": [
    { "events": ["PreToolUse"], "tools": ["Bash", "Write*"],
      "timeout_ms": 10000, "on_error": "block" } ] }
// ← result
{ "hooks": [ { "index": 0, "events": ["PreToolUse"] } ],   // 采纳的条目
  "refused": [] }                                           // 被拒的条目
```

传 `[]` 或 `null` 撤回这条连接登记的全部。每次调用整个替换此前登记的。条目逐个检查,坏一条
只丢一条;被拒的进 `refused`,形状同 `daemon.setHostTools`(`index`、`field`、`error`):读不出来
(未知字段)、`events` 为空或含上面五个以外的事件(`field:"events"`)、`tools` 超限或含空串
(`field:"tools"`)、`timeout_ms` 越界(`field:"timeout_ms"`)、一条连接超过 16 条(第 17 条起
`at most 16 hooks per connection`)。daemon 里已经有 8 条别的连接登记着 hook 时,整个调用
`INVALID_PARAMS`。

**影响面:登记了 hook 的连接,看得到这个 daemon 里每个会话每次工具调用的输入,并且能拦下它们。**
hook 不限于哪个会话、哪个订阅者:`PreToolUse` 的 payload 带 `tool_input`,`UserPromptSubmit`
带用户消息的全文。只把它交给被信任的宿主,别把它当成"我只看自己的会话"的能力。

语义:

- **立即生效,包括已经建好的会话。** 登记表在每次触发时现读,`daemon.setHooks` 返回之后的
  下一次触发就走它。这一点与 `daemon.setHostTools` 不同。
- **与设置文件里的 hook 叠加,不被替换。** 同一事件上设置里的 hook 先跑;其中有一个拦下,
  就不再问宿主;否则宿主看到的是设置里的 hook 用 `updated_input` 改写之后的输入。详见
  [`hooks.md`](hooks.md)「宿主连接的 hook」。
- **帧只发给登记的那条连接**,不发给别的订阅者,也不在 `session.subscribe` 的待答列表里。
- `PreToolUse` 在权限检查之前,所以 `bypassPermissions` 下一样能拦,也不会有人被询问。覆盖子
  Agent 的调用,和 skill 的内联命令(`` !`cmd` `` 是一次 `Bash` 调用)。
- **也覆盖不带会话的 `plugin.call`**(哪条连接发起的都一样):它的临时引擎走到登记的事件,同样
  发 `hook_call` 给登记者。帧外层的 `session_id` 是这次调用的临时会话 id,帧上多一个
  `plugin_call:{function, run_id?}`(§7.6);用这个 `session_id` 回 `session.respondHook`。这个 id
  只在调用跑着时认得,调用结束(答完、超时、取消)之后再回是 `SESSION_NOT_FOUND`。
- 回答能做什么,见 `session.respondHook`。
- 失败(超时、连接没了、帧发不出去、这条连接上同时悬着的 hook 调用超过 256 个)按 `on_error`:
  `allow` 让调用继续;`block` 拦下,模型读到 `` Hook `PreToolUse` #1 failed and its on_error is block. ``
  (序号是它在这个事件的全部 hook 里的位置)。**拿 hook 当策略闸门的,写 `on_error:"block"`**:
  256 个的上限是这条连接所有会话共用的,别的连接起的会话多、调用密,也能把它占满,`allow` 之下
  多出来的调用就不经它直接放行。
- `Stop` 上 `block` 一次,这一轮就再跑一步;次数没有上限(设置里的 hook 也一样)。一直拦
  `Stop`,或在 `on_error:"block"` 下一直不答,这一轮就一直不结束、一直调模型。
- 一轮在 hook 等回答时被中断:`hook_call_resolved{withdrawn, cancelled}`,发给登记者。

**连接断开,它登记的 hook 就没了,重连后要重新调用本方法。** 它还悬着的 hook 调用立刻结束
(按 `on_error` 处理)。断开时还没处理完的本方法不算数,同 `daemon.setHostTools`:连接一关,
它就登记不进来了。

#### `daemon.shutdown`

无参(传什么都一样)。先停止接受新连接,再关掉所有会话的运行体,等它们停下(最多几秒)
后回响应,进程随后退出。

```jsonc
// ← result
{ "shutting_down": true }
```

**不删除任何 transcript,包括侧链的**(§3.3)。也没有"优雅等待"选项:正在跑的 turn
被直接取消。关停开始后,已经连着的连接上要新建或带回内存的会话(`session.create`、
`session.run_turn`、`session.resume` 等)一律报错 `the daemon is shutting down`。
SIGINT / SIGTERM 走同一条路。

### 6.2 `scene.*`

#### `scene.list`

无参。列出这个二进制注册的全部场景,包括未激活的。

```jsonc
// ← result
{ "scenes": [
    { "scene":"code", "name":"Agix Code", "description":"…",
      "active":true, "sessions":0,
      "requires_project":true, "supports_team":true } ] }
```

`sessions` 是当前内存里在这个场景下跑的会话数,有没有落盘的 transcript 都算。**数组顺序不稳定**(底层是
HashMap),客户端要按 `scene` 找条目,不要按下标。

#### `scene.activate`

```jsonc
// → params
{ "scene": "harnix" }
// ← result
{ "scene": "harnix", "active": true }
```

幂等。场景 id 不是这个二进制注册的 → `SCENE_NOT_FOUND`。

#### `scene.deactivate`

```jsonc
// → params
{ "scene": "harnix" }
// ← result
{ "scene": "harnix" }
```

该场景下还有**内存中的**会话时失败,`SCENE_HAS_ACTIVE_SESSIONS`,`message` 里列出
挡路的会话 id(没有结构化 `data`)。调用方先 `session.close` 再重试,不提供 `force`。

停用只是把场景移出激活集合,**不删除磁盘上任何内容**;它的历史会话仍然列得出来,
重新激活后仍可 resume。

#### `scene.reload`

无参。重新读 `<状态根>/scenes/<id>/` 下的声明式场景,重建场景表。

```jsonc
// ← result
{ "scenes": [ "code", "harnix", "triage", "…" ],  // 重建后全部可用的场景 id
  "added": [ "triage" ], "removed": [ ],
  "errors": [ "…" ] }                              // 没加载上的,逐条说明原因
```

已激活、重建后仍在的场景指向新的定义:改过 `prompt.md` 的场景,此后新建的会话用改过的版本;
已经在跑的会话保持建会话时的那份(提示词、工具面),只有首轮之后给会话起名用新定义的
`session_naming`。新增的场景照常需要 `scene.activate`。消失的场景从场景表
移除,已激活的仍需 `scene.deactivate`。

#### `scene.register` / `scene.unregister`

不落盘地登记一个声明式场景,只在这个实例里有效:

```jsonc
// → scene.register params
{ "scene": { "id": "triage", "name": "Triage", "tools": ["Read", "Grep"] },  // scene.json 的内容;对象或 JSON 文本
  "prompt": "把进来的问题分类。" }                                          // prompt.md 的内容
// ← result
{ "scene": "triage", "registered": true }

// → scene.unregister params
{ "scene": "triage" }
// ← result
{ "scene": "triage", "registered": false }
```

`scene` 按声明式场景的规则从严校验(格式见 `extension_points.md`「声明式场景」),不合格或 id
已被占用(内建、宿主、磁盘或已登记的场景)→ `INVALID_PARAMS`。登记不等于激活,要用就
`scene.activate`。`scene.unregister` 只撤 `scene.register` 登记的场景,别的 →
`SCENE_NOT_FOUND`;有内存中的会话在用时与 `scene.deactivate` 一样返回
`SCENE_HAS_ACTIVE_SESSIONS`。daemon 重启后登记的场景不在了。

#### `scene.describe`

```jsonc
// → params
{ "scene": "code", "project_root": null, "include_secrets": false }
// ← result
{ "scene": "code", "name": "Agix Code", "description": "…",
  "project_root": null,
  "active": true,
  "capabilities": { "requires_project": true, "supports_team": true },
  "tools": { "allowed": null, "disallowed": [], "deferred": [ "…" ] },
  "settings": { } }
```

`project_root` 省略与显式 `null` 在这里是一个意思(没有会话要绑,不需要区分)。

`tools.allowed` 为 `null` 而不是 `[]`,是因为空白名单的含义是"不限制";`[]` 会被
读成"一个工具都不给",正好相反。`deferred` 是"先只报名字,`ToolSearch` 取到 schema
才展开"的那批工具。

`settings` 是各层(三层设置文件与 `cli` 层)合并后的完整结果,**不带溯源信息** —— merge 之后不保留每个字段来自
哪层。要看某一层的原样内容用 `config.get` 的 `tier`,要看某个键为什么是这个值用 `config.explain`。

`include_secrets` 默认 `false`:`api_key` / `auth_token` / `token` / `secret` /
`password` / `authorization`,以及以 `_token` / `_secret` / `_password` 结尾的键(键名不分
大小写),值一律脱敏为末 4 位(任意嵌套深度);`headers` 与 `env`(MCP server 的请求头
与 stdio 环境变量)不论写成对象还是一个字符串,里面的值不论键名全部脱敏,只有顶层的 `sandbox.env`
不算(它列的是变量名,不带值);URL 里的密码换成 `***`(`https://u:***@host`)。

### 6.3 `session.*`

#### `session.create`

```jsonc
// → params
{ "scene": "code",                      // 可选,默认 daemon 的 --scene
  "project_root": "/Users/x/repo-a",      // 见 §3.1 的三态
  "session_id": "…",                      // 可选,用这个 id 建;已有就答那个会话,见下
  "options": {                            // 可选,只在创建时生效
    "settings": {                         // 可选,这个会话(含子 Agent、团队成员)的设置覆盖层,见下
      "permissions": { "mode": "default", "deny": ["Bash(rm:*)"] },
      "turn": { "token_budget": 2000000, "max_model_calls": 40 },
      "model": { "effort": "high", "thinking": "on" },  // low | medium | high | xhigh | max;auto | off | on | {"on_budget": N}(N ≥ 1024)
      "prompt": { "append": "…" } },     // 系统提示词:字符串,或 [{"name","text"}] 每项一块,见 session.setPrompt
    "recorder": { "mode":"replay", "name":"…", "dir":"/abs/recordings", "strict":true },
    "clock": { "start": "2026-10-28T09:00:00Z" },  // 可选,记下来的时间从这里起
    "telemetry": { "output":"/abs/path.jsonl" },
    "output_schema": { "type":"object", … },  // 结构化输出必须满足的 JSON Schema
    "attended": true,                     // 可选,省略 = 场景的 interactive
    "auto_name": false,                   // 可选,false = 首轮之后不起名
    "provider": "deepseek",               // 可选,providers 里的 id
    "model": "deepseek-chat",             // 可选
    "prompt_types": ["permission", "question"],  // 可选,宿主能回答的提问,默认只有 permission
    "skill_dirs": [ { "path": "/abs/skills" } ],  // 可选,这个会话自带的 skill 目录,见下
    "host_tools": [ ] } }                 // 可选,宿主自己实现的工具,见下
// ← result
{ "session_id": "…", "scene": "code", "session_kind": "primary",
  "project_root": "…",
  "project_trust": "untrusted",         // trusted | untrusted | none,§3.6
  "trust_source": "default",            // default | user | host | null,§3.6
  "created_at": "…",
  "existing": false }                   // true = session_id 已经是一个会话,答的就是它,没有新建;
                                        // options.settings 有被夹住的值时另带 clamped_options,见下
```

`scene` 必须是**当前激活**的场景,否则 `SCENE_NOT_FOUND`。场景的 `requires_project`
为真而 `project_root` 是 `null` → `PROJECT_REQUIRED`;给了路径但目录不存在 →
`PROJECT_NOT_FOUND`。

**`session_id`——建会话做成幂等。** 省略时引擎生成 id。给了就用它,格式必须是引擎自己的
id(BASE58 编码的 UUID,与引擎生成的写法一字不差),否则 `INVALID_PARAMS`。这个 id 已经是
一个会话(在内存里,或会话存储里有它的日志——daemon 重启过也算;sidechain 的 id 也算)时
不新建、不动它的转录:

- 它是主会话、`scene` 与 `project_root` 都和这次请求的一样、`options` 与建它时的一样 → 答那个会话,
  `existing: true`,`created_at` 是它当初建立的时间;不在内存里就像 `session.resume` 一样把它带回来,
  接着 `run_turn` 即可。
- 否则 → `SESSION_EXISTS`,`data` 带那个会话的 `scene` / `project_root` / `session_kind`,什么都不做;
  只是 `options` 不同时另带 `options_differ`,列出不同的字段名。

**`options` 怎么比。** 比的是建会话时给的那份(记在会话日志里,见下),不是此后 `session.setModel`、
`session.setPrompt`、`session.setSkillDirs` 改出来的当前值,所以原样重发建会话的请求总能得到 `existing: true`。逐字段比,
省略 `options` 与 `{}` 相同,列表字段连顺序一起比;给了与没给不同,任何一项不同都拒绝,不会悄悄忽略。
例外:

- `recorder` 是录制(`mode:"record"`)且这次没给 `dir`:建会话时省略的 `dir` 已经换成了当时的录制根目录,
  没给 `dir` 就算与记下的一致。
- 回放(`replay` / `rerun`)不记进日志。会话已经不在这个进程里(daemon 重启过、或被回收且日志能还原它)
  时无从核对,这次给的回放不算不同,并且照 `session.resume` 的规则用上,从头回放;会话还在这个进程里时
  照常比。
- 更早的版本建的、日志里没记 `options` 的会话,按建时没给 `options` 比。

两个连接同时用同一个 id 建,只有一个新建,另一个得到 `existing: true`(`options` 不同则是 `SESSION_EXISTS`)。
被 `session.delete` 删掉的 id 不算已有,可以再建。查找、核对与把会话带回内存在同一把会话锁里做,
同时到达的 `session.delete` 排在它前面或后面,不会让一个建会话的调用得到 `SESSION_NOT_FOUND`。
`session.import`(`keep_ids`)正在写这个 id 时 → `SESSION_BUSY`,`reason: "importing"`,稍后再试。

`options` 在新建时生效,并随会话保存:之后每次重建引擎(设置变化、空闲回收后再用、
daemon 重启后恢复)都按同一份 `options` 重建,之后的调用(`run_turn`、`session.resume`)里再传的
`options` 被忽略;`session.create` 带 `session_id` 时不忽略,与记下的不同就拒绝(见上)。
会话日志里没有记录过 `options` 的旧会话例外:恢复时采用这次调用带的那份。
`options` 记在会话日志里一条 `ns` 为 `agix.session`、`event` 为 `spec` 的扩展条目上,
`session.setModel` 选的模型与厂商、它与 `session.setPrompt` 改过的会话设置(`model.effort`、`model.thinking`、
`prompt.append`、`prompt.override`),`session.setSkillDirs` 设的 skill 目录也记在那里,所以同样跨重启保留。

`options` 逐字段读,读不出来的任何一项都让这次调用失败:`INVALID_PARAMS`,什么都不做(不建
会话、不跑这一轮),`error.data.invalid_options` 列出每一项。读不出来包括:不认识的字段、值读不出来的字段(如 `"attended": "yes"`)、`options`
本身不是对象,以及 `options.settings` 里会话不能给的键(见下)。`effort`、`thinking`(更早叫 `thinking_mode`)、
`prompt`、`turn`、`permission_mode`、`permission_rules` 是不认识的字段,`error` 写明换成了哪个
(`settings.model.effort`、`settings.model.thinking`、`settings.prompt.append` / `settings.prompt.override`、
`settings.turn`、`settings.permissions.mode`、`settings.permissions` 的规则串)。`prompt_types`、`host_tools`、`skill_dirs`
这几个列表字段逐条读,`field` 写成 `prompt_types[0]` 这样带下标;值本身不是数组时 `field` 不带下标。`field` 为 `null` 表示整个
`options` 读不出来。`session.create`、`session.run_turn`、`session.resume` 都这样。

```jsonc
{ "code": -32602, "message": "options that do not read: model, settings.hooks",
  "data": { "invalid_options": [
    { "field": "model", "error": "invalid type: integer `3`, expected a string" },
    { "field": "settings.hooks", "error": "it is decided for each project and scene, not each session: set it in the settings, not in a session's options" } ] } }
```

`recorder.mode` 取 `record` / `replay` / `rerun`(`rerun` 的会话自己直连 provider,不录也不放;把录像里的
一次调用改了重发用 `session.rerunCall`)。`dir` 是录像根目录,必须是绝对路径;省略时用
daemon 自己的录制根目录。`name` 是根目录下这份录像的目录名,必须是单独一段路径(不含 `/`、
不是 `.` / `..`);录制时省略就用会话 id。两项不合要求都进 `invalid_options`(`field` 为
`recorder.dir` / `recorder.name`)。

**录制随会话保存,回放不保存。** `mode:"record"` 的 `recorder` 和其余 `options` 一样记进会话日志:
建会话时省略的 `dir` 当场换成那时的录制根目录记下来,之后设置怎么改都录到同一处;daemon 重启后
会话回到内存(`session.resume`、自动恢复的 `session.run_turn`),不用再给 `recorder` 就接着录。
日志里已经记了录制的,这次调用带的 `recorder` 被忽略。`replay` / `rerun` 不记:同一进程里的重建
接着原来的回放进度,不从头重放;daemon 重启之后,把会话带回内存的那次调用在 `options` 里再给一次
才接着回放(`session.subscribe` 不收 `options`)—— 这是 `options` 里唯一以这次调用为准的情形。
从没记过录制的会话(包括更早的版本录制的),恢复时带上 `mode:"record"` 的 `recorder`,从此记下。

**一个会话一份录像。** 录制时,会话自己的对话调用、它起的子 Agent(`Agent` 工具、
`session.startAgent`)、首轮之后的起名、压缩等辅助调用,全都录进同一份录像,每次调用记着是哪个
会话发的(`session.calls` 的 `session_id` / `parent_session_id`)。`name` 给了就录在那个名字下,
同样包括子 Agent。录像只覆盖**最近一次运行**:进程里第一次调用时重写 `calls.jsonl`,所以 daemon
重启后会话的第一次调用把上一次运行的录像清掉重来(`session.calls` 的 `generation` 随之改变)。
`session.fork` 出的会话照样录制,但不带源会话的 `name`,录在自己的 id 下 —— 否则它的第一次调用
会把源会话的录像重写掉。

**录像不脱敏。** 系统提示、整段对话、工具参数与结果按发出去的原样记下,不经过遥测的脱敏
策略——录像要的就是"实际发了什么"。所以提示里带着的密钥录像里也有;录像目录与文件只对
属主可读,保管标准与它所录的 transcript 相同。

回放在每个会话里分两路对录像:对话调用按顺序对录像里的对话调用,跳过夹在中间的辅助调用,
严格回放查的顺序就是这一路;带 `purpose` 的辅助调用(压缩、记忆提取、起名……)取录像里同一
`purpose`、还没回放过的最早一条,不管它录在哪两次对话调用之间 —— 后台的辅助调用什么时候
完成每次都不一样。对上的那条照样比对请求内容。某个 `purpose` 的辅助调用录像里已经没有了,
回放报错,不会转去调真的模型;录像里有、这次没要的,结束时算回放没跑完。

首轮结束后的自动命名也走这个会话的录制器:录制时它是一条 `purpose` 为 `title` 的辅助调用,
回放时从录像里取。起名是上面两条的例外:录像里没有这条调用(录的时候场景不起名、或者是更早
的录像),回放的会话就不起名,`name` 留空,不会转去调真的模型,严格回放也不把这当成分歧;
录像里有、这次没要的起名调用留着不用,不算回放没跑完。`auto_name:false` 的会话根本不发起名调用,录像里没有 `title`,也就没有东西要录要放。

`clock.start`(RFC 3339)是这个会话"记下来的时间"的起点:提示里的日期、会话日志的时间戳
从它起、按墙上时钟往前走(`ShiftedEnvironment`)。不给时用机器的时钟 —— 回放(`recorder.mode`
为 `replay`)例外:从录像开录的时刻(录像头的 `created_at`)起,于是提示里的日期与录下来的
那天相同,严格回放(`strict:true`)不会因为换了一天而对不上。`start` 写错只记警告、照旧用
机器的时钟。

`output_schema` 给了,`StructuredOutput` 工具就以这份 schema 作为自己的参数格式直接
宣告给模型(不再需要先 `ToolSearch`),不符合 schema 的调用以错误结果退回、模型可以
改了再交;被接受的那一份出现在 `turn_complete` 的 `structured_output` 里(§7.2)。
带 `output_schema` 的会话一定有 `StructuredOutput`:它和插件的 `admit_tools`、宿主工具一样,照场景的
允许表放行,只有场景的 `tools.deny` 点了它的名才进不来。那样 `session.create`(`session.run_turn`
自动建会话、`session.resume`、重建同理)返回 `INVALID_PARAMS`,`error.data.invalid_options` 里是
`{"field":"output_schema","error":"scene \"x\" denies StructuredOutput, …"}`,不建一个交不回结果的会话。
子 Agent、`session.startAgent` 带 `output_schema` 同样。

**`options` 的两类。** `model`、`provider` 选这个会话的对话用哪个模型、先走哪个厂商,`session.setModel`
之后还能改(子 Agent 怎么跟随见那里)。`settings` 是这个会话的设置覆盖层,作用于会话连同它的子 Agent 与团队成员;
推理强度、思考开关与系统提示词(`model.effort`、`model.thinking`、`prompt.append`、`prompt.override`)只在这一层给,
`session.setModel` 的 `effort` / `thinking` 与 `session.setPrompt` 之后改的也是这一层,判法相同。

**`settings`——按会话给的设置。** 形状就是 settings.json 的形状(`{"turn": {...}, "permissions": {...},
"agents": {...}, "sandbox": {...}, ...}`),叠在会话所在项目与场景合并好的设置之上,managed 层仍压在它上面。
只收按会话决定的键:按项目与场景决定的(`providers`、`model.order`、`model.tasks`、`mcp`、`hooks`、`recorder`、
`scripts`、`permissions.client_may_loosen`)、按进程决定的(`app`、`plugins.enable_on_install`、`telemetry.otlp_endpoint`)写在这里
进 `invalid_options`,`field` 为 `settings.<键>`,`error` 写明它按什么决定;不认识的键、改过名的键、值不合
设置的类型、程序专用的 `permissions.mode: "bubble"`、读不出来的规则串(`field` 带下标,如
`settings.permissions.deny[0]`)、`model.thinking` 的 `on_budget` 小于 1024 同样。被拒时什么都不建、不跑。

缺省只能收紧:键表(`settings::keys`,`ARCHITECTURE.md` §6.6)的信任列里只许更严的键,值比设置的宽时不生效、留着
设置的值,`session.create` 的结果带 `clamped_options` 列出每一项(`field` 是 `options` 里的点分全路径,
`requested` 是给的,`applied` 是生效的,如 `[{"field":"settings.turn.token_budget","requested":2000000,"applied":500000}]`;
没有被夹住的就没有这个键)。`permissions.allow` / `allow_exact` 同样被扣下(`applied` 为 `[]`);`ask` / `deny`
照加,与各层的规则并列。设置开了 `permissions.client_may_loosen: true` 时放宽照样生效,含 allow 规则,与
`session.setPermissionMode` 的放宽同一个开关。这是安全缺省,不是安全边界:连得上 socket 的客户端本来就能
`config.set` 改设置。`permissions.mode` 的夹紧见 §4.5;managed 层关了 `bypassPermissions` 时会话得不到它。

**`settings.plugins`——会话用哪些插件。** `plugins.allow` 与 `plugins.enabled` 是会话键,也只能收紧:会话的集合是
`allow`(不写或 `[]` 是全部)与池里**实际启用**的包取交集,再从这个池集合里补上这些包的依赖(版本要满足);
`enabled: false` 是这个会话没有插件,宿主自带的包(`source: host`)也不例外,`enabled: true` 放不开池的 `false`。
名单里池没有启用的名字不放开,`clamped_options` 报 `settings.plugins.allow`(`requested` 原样,`applied` 是留下的);
名字全不在时 `applied` 是 `[]`,会话没有插件,不是「空表示全部」。`plugins` 段读不出(不是对象、`allow` 不是字符串
列表、`enabled` 不是布尔)时同样没有插件。不在集合里的包对这个会话、它的子 Agent 与团队成员不贡献任何东西:脚本不跑,
命令(含 `RunExt` 与 `commands.list {session_id}`)、技能、agent 类型不列,`plugin.functions {session_id}` 不列、
`plugin.call` 拒绝它的函数;它的 MCP server 仍由共享的连接 hub 连着,但不交给这个会话(包与 settings 写了同一个键
时连上的是包的那个,排除这个包的会话两者都没有);场景 `plugin:<包名>` 在包不在集合里时被拒绝:`session.create`、
恢复与重建报 `INVALID_PARAMS`、`invalid_options` 字段 `settings.plugins`,agent 类型的 `scene:` 指向这样的场景时
子 Agent 的派生报错而不退回父场景。收窄过的会话在每次建引擎时定下集合,之后才启用的包等重建才进来;没有设
`plugins` 的会话跟着池走。`session.inspectContext` 的 `plugins` 报出集合。

`options` 随会话记下,`settings` 不记判定结果:每次重建引擎都按这时的设置重新判,设置后来收紧了按设置,
放宽了仍守着会话给的值。更早的会话在日志里记的旧形状恢复时按这一层读:`options` 的 `effort`、`thinking` /
`thinking_mode`、`prompt` 按 `settings.model.effort`、`settings.model.thinking`、`settings.prompt.*`,
`turn`、`permission_mode`、`permission_rules` 按 `settings.turn`、`settings.permissions.mode` 与
`settings.permissions` 的规则串(allow 规则随之按上面的规矩扣下);之后用 `session.setModel` / `session.setPrompt`
设过的推理强度、思考开关与提示词同样按这一层的键,`null` 即去掉那个键。

`turn.max_model_calls` 是每个 agent 自己一轮的调用上限,子 Agent 继承会话的值(agent 类型的 `max_turns` 照旧
覆盖它,但不超过这一层与部署方 managed 层写的上限;agent 类型的 `effort` 同样不高过这一层写的 `model.effort`);`turn.token_budget` 是这一轮连同它起的所有子 Agent 共用的一份(§4.8 `budget_exceeded`)。

`prompt_types` 说宿主能回答哪几种 `prompt` 帧(§7)。`permission` 总是会问,写不写都一样。
列了 `question`,有人值守的会话(`attended`,缺省取场景的 `interactive`,`ARCHITECTURE.md` §3.2.3,如 `code`)才有
`AskUserQuestion`,模型的提问推成 `prompt_type:"question"` 的帧等宿主回答;没列时这个
会话根本没有 `AskUserQuestion` —— 一个只会被拒绝的工具不该给模型看。列了 `mcp_elicitation`,
MCP server 在工具调用中要问用户的(2026-07-28 server 的 `input_required` 轮次,或老版本 server
自己发的 `elicitation/create`)推成 `prompt_type:"mcp_elicitation"` 的帧;没列时不推,server 收到
`cancel`。无人值守的会话从不问,同样回 `cancel`。不认识的取值被忽略,不会让整份 `options` 作废。

`attended` 说这个会话有没有人值守,省略时取场景的 `interactive`。`ARCHITECTURE.md` §3.2.3 那张表里
`interactive` 定的四件事都跟着它走:要等人回答的工具(`AskUserQuestion` 仍要 `prompt_types` 列了
`question`,场景的工具表也得允许)、提示词(有两种提示的场景如 `code` 按它挑)、内置技能(场景的
`attended_skills` 只在有人值守时列)、权限询问送给宿主还是直接拒绝(「项目之外」的收紧、MCP
询问回 `cancel`、`Bash` 要求沙箱也一样)。`attended:false` 的会话就按无人值守的场景对待:询问直接
拒绝,无人值守的沙箱规则也一样适用(`ARCHITECTURE.md` 权限顺序 S15:有人值守时要问的受限命令,在会话
沙箱完整、网络受限时不问、在沙箱里跑);`attended:true` 让询问送给宿主。规则与托管设置照旧判。子 Agent 与团队成员随根会话(§10)。随 `options` 记在会话日志里,重建、恢复、
`session.fork` 都保留。

`auto_name` 为 `false` 时首轮之后不调模型起名,`name` 一直是 `null`,直到 `session.rename`;省略或
`true` 按场景的 `session_naming`(`true` 不会让没有 `session_naming` 的场景起名)。同样随 `options` 保存。

`skill_dirs` 给这个会话自己的 skill 目录(宿主的、按会话不同的 skill 来源):`[{ "path": "/abs" }]`,
元素是对象不是裸字符串,至多 64 个;路径在引擎所在的机器上。目录里每个子目录一个 skill,格式与磁盘上的
skill 目录相同。只有列了它的会话看得到:提示词里的 skill 清单、`Skill` 工具、`/name` 斜杠命令、
`commands.list {session_id}` 都按会话算。子 Agent、团队成员、`context: fork` 的 skill、`session.startAgent`
起的委派看到的是父会话的那一组,并跟着它变。

同名时:宿主目录里的 skill 顶替内建的、插件包的、未受信项目的同名 skill;用户的磁盘层(`{global}/skills`、
场景层)与受信项目的 `.agents/skills` 照旧占着名字。列出的几个目录之间,先列的赢(同 PATH)。
这类 skill 的 `source` 是 `host`(`commands.list`,`session.get` 的 `shadowed.by`)。

信任同插件包:`allowed-tools` 只认收窄的写法(`Bash(cargo test:*)`),裸工具名与指到项目之外的模式不认;
`` !`cmd` `` 经会话自己的 `Bash`、按会话的权限执行,`skills.allow_shell: false` 照样生效。
读取以目录自己为根:skill 目录、`SKILL.md`、`files:` 里经符号链接指到目录之外的不读,记警告;
名字以 `mcp__` 开头的不读;解析不了的 `SKILL.md` 跳过,同磁盘层。

建会话时逐条检查,不过的 `INVALID_PARAMS`、`error.data.invalid_options` 一项一条、`field` 为
`skill_dirs[i].path`(数量超限、构建没有 skills 模块时为 `skill_dirs`),什么都不建:必须是绝对路径;
已存在但不是目录的拒绝;解开链接后同一目录出现两次(记在后一项上);落在这个绑定的 skill 层
(`{global}/skills`、`{global}/scenes`、场景层的 `skills`)、会话的项目根、工作目录、可写目录
(`files.extra_dirs`、状态根的 scratchpad 与 memory)、沙箱里的命令能写的目录(状态根的
`tmp`、`sandbox.cache_dirs`、`/dev`、macOS 的每用户临时目录 `/var/folders/…/T`,`tempfile` 之类
缺省建在这里)或凭据位置之内、或把它们包在里面。不存在的目录收下,`state` 为 `missing`,出现之后在下一轮
开始前(daemon 为会话跑的每一轮前检查)读进来,此后被监视;出现时是符号链接的,按它解析到的位置监视,
那个位置同样受下面的读写边界约束。落在*另一个*项目(包括未受信的项目)里的目录照收,里面的 skill
按宿主内容对待(`source:"host"`,`` !`cmd` `` 照跑),不按那个项目的信任判:列哪个目录由宿主负责。没有 skills 模块的构建拒绝任何非空的 `skill_dirs`。`session.resume` 时记下的某个目录
已经通不过检查:这一项被略去、日志里记警告、`session.get` 报它 `state:"refused"`,其余照用。

读写边界:会话自己的宿主目录不问就能读(算在它的可读根里,严格沙箱档也是,无人值守的场景也是)。
daemon 进程里任何会话列过的目录,对所有会话都是只读:内建工具里任何非只读的调用,参数里点名了其下的
路径,在每一种权限模式下都被拒,`bypassPermissions` 也不例外(权限阶段 `host_skill_dir`)。MCP 工具与
宿主工具(`host_tools`)不向权限判定报路径,这一条拦不到它们,同凭据位置。这个保护集合在进程内只增不减,
宿主自带的包(`plugin_packages.md`「宿主自带的包」)登记时目录也进这个集合。
沙箱里的命令写不到宿主目录:列它的会话能写的根都在上面的检查里拒掉了,集合里的每个目录也在每个会话的沙箱
策略里钉成只读(落在别的会话的项目里时,那里的命令同样写不了它,也挪不走它上面的目录)。已知的缺口:冷会话的
目录,要等某个列了它的会话在本进程里被载入才受保护;不沙箱的命令写到那里而没有点名路径,拦不住(同今天的
`.agents/skills`),路径粘在选项上(`-o/dir/x`)或由变量拼出(`"$D"x/…`)也算没点名;Linux 上只读绑定的是
命令开始时已存在的目录。

`host_tools` 声明宿主自己实现的工具。它们和会话自己的工具一样宣告给模型,经过同样的权限、
hooks、中间件与遥测;模型调用一个,宿主收到 `host_tool_call` 帧(§7.4),用
`session.respondToolCall` 回答,回答就是模型读到的结果:

```jsonc
"host_tools": [
  { "name": "HostEcho",                       // ^[A-Za-z][A-Za-z0-9_-]{0,63}$,不以 mcp__ 开头
    "description": "Echo text back from the host.",   // ≤ 8 KiB
    "input_schema": { "type": "object",       // type:object、能编译的 JSON Schema,≤ 64 KiB
                      "properties": { "text": { "type": "string" } },
                      "required": ["text"] },
    "read_only": true,                        // 可选,默认 false;plan 模式只放行 read_only 的
    "destructive": false,                     // 可选,默认 false
    "concurrency_safe": true,                 // 可选,默认同 read_only
    "timeout_ms": 120000,                     // 可选,默认 120000,范围 1000..=600000
    "deferred": false,                        // 可选,true 时只报名字,schema 经 ToolSearch 取
    "subagents": false,                       // 可选,true 时子 Agent 也拿得到,见 §7.4
    "permit": "ask" } ]                       // 可选,"ask"(默认)或 "allow",见下
```

一个会话至多 64 个。读不出来的逐项进 `invalid_options`,`field` 带下标与字段名,如
`host_tools[1].input_schema`、`host_tools[0].name`(重名也报在 `name` 上);与会话已有的工具
重名(`Read`、场景或插件的工具)要等引擎建起来才知道,报在 `host_tools` 上。两种都是
`INVALID_PARAMS`,不建会话。宿主工具只是加法,与场景的 `extra_tools` 同一个重名检查,场景的
`tools.deny` 写名字即可拒掉它。它们随 `options` 记在会话日志里(`permit` 也在内),重建、恢复、
`session.fork` 都照原样再挂上:不带 `options` 恢复的会话,`permit` 取日志里记的那份。连接提供的工具(`daemon.setHostTools`,§6.1)是另一条路:不进这里,也不进会话日志,
重建时取登记表当时的内容;会话自己的同名项优先。

权限上它缺省和 MCP 工具同一档:缺省模式下每次调用都先问(`prompt_type:"permission"`);无人值守的
会话(如没给 `attended` 的 `harnix`)里这样的询问直接被拒绝,宿主要自己回答就给 `attended:true`。
想免问有两条路:

- 条目上写 `"permit": "allow"`。只放行这一个工具的调用,如同一条只认它的 allow 规则:不按名字去匹配
  别的工具(名为 `mcp` 的宿主工具放行不了任何 `mcp__*`),不放宽会话的权限模式,也不受
  `permissions.client_may_loosen` 管。压过它的照旧:任何一层(含 managed)的 deny 与 ask 规则、
  宿主自带的权限策略的拒绝与询问、`PreToolUse` hook 的拦截、`plan` 模式只放行 `read_only` 的。
  `dontAsk` 模式和无人值守的会话里它算一条命中的 allow 规则;`bypassPermissions` 下用不上它。
  只有宿主声明的那个工具认这个放行:插件或场景的工具自称宿主的也不算。遥测的
  `permission_decision` 里这样放行的 `basis` 是 `host_declared`,与规则放行的 `rule` 分得开。
  子 Agent 拿到的同一个工具(`subagents:true`)带着同一个放行,子 Agent 的模式上限照旧管它。
  能力位 `host_tool_permit`;不认这个字段的引擎把整个 `options` 以 `INVALID_PARAMS` 退回,
  宿主先看 `daemon.ping` 的能力位再决定写不写。
- 在设置的 `permissions.allow` 里写 `HostEcho`(经 `options.settings.permissions.allow` 给时,设置得开着
  `permissions.client_may_loosen`,否则被扣下)。

deny 规则同样管得住它。宿主只会收到符合 `input_schema` 的 `input`,不符合的调用以校验错误
退回给模型,不发帧。

`provider` 把这个会话要用的 provider 排到这个会话的 `model.order` 最前,其余照旧:它不可用(配额用尽、
没有凭据、连不上……)时仍按 `order` 往后换。`model` 可以是模型 id,也可以是档位名(`strong`、
`balanced`、`fast`);省略时对话取 `model.tasks.main` 的档位。provider 没配置、或 `model` 不在该 provider 的
`models` 白名单里 → `INVALID_PARAMS`。对话的模型依次取:会话自己选的 `model`(`options.model` 或
`session.setModel`);没选时 `model.tasks.main` 那一档在当前 provider 里对应的模型。子 Agent、压缩、记忆、
标题、搜索也都走同一个 `order`,各取自己任务的档位(`model.tasks`)。

#### `session.run_turn`

**流式方法**:最终响应之前会在同一连接推送 0..N 个 `session.event` 帧(§7)。
并发约束见 §5.3,顺序保证见 §5.5。

```jsonc
// → params
{ "session_id": "…",              // 可选!省略则自动新建一个会话
  "message": "把权限层重构一下",     // 必填;只有空白且没有附件 → INVALID_PARAMS
  "turn_id": "t-1",               // 可选,省略则服务端生成;不能是目标轮(goal-<修订>-<轮次>-…)或定时任务(schedule- / cron- 开头)的形式
  "attachments": [ ],             // 可选,见 §4.4
  "options": { },                 // 可选,同 session.create,仅新建时生效
  "on_busy": "reject" }           // 可选,reject | queue | steer,见 §5.3
// ← result
{ "session_id": "…", "turn_id": "…",
  "delivery": "turn",             // turn = 这条消息跑了一整轮;steer 见 §5.3
  "name": null,                   // 首轮自动命名的结果,场景不开启或 auto_name:false 时为 null
  "api_calls": "…" }              // 数字:本轮打了几次模型 API
```

`message` 为空或只有空白、又没有一个形状对的附件(§4.4),这次调用直接以 `INVALID_PARAMS`
失败:不建会话、不调模型。只带附件、`message` 为空是可以的。

**最终响应里没有 `stop_reason` / `usage`** —— 那两个在 `turn_complete` 事件里
(§7)。想拿到它们就必须读事件流。

`session_id` 给了但不在内存里:磁盘上有历史就自动 resume,没有就新建一个用这个 id
的会话。跨场景的 id 不会被静默恢复(§3.4)。

turn 期间的引擎错误不是事件,是**这次调用的错误响应**:`ENGINE_ERROR (-32002)`,
`message` 形如 `"<kind>: <描述>"`,`data` 说明是哪一类失败、能不能重发、什么时候再来、
这一轮有没有工具已经跑过(§9.1)。

#### `session.get`

```jsonc
// → params
{ "session_id": "…" }
// ← result
{ "session_id": "…", "name": null, "preview": null,
  "message_count": 0, "created_at": "…", "last_active": "…",
  "status": "active",              // active | inactive
  "session_kind": "primary",
  "resumable": true,
  "scene": "code", "project_root": "…",   // null = 无项目会话,或老日志没记
  "scene_active": true,
  "turn_state": "idle",            // idle | running | rebuilding | operating
  "current_turn_id": null,
  "reasoning": { "effort": "high", "effort_source": "settings",
                 "thinking": "on", "thinking_source": "settings" } }
```

比 `session.list` 的条目多 `scene_active` / `turn_state` / `current_turn_id` 三项 ——
第一个要问 daemon 当前激活了哪些场景,后两个只有内存里的池子知道。`turn_state` 就是
"发送按钮该不该置灰"的依据:`running` 是有一轮在跑,`rebuilding` 是正在原地重建,`operating`
是一个 `inspectContext` / `dropMessages` / `editMessage` 正独占着会话;后两种状态下
`run_turn{on_busy:"reject"}` 报 `SESSION_BUSY`(§9.1),`current_turn_id` 为 `null`。
不在内存里的会话是 `idle`。

`skill_dirs` 只有会话列了宿主目录时才有,形如:

```
[ { "path": "/abs/skills", "state": "ok",
    "skills": [ "tour" ],                 // 这个目录在本会话里占着的 skill 名
    "shadowed": [ { "skill": "x", "by": "user|project|host", "path": "…" } ] } ]
```

`state` 取 `ok`、`missing`(目录还不存在)、`refused`(恢复时通不过检查);
`shadowed` 是这个目录里被别处同名 skill 遮住的,`by` 是占着名字的一方。不在内存里的会话只有 `path`。

`reasoning` 是这个会话下一次模型调用带的推理强度(`null` = 不发,交给端点)与思考开关,`*_source`
为 `session`(会话自己那层设置的 `model.effort` / `model.thinking`,`options.settings` 或 `session.setModel` 设的)
或 `settings`(会话绑定的项目与场景的,包括会话给了却被 managed 层压住的)。sidechain 与没记场景的老会话没有这个键。

sidechain 另有 `model` / `provider`:取自父会话里 `detail.agent_session_id` 是它的那件任务(团队常驻成员取最近的一条消息的),
与那件任务 `detail` 里的同值(见 `session.tasks`),口径同 `session.inspectContext` 的 `model` / `provider`:真正的模型 id
与 provider id。父会话不在内存里、或它已经不记这件任务时没有这两个键;子 Agent 再起的子 Agent 的侧链也没有 —— 它的
父会话是子 Agent,不是池里的会话。

`scene` / `project_root` 与 `session.list` 的条目**同源**(都是那一条 `SessionInfo`),
所以两个方法对同一个会话不会给出不同的答案。

**`context`——上下文用了多少。** 形状与 `turn_complete` 的 `context` 相同(§7.2),取最近
一次调用过模型的那一轮。会话在内存里、且至少有一轮调过模型时才有这个键。

**`usage`——会话从创建起花了多少。** §3.5 的四字段用量,是此刻的读数:会话自己的调用、它起的
子 Agent(前台、后台)与团队成员的调用、这些 agent 在旁边发的调用(压缩、记忆、`WebSearch`、
`WebFetch` 的抽取),每次调用一结束就算进去,不等子 Agent 或这一轮结束。首轮之后的自动命名与手动
压缩不在任何一轮里,不计;宿主 `session.startAgent` 起的子 Agent 计入。它跨重建、驱逐与 daemon 重启
(记在会话日志的 `agix.spend` 里,见 ARCHITECTURE §4);`session.fork` 出来的会话从源会话在分叉点
已花的接着算。会话在内存里时才有这个键。

**`health`——这个会话的插件报的健康检查。** 形状与 `daemon.doctor` 的 `health` 相同
(`status` 取最差的一项,`checks` 每项带 `name`/`status`/`summary`/`details`)。只有插件注册了
检查(`Registrar::health_check`)的会话才有这个键。每项检查最多 5 秒:超时的那一项报 `degraded`、
summary 为 `did not answer within 5s`,`session.get` 不再等它。

**`scripts`——这个会话的脚本都干了什么。** 只有绑了脚本的会话才有这个键;没绑的会话
**没有这个键**,而不是一个空对象——「这儿没有脚本」和「脚本一次都没跑」是两个不同的
答案。

```jsonc
"scripts": {
  "calls": 7, "applied": 5, "no_change": 1, "failed": 1, "refused": 0,
  "dropped": 0,                    // 非 0 表示 recent 只是尾巴
  "recent": [                      // 最近 20 条,新的在前
    { "point": "prompt.assemble", "script": "/proj/.agix/scripts/x.js",
      "entry": "onAssemble", "turn": 2, "outcome": "failed",
      "detail": "script exceeded its 100ms budget" }
  ]
}
```

`outcome` 取 `applied`(点采纳了返回值)、`no_change`(跑了但没有可施加的东西)、
`failed`(调用没完成:抛异常/超时/配额耗尽/没有引擎)、`refused`(要求的事它的出身
不允许)。**这四个在引擎里的效果完全一样——什么都没变**,所以一个脚本"没生效"到底
属于哪一种,只有这里说得清。`detail` 在后三种里带着原因。

侧链会话另有 `parent_session_id`;主会话没有这个字段(不是 `null`,是不出现)。

**`preview` 恒为 `null`,`message_count` 恒为 `0`** —— 两个字段在协议里占了位置,
但没有任何代码路径会填它们。要消息条数用 `session.history` 的 `total`。

#### `session.list`

```jsonc
// → params
{ "include_children": false,     // 可选,默认 false = 不含侧链
  "parent_session_id": null }    // 可选,给了就只返回这个会话的侧链
// ← result
{ "sessions": [
    { "session_id": "…", "name": null, "preview": null,
      "message_count": 0, "created_at": "…", "last_active": "…",
      "status": "active", "session_kind": "primary", "resumable": true,
      "scene": "code", "project_root": "…", "compact_count": 0 } ] }
```

返回内存中的会话 + 磁盘上的历史会话,合并去重,**不分页、不过滤场景**。
没有 `scene` / `project_root` / `status` / `limit` / `cursor` 这些**参数** —— 传了
会被忽略;但每个**条目**都带 `scene` 与 `project_root`,按项目或场景分组在客户端一次
遍历就能做完,不用逐个 `session.get`。

冷会话的范围是**这个 daemon 服务过的项目**:它自己的项目,加上这个进程里有会话
建在或恢复在其上的每个项目。不是磁盘上的全部项目 —— 那会报出别的 daemon、别的工作
目录的会话。设置变化不会让 daemon 忘掉服务过的项目;重启会,一个项目在重启后有会话
落到它上面之前不出现在列表里。活跃会话不受影响,它们本来就在内存里;`session.get`
按 id 查,不在列表范围里的冷会话也查得到。

`scene` / `project_root` 来自会话自己的 `Meta` 行(§3.4 校验的是同一处),所以冷会话
也答得出来。**两个字段恒出现**,老会话(`Meta` 里没记场景的 pre-v2 文件)是 `null` —— 
`null` 是"这份日志没写",不是"没有项目"。

`compact_count` 是这个会话被压缩过几次 —— 引擎写 transcript 的时候顺手记的,冷会话
也答得出来。它回答的是"这个会话被压过没有",**不回答"哪一次压掉了什么"**:那件事只有
压缩发生的当时说得清,看 §7.1 的 `compact` 帧。

**未激活会话的摘要字段几乎全是空的**:`name` 为 `null`,`created_at` / `last_active`
是**空字符串**,`message_count` 为 `0`。这是设计如此 —— 列表不为了填几个摘要字段去
逐个读 transcript。活跃会话的 `created_at` / `last_active` 是真的,`name` 在场景开启
自动命名且已经跑过一轮时才有值。

`parent_session_id` 是**事后**查子 Agent 的路径:它扫的是磁盘上的 `Meta`,daemon
重启后仍然可用。拿到子会话 id 后用普通 `session.history` 读内容 —— 侧链与主线共用
同一套查询 API,不提供父子合并视图。

这个过滤只比对 `Meta.parent_session_id`,所以 `session.fork` 出来的会话也会出现在
结果里,而且**一律被标成 `"session_kind":"sidechain"`、`"resumable"` 按侧链规则算**
—— 这一路径的 `session_kind` 是写死的,不是从磁盘读的。要判断一条记录到底是不是
侧链,用 `session.get` 问它自己。

#### `session.subscribe`

```jsonc
// → params
{ "session_id": "…",
  "resume": false }            // 可选,默认 false
// ← result
{ "session_id": "…",
  "last_seq": "…",             // 数字:transcript 水位,用于追赶(§5.6)
  "pending_prompts": [ ],      // 还没被回答的 kind:"prompt" 帧,原样重放
  "pending_host_calls": [ ],   // 还悬着的 kind:"host_tool_call" 帧,原样重放
  "resumed": false }           // 这次调用有没有替你恢复它
```

订阅之后该会话的 `session.event` 帧都推到这条连接上,直到 `session.unsubscribe`
或连接断开。重复订阅同一会话是幂等的。

**默认只能订阅内存里的会话** —— 会话不在内存里返回 `SESSION_NOT_FOUND`,而不是给一
个永远不会有数据的订阅。

**`resume: true` 表示"打开它,不管它在不在内存里"**:冷会话先恢复再订阅,响应带
`"resumed": true`。这不是默认行为,
因为恢复要读一遍 transcript 并占掉会话池的一个名额 —— 一个画会话列表的宿主给每一行
都订阅一次,默认恢复就等于替它把整页会话全恢复了,而它以为自己只是在看。

`resume: true` 不接 `options`:要给一个没有记录过 `options` 的旧会话指定
`settings` 之类再恢复,显式调 `session.resume`。恢复失败的错误跟 `session.resume` 完全一致 —— 跨场景仍然
`SCENE_MISMATCH`(§3.4),终态侧链仍然 `SIDECHAIN_TERMINAL`,池子满了仍然按恢复那边
的规矩来。

`pending_prompts` 里的帧带**原来的 `prompt_id`**,所以中途打开的 tab 可以直接答那个
还悬着的提问(权限提问与模型的提问都在里面)。提问在引擎推出它的 `prompt_resolved`(§7)时清掉;
一轮自己的提问最晚在这一轮结束时清掉,后台子 Agent 的提问不属于哪一轮,留到被回答或它的任务结束。
宿主工具的调用(§7.4)不在 `pending_prompts` 里,单列在 `pending_host_calls`:它不是问人的,
画提问列表的客户端不该看到它。

#### `session.unsubscribe`

```jsonc
// → params
{ "session_id": "…" }
// ← result
{ "session_id": "…", "subscribed": false }
```

**没订阅过也返回成功**:调用方要的是"别再推给我",而现状已经如此。

#### `session.interrupt`

```jsonc
// → params
{ "session_id": "…" }
// ← result
{ "session_id": "…", "interrupted": false }
```

取消当前 turn,**保留会话**。没有进行中的 turn 时 `interrupted` 是 `false`,不是
错误 —— 调用方要的是会话空闲,而它确实空闲。模型还没开始回答(请求还在等响应、在退避
重试之间)、正在写自动压缩的摘要(含请求因过长被拒、为重试而做的那次压缩),也立即生效,不等它答;
压缩作废,会话历史保持原样。这一轮以 `stop_reason: "cancelled"` 结束。

中断的是最近一次提交、还没结束的那一轮(`turn_started` 报的那一轮),只认这一轮的 `turn_id`。
它提交时引擎可能还在收上一轮的尾、在重跑 `async_rewake` hook,目标轮可能还在把认领记进会话
日志、消息还没送到引擎,这时中断照样落在它身上:它一开始就是取消的,以 `cancelled` 结束,不调
模型;上一轮不受影响。

**中断也让目标解除武装**(`session.goal`),和以取消结束的一轮一样,要人说继续。所以一轮已经
推了 `turn_complete`、还在收尾(记目标)时到达的中断,`interrupted` 照样是 `true`:这一轮已经
结束,它起的后台任务不受影响,但它之后不再开下一个目标轮。

**这一轮起的后台任务跟着结束。** 这一轮里 `Monitor`、后台 `Agent`、后台 `RunCode` 起的任务挂在
这一轮下面,中断这一轮会连带停掉它们,哪怕它们本来要活过这一轮(`task_finished` 照常推)。更早的轮
起的、还在跑的后台任务不受影响;宿主用 `session.startAgent` 起的子 Agent 与团队成员挂在会话下,
也不受影响。只想停一件任务、不想停这一轮,用 `session.stopTask`。

**`session.compact` 本身就是一轮**,可以这样中断:摘要请求立即放弃,迟到的摘要不会写进历史,
会话历史保持原样,这一轮以 `cancelled` 结束。

#### `session.compact`

**流式方法**,与 `session.run_turn` 发 `"/compact"` 相同。

```jsonc
// → params
{ "session_id": "…",
  "instructions": "保留所有文件路径原文",   // 可选:摘要要保留或强调什么,最长 4000 字符
  "at": { "to_turn": "t1" } }              // 可选:恰好一个 to_turn | to_tool_use
// ← result:同 session.run_turn
```

`instructions` 原样放进摘要请求里,摘要仍按固定的九个标题写。没配摘要模型、走级联压缩时
它不起作用。`session.run_turn` 发 `/compact <说明>` 等价于带 `instructions`;
`/compact --at turn:<id> <说明>`、`--at tool_use:<id> <说明>` 等价于带 `at`。

`at` 把压缩范围定在那一点:`to_turn` 是到这一轮结束为止(同 `session.rewind` 的 `to_turn`),
`to_tool_use` 是到持有该工具调用及其结果的那一往返为止;之后的消息原样留在历史里。带了 `at`
就一定要摘要模型写摘要,没有摘要模型、摘要失败或找不到这一点时历史不变,正文说明原因。

**不看阈值,现在就压。** 配了摘要模型时由它写全文摘要(`FullCompact`),否则按引擎的
级联压到历史当前大小的一半左右。动手前推一帧 `compact_started`,压了会推一帧 `compact`
(§7.1),没什么可压时没有这一帧、以 `compact_ended` 收尾;两种情况都以 `turn_complete`(`stop_reason: "command_executed"`)结束,正文
里有一句结果说明。写摘要期间被 `session.interrupt` 中断时历史不变,以 `cancelled` 结束。
会话正在跑 turn 时返回 `SESSION_BUSY`。

#### `session.clear`

**流式方法**,与 `session.run_turn` 发 `"/clear"` 相同。

```jsonc
// → params
{ "session_id": "…" }
// ← result:同 session.run_turn
```

清空对话上下文。清空本身写进 transcript,之后从磁盘恢复的会话也是空的。会话正在跑
turn 时返回 `SESSION_BUSY`。

#### `session.rename`

```jsonc
// → params
{ "session_id": "…", "name": "重构 daemon 装配" }   // name 为 null = 清除
// ← result
{ "session_id": "…", "name": "重构 daemon 装配" }
```

会话标题是**使用者的东西**,所以它写在会话自己的事实里(`~/.agix` 下的会话
sidecar),不是某个宿主自己的一层覆盖 —— 各宿主各存一份的结果是同一个会话在这边
叫一个名字、在那边叫另一个,共用 `~/.agix` 的意义就打了折。`session.list` /
`session.get` 的 `name` 就是这里写进去的值。

**冷会话也能改名**:命名是关于会话的事实,不是对运行中引擎的操作,所以它不会为了
改个名字把会话恢复起来。

`name` 传 `null`(或者不传这个键)= 清除,会话回到"自动命名"的状态。非空、最长
200 字符、不接受控制字符 —— 这是要显示在别人列表里的一行字。

**用户改过的名字不会被自动命名覆盖**:开了自动命名的场景在首轮结束后会生成一个
名字,但它跳过已经被 `session.rename` 命名过的会话。生成名字用的是会话自己项目的
`model.tasks.title` 那一档的模型。

#### `session.goal` / `session.setGoal` / `session.updateGoal`

会话目标:一个一轮做不完的目标。会话空闲时引擎自动开下一轮(`initiator:"goal"`),注入
`<goal_round>` 提醒模型目标和轮次,直到模型标记完成或受阻,或者轮数或 token 预算用完(用完即转为
`blocked`)。模型经 `GoalCreate` / `GoalGet` / `GoalUpdate` 工具操作同一个目标。

```jsonc
// session.goal → params
{ "session_id": "…" }
// ← result;没有目标时 goal 为 null
{ "goal": { "id": "…", "revision": 3, "objective": "迁移到新接口,测试全绿",
            "phase": "active", "armed": true, "max_rounds": 256,
            "token_budget": 200000,          // 没设时没有这个键
            "tokens_used": 41000,            // 目标各轮已花的,口径同预算
            "rounds_started": 2, "rounds_since_armed": 2, "blocked_reason": null } }

// session.setGoal → params(会话已有未完成的目标时失败)
{ "session_id": "…", "objective": "…", "max_rounds": 256,   // max_rounds 可选,默认 256
  "token_budget": 200000 }                                  // 可选,缺省不限
// session.updateGoal → params
{ "session_id": "…", "action": "pause" }   // pause | resume | edit | clear
{ "session_id": "…", "action": "edit", "objective": "…", "max_rounds": 64, "token_budget": 300000 }
// ← 二者的 result 同 session.goal
```

- `phase`:`active` / `paused` / `blocked` / `complete`。只有 `active` 且 `armed` 的目标
  会自动开轮。
- `token_budget` 是目标各轮能花的 token(未命中缓存的输入、缓存写入与输出;各轮起的子 Agent、团队成员与
  旁路调用都在内),从目标设下起算,设它的那一轮和期间宿主、定时任务的轮不算。剩余付不起一次调用(扣掉这次
  输入的估计后不到 1024)时,那一轮在调用前以 `budget_exceeded` 结束,目标转 `blocked`,`blocked_reason` 形如
  `token budget: 2400 of 3000 tokens used`,不再开下一轮;`resume` 之前要先 `edit` 把 `token_budget`
  调高(同 `max_rounds` 用完)。`tokens_used` 跨重启;会话不在内存里时 `session.goal` 从日志读出。
  `token_budget` 给 0 → `INVALID_PARAMS`。会话自己的 `session.token_budget` 照样约束目标的轮。
- 模型经 `GoalCreate` / `GoalUpdate` 设或改的目标,`max_rounds` 不超过设置 `goal.max_rounds`(缺省
  10000),给多了压到它并在工具结果里说明;宿主经这里设的只受 1..=10000 约束。`armed` 不落盘:会话从日志恢复(驱逐后、daemon 重启后)目标一律不武装,要人
  `resume` 才继续。会话在内存里原地重建(`session.setPrompt`、`session.setModel` 换厂商、
  配置变化)保留武装;重建之后推一次 `goal_changed`。
- 宿主经这两个方法的操作都按「用户亲自发起」对待。模型那一侧:设定、修改、暂停、恢复
  只能在用户发起的那一轮做;自动轮里只能标完成或受阻,受阻要从自武装起的第 3 个自动轮起。
- 一轮以取消、hook 中止、预算耗尽、出错结束,目标就解除武装,等人决定;`session.interrupt`
  报 `interrupted: true` 时也解除武装,哪怕那一轮已经答完、正在收尾。以 `max_tokens`
  结束的一轮和正常结束一样,接着开下一轮。
- 在空闲会话上设定或恢复目标,第一轮马上开始;`pause` 碰上正在跑的自动轮会中断它。
- 同一时刻至多有一个自动轮在等着开始:别的轮(客户端的、定时任务的)在它之前结束,不会
  再开一轮。等着的这一轮开始前目标被暂停、清除、修改或解除武装,它就不跑了。
- 清除之后再设的目标,`revision` 接着清除前的往上数,不会和旧目标的重复。
- 会话不在内存里时,`session.goal` 从日志读(`armed` 为 `false`),`setGoal` /
  `updateGoal` 先把它恢复起来。会话不存在 → `SESSION_NOT_FOUND`;状态不允许的操作 →
  `INVALID_PARAMS`,`message` 说明原因。
- 每次变化推 `goal_changed`(§7),并以扩展条目(`ns:"agix.goal"`)记进会话日志。

#### `session.appendEntry`

往会话日志里加一条扩展条目(`history.extension_entry`):宿主自己的、给人看的东西,
比如一张任务卡、一次批准的结论。和其余条目同序,恢复、分叉、`session.entries` 都看得到;
模型看不到它,除非宿主给这个命名空间配了 `ExtensionsAreVisible`。

```jsonc
// → params
{ "session_id": "…",
  "ns": "com.example.cards",   // 必填,1–128 字符;`agix.` 开头的是引擎自己的,拒绝
  "event": "job_done",         // 必填
  "payload": { } }             // 可选,任意 JSON,引擎从不解析
// ← result
{ "id": "…", "ts": "…", "ns": "com.example.cards", "event": "job_done",
  "payload": { }, "at": "…" }  // at:它前面已有的消息数,坐标同 session.history 的 seq
```

会话冷着也能写,不会把它拉进内存。会话在内存里时给它的订阅者推一帧
`extension_entry`(§7),`turn_id` 为 `null`。会话不存在 → `SESSION_NOT_FOUND`;`ns`
或 `event` 不合规 → `INVALID_PARAMS`。

#### `session.entries`

按日志顺序列出会话的扩展条目,不含 `agix.*`(引擎的记账)与侧链里的。

```jsonc
// → params
{ "session_id": "…", "namespaces": ["com.example.cards"] }   // namespaces 可选,省略 = 全部
// ← result
{ "entries": [ { "id": "…", "ts": "…", "ns": "com.example.cards", "event": "job_done",
                 "payload": { }, "at": "…" } ] }
```

`at` 让宿主把条目插回对话里:它排在第 `at` 条消息(`seq == at`)之前,`at` 等于消息
总数就是在最后。会话不存在 → `SESSION_NOT_FOUND`。

#### `session.setModel`

```jsonc
// → params
{ "session_id": "…",
  "model": "claude-opus-4-6",     // model / provider / effort / thinking 至少给一个
  "provider": "anthropic",        // 可选,providers 里的 id
  "effort": "high",               // 可选,写会话设置的 model.effort;null = 去掉,回到设置的值
  "thinking": "off" }             // 可选,写会话设置的 model.thinking;null = 去掉,回到设置的值
// ← result
{ "session_id": "…", "model": "claude-opus-4-6", "provider": "anthropic",
  "effort": "high", "thinking": "off",        // 会话设置里的,null = 随设置
  "applies": "next_turn" }                    // 被夹住时另带 clamped_options,见下
```

0.3 的 `thinking_mode` 改名为 `thinking`;传 `thinking_mode` 以 `INVALID_PARAMS` 失败,报错里给出新名字。

换掉这个会话**下一次模型调用**用的模型。transcript 不动 —— 这是换"谁来回答下一
条",不是改写已经说过的话。所以"这一段用便宜的,接下来换个强的"不再需要新建会话,
也就不再需要丢掉上下文。

只给 `model`:换模型(模型 id 或档位名),仍走当前 provider。给 `provider`:把它排到这个会话
`model.order` 的最前,会话在原地按新的规格重建(它是空闲的,不会腰斩什么),没同时给 `model` 就取
`model.tasks.main` 的档位。`model` 按它将被发往的 provider 的 `models` 白名单检查,不在
白名单里 → `INVALID_PARAMS`,而不是把一个对端不认识的模型名静默发过去。子 Agent、
压缩等其他任务照 `model.tasks` 取各自的档位;这个会话此后派生的子 Agent,`Agent` 调用与 agent
类型都没指定模型时,取 `model.tasks.subagent` 那一档的模型,与对话用的是同一个模型时跟随这里换上的模型。
`WebSearch` 搜索用 `model.tasks.web_search` 那一档的模型。

`effort` / `thinking` 写进这个会话自己那层设置(`options.settings` 那一层)的 `model.effort` /
`model.thinking`,与建会话时给的合在一起记下,从下一轮起生效;和那一层的其他键一样作用于会话连同它的
子 Agent 与团队成员(agent 类型写了 `effort` 的照旧覆盖),判法也相同:managed 层定了别的值时不生效,
结果带 `clamped_options`(形状同 `session.create` 的,`field` 为 `settings.model.effort` /
`settings.model.thinking`)。会话在原地按新设置重建,同换厂商(忙时的拒绝也同,见下)。键不给就不动,
给 `null` 就把这个键从会话的设置里去掉、回到项目与场景设置的 `model.effort` / `model.thinking`
(建会话时给的也去掉)。取值不认识、`on_budget` 小于 1024 → `INVALID_PARAMS`
(`options.settings.model.thinking` 同样,进 `invalid_options`)。发出时预算不小于这次调用的 `max_tokens`
就降到 `max_tokens - 1`,`max_tokens` 留不出 1024 时这次调用关掉思考,都记一条 warn。当前生效值与来源见 `session.get` 的 `reasoning`。

**有 turn 在跑时返回 `SESSION_BUSY`**(`reason: "turn_running"`),不排队。选择记在会话上,
哪一代引擎跑下一轮都在那一轮开始时读它;正在跑的这一轮不会换,这时受理下来,调用方被告知模型
换了,却看着旧模型答完这一轮。要换就先 `session.interrupt`,或者等这一轮结束。会话正在原地重建时同样
`SESSION_BUSY`,`reason` 为 `"rebuilding"`。换厂商要重建会话,会话还有后台任务在跑(子 Agent、
团队成员、后台程序、Monitor、宿主的 `plugin.call`)时同样 `SESSION_BUSY`,`reason` 为 `"background_tasks"`,`data` 多一个
`running_task_ids` 列出它们 —— 重建会停掉它们;等它们结束,或先 `session.stopTask`。改 `effort` / `thinking`
同样要重建,同样这样拒绝。

重建时旧引擎 5 秒内没停下(卡在不理会取消的工具调用里之类;只通知的 hook 在后台跑,不拖住它),也返回 `SESSION_BUSY`
(`reason` 为 `"previous_engine"`、`current_turn_id` 为 `null`、没有 `running_task_ids`):这次的选择**没有生效,也不记进日志**;
旧引擎已经取消,会话被移出内存(`daemon.event` `session_evicted`,`reason: "rebuild"`),下一次
`run_turn` / `session.resume` 等旧引擎停下后按原来的选择从转录恢复。要换,稍后再调一次。

换厂商时上一轮的日志没写全(`turn_complete` 报 `persisted: false`,§7.2),重建前先让引擎补写;
补写失败返回 `ENGINE_ERROR`(`kind: "persist_failed"`),选择没有生效,会话照旧在原来的引擎上。

**只对内存里的会话有效**,冷会话返回 `SESSION_NOT_FOUND`(先 `session.resume`)。
选择记在会话日志里(见 `session.create` 的 `options`):会话被驱逐后重建、
`config.reload` 之后重建、daemon 重启后恢复,选的都还是这个模型、厂商,会话设置里的推理强度与思考开关也一样;
`session.fork` 带过去切点之前记下的规格,其中也有它们。

#### `session.setSkillDirs`

```jsonc
// → params
{ "session_id": "…",
  "skill_dirs": [ { "path": "/abs/skills" } ] }   // 整组替换;[] = 清空
// ← result
{ "session_id": "…", "skill_dirs": [ { "path": "/abs/skills" } ], "applies": "next_turn" }
```

换掉这个会话的整组宿主 skill 目录,检查与合并规则同 `session.create` 的 `options.skill_dirs`
(`INVALID_PARAMS` 与 `invalid_options` 同样,`field` 带 `skill_dirs[i].path`)。不重建引擎:
后台任务不停、也不因它们在跑而拒绝;下一轮开始时会话比对新旧的 skill 视图,发提醒与
`skills_changed`;正在跑的这一轮可能已经看到新的一组。已经展开进 transcript 的 skill 正文不变。

会话不在内存里时只改记下来的规格,`session.resume`、调度出来的轮次、重建与重启按它走。
这样设的值优先于 `options.skill_dirs`(同 `session.setModel`),并记在 `agix.session/spec` 条目里。
会话不存在 → `SESSION_NOT_FOUND`。

#### `session.setPrompt`

这个会话自己那层设置(`options.settings` 那一层)里的 `prompt.append` / `prompt.override`:一段追加文字
与一份整体替换,优先于 settings.json 的同名键。`session.create` 的 `options.settings.prompt` 给初值,
这个方法在会话进行中改,判法与 `options.settings` 相同。managed 层写了 `prompt.append` 时,那一段在
`override` 之下仍接在系统提示词末尾。

```jsonc
// → params
{ "session_id": "…",
  "append": "回答一律用中文。",   // 字符串或 [{"name","text"}] = 设置
  "override": null }               // null = 清除,回到 settings 文件的值;省略 = 不变
// ← result
{ "session_id": "…", "append": "回答一律用中文。", "override": null,   // 会话设置里的,null = 随设置
  "applies": "next_turn" }                    // 被夹住时另带 clamped_options,同 session.setModel
```

`override` 替换整份系统提示(场景骨架、规则、MCP 说明都不再出现),`append` 接在最后。

`append` 给成字符串是一个系统块 `config.prompt_append`;给成数组(能力位 `prompt_append_blocks`)
是每项一块,按数组次序、整组在字符串那一块的位置,缓存断点不因此移动;`session.inspectContext` 逐块报 `tokens`。
块名与 origin 说的是这个值由哪一层给出:

| 给出 `prompt.append` 数组的层 | 块名 | `inspectContext` 的 `origin` | `session.call` 的 `origin` |
|---|---|---|---|
| 会话自己的(`options.settings`、`session.setPrompt`) | `host.<name>` | `{"host":"<name>"}` | `host:<name>` |
| 宿主 / 运维在内存里给的(`agixd --set`、嵌入的 `EmbedConfig.settings` 与宿主改过的设置) | `host.<name>` | `{"host":"<name>"}` | `host:<name>` |
| 设置文件(global、scene、project 各自的 `settings.json` / `settings.local.json`) | `config.<name>` | `{"config":"<name>"}` | `config:<name>` |
| managed 层 | `config.<name>` | `{"config":"<name>"}` | `config:<name>` |

项目的设置文件不受信也照用 `prompt.append`,所以克隆来的仓库给的数组只会是 `config.<name>`,
冒充不了宿主扩展的块。字符串不分层,照旧是 `config.prompt_append`、origin `config`。
`name` 不能是 `prompt_append`、`prompt_override`(那会和 `config.prompt_append`、`config.prompt_override` 同名)。

```jsonc
"append": [ { "name": "harnix:review", "text": "…" },   // name:非空,字母数字与 _ . : -,组内不重复
            { "name": "user", "text": "…" } ]           // text:非空;[] = 什么都不追加
```

数组整个替换下层的 `append`(字符串或数组都一样),不按项合并。
从下一轮生效:空闲的会话立即按新提示重建,历史不变。和 `options` 一样记在会话日志里,
重建、空闲回收后再用、daemon 重启后恢复都保留;settings 文件不被改动。和那一层的其他键一样,
它派生的子 Agent 与团队成员拿到的也是这一层。

有一轮正在跑、或有后台任务在跑时拒绝,`SESSION_BUSY`,`data` 同 `session.setModel`;重建时旧引擎
没停下也是 `SESSION_BUSY`,提示词不变,会话被移出内存,同 `session.setModel`。上一轮的日志没写全、
重建前补写又失败时是 `ENGINE_ERROR`(`kind: "persist_failed"`,§7.2),提示词不变,会话照旧。`append` 与 `override`
都没给,或类型不对、数组里的 `name` / `text` 不合上面的规矩 → `INVALID_PARAMS`;会话不存在 → `SESSION_NOT_FOUND`。

#### `session.setPermissionMode`

```jsonc
// → params
{ "session_id": "…", "mode": "plan" }       // §4.5 的取值,bubble 除外
// ← result
{ "session_id": "…", "mode": "plan", "requested": "plan", "clamped": false }
```

切换会话的权限模式,**立即生效**:之后的每一次权限判定都按新模式,正在跑的这一轮里后面的工具调用
也是,所以有一轮在跑时也照收,不返回 `SESSION_BUSY`。已经在等回答的询问是按切换前的模式问出的,
答了照常算数;只有 `ExitPlanMode` 例外 —— 判定它(权限脚本、hook)或它的询问悬着时模式被切过,之后的
`permit` 也不再改模式,模型得到一个拒绝,说模式已被切成了什么。模型在下一步读到一条提醒,说用户把模式切成了什么。

夹紧同 `options.settings.permissions.mode`(§4.5):不宽于会话所在项目与场景的设置,除非设置开了
`permissions.client_may_loosen`;managed 层关了 `bypassPermissions` 时它得 `default`。`mode` 是会话
此刻所在的模式,`requested` 是请求的,`clamped` 为 `true` 表示设置把它夹窄了。`mode` 不是 §4.5 的取值、
或是只能由程序设的 `bubble` → `INVALID_PARAMS`;会话不存在 → `SESSION_NOT_FOUND`;场景不符 → `SCENE_MISMATCH`。
写不进会话日志 → `ENGINE_ERROR`(`kind: "persist_failed"`,§7.2):`plan` 以外的模式没记进规格时,模式不变;
进出 `plan` 没记进 `agix.session_state` 时,内存里的会话已经切过去了、按新模式判,只是日志里还没有,下一轮
结束时引擎再写,在那之前 daemon 重启会回到切换之前;不在内存里的会话,规格与 plan 状态哪一样没写进去,哪一样就没变。空闲的会话进出 `plan` 时回复
等引擎写完日志,最多 5 秒,没写完同样报 `persist_failed`;有一轮在跑时不等,那一轮结束时写。

切到 `plan` 等于从当前模式进入 `plan`,之后模型的 `ExitPlanMode` 获准时回到那个模式;已经在 `plan`
里就什么都不变。从 `plan` 切到别的模式就离开了 `plan`,不再记得进入前的模式。

`plan` 以外的模式记进会话的规格(`agix.session/spec` 条目,优先于 `options.settings.permissions.mode`,同
`session.setModel`;记的是夹紧后得到的模式,设置之后放宽也不会把它放宽),在不在 `plan`、退出后回到哪个记进 `agix.session_state`(§4.5);会话重建、驱逐后
再用、daemon 重启后恢复都保持,恢复时规格里的模式与 `plan` 退出后要回到的模式都按当时的设置重新夹紧。冷恢复时 `session.resume` 的
`options.settings.permissions.mode` 只在会话没记下 `options` 时采用,这里设的模式仍优先于它。会话不在内存里时只改
它的日志,不把它带回内存,下一次 `run_turn` / `session.resume` 照它来。

模式变了就给订阅者推 `permission_mode_changed`(§7,`source: "host"`);模型自己 `EnterPlanMode` /
`ExitPlanMode` 改了模式也推(`source: "model"`),`session.rewind` 回到 `plan` 里时也推(`source: "host"`)。
切到会话已经所在的模式不推。短时间里连着变很多次时,中间几次的帧可能不推,最后一次的会推;要确认读 `session.resume` 的 `permission.mode`。

子 Agent:已经在跑的子 Agent 留在它起来时的模式里,但它的每一次调用仍经父会话、按父会话此刻的模式约束
(父会话在 `plan` 或 `dontAsk` 时取两者中更严的,父会话是 `bypassPermissions` 时随父会话,只能收紧的 agent
类型除外,§10);此后
起的子 Agent 从切到的模式算起(父会话在 `plan` 里时,从 `plan` 退出后要回到的那个算起),agent 类型与
团队的 `permission_mode` 照 §10 再作用于它。

#### `session.history`

```jsonc
// → params
{ "session_id": "…", "offset": 0, "limit": 100 }
// ← result
{ "session_id": "…",
  "messages": [ ],          // 投影后的消息数组,元素形状见 §4.1
  "total": "…",             // 投影后的消息总数
  "offset": 0,
  "limit": 100,
  "has_more": false,
  "entry_count": "…",       // 原始日志条目数(≥ total)
  "active": true }
```

返回的是**投影后的消息视图**,不是原始 JSONL 行:压缩边界被合并、侧链条目被过滤。

每条消息在 §4.1 的形状(`role`、`content` …)之外还带:

```jsonc
{ "role": "user", "content": [ ],
  "seq": 0,                 // 在投影视图里的位置,即 offset 的单位
  "id": "…",                // 写下它的那一行的 id;压缩后的替换消息是 "<压缩行 id>#<序号>"
  "ts": "2026-10-20T08:00:00Z",
  "turn_id": "…",           // 哪一轮写的;旧日志没有,为 null
  "origin": "user",         // 见下
  "usage": { } }            // 产生这条回复的那次调用的用量;只有 assistant 有,旧日志为 null
```

`origin` 区分模型眼里同样是 user 的几种消息:

| 值 | 含义 |
|---|---|
| `user` | 人打的字 |
| `steer` | 人在一轮运行中送来的(`on_busy:"steer"`) |
| `command` | 人跑的斜杠命令展开后的内容 |
| `injected` | 引擎或工具加的:提醒、召回的记忆、skill 的指令 |
| `tool_result` | 对上一条回复里工具调用的应答 |
| `assistant` | 模型的回复 |
| `compact` | 压缩后替换进来的消息,没有自己的一行 |
| `extension` | 插件写进 transcript、按项目配置对模型可见的条目 |
| `null` | 旧日志,不知道 |

界面只把 `user` / `steer` / `command` 画成人说的话。`turn_id` 与实时帧的 `turn_id`
相同,`seq` 与 `step_end` / `turn_complete` 帧的 `seq`、`session.subscribe` 的
`last_seq` 同一套坐标(§5.6),所以实时看到的一轮和事后读到的一轮可以逐条对上。

`limit` 默认 100,上限 500,超了**夹紧不报错** —— 响应回显实际生效的 `limit`,配合
`total` / `has_more` 就能知道自己被夹了。`offset` / `limit` 传成负数或非整数一律
退回默认值,同样不报错。

侧链会话有自己完整的 transcript,用它自己的 `session_id` 查即可。

#### `session.calls`

会话录像里的模型调用,旧的在前。只读,与 `session.history` 同一敏感级别(录像不脱敏,见
`session.create`);会话不在内存时从日志读它记下的 `recorder`,不建引擎、不把会话带回内存。

```jsonc
// → params
{ "session_id": "…", "from": 0, "limit": 200, "generation": 1763200000000 }   // 后三项可选
// ← result
{ "session_id": "…",
  "recording": { "name": "…", "created_at": "2026-11-15T09:46:40Z",
                 "covers": "latest_run", "generation": 1763200000000 },   // 没录:null
  "calls": [ { "index": 0,
               "session_id": "…", "parent_session_id": null, "agent_type": null,  // 子 Agent 的带父会话与类型
               "turn_id": "…", "turn": 1, "step": 0,
               "purpose": null,          // 对话调用为 null;辅助调用是 compact / title / memory …
               "ts": "2026-11-15T09:46:40Z", "provider": "anthropic", "model": "…",
               "input": { "system": [ { "source": "identity", "chars": 1234, "digest": "879106ac742f0567" } ],
                          "tools": 23, "tools_digest": "286e970158096479", "messages": 8 },
               "output": { "chars": 456, "text_preview": "…", "thinking_chars": 0,
                           "tool_uses": [ { "id": "…", "name": "Read", "input_preview": "{\"file_path\":\"…\"}",
                                            "result_is_error": false } ] },
               "status": "ok",           // ok | cancelled | error | in_flight | unfinished
               "stop_reason": "tool_use",
               "usage": { "input_tokens": 10, "output_tokens": 20,
                          "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0 },  // §3.5;没结束为 null
               "timing": { "sent_at": "2026-11-15T09:46:40Z", "ttft_ms": 800, "duration_ms": 4200 },
               "error": null } ],        // status 为 error 时 { kind, message, http_status, retry_after_ms }
  "next": 1, "total": 1, "damaged": 0 }
```

只有录制(`recorder.mode:"record"`)的会话有录像;没录的、录了但还没调用过模型的,`recording`
为 `null`、`calls` 为 `[]`,不报错。子 Agent 与辅助调用都在根会话的录像里(见 `session.create`),
各行的 `session_id` 写明是谁的;传子会话的 id 读不到它们。

`digest` 与 `tools_digest` 是录像里那一块的 blob id(规范化 JSON 的 SHA-256 前 16 位十六进制,系统块各一个,
工具目录按发出的顺序一个):两次调用的值相同,发出去的那一块就相同,宿主不必取全文去比。录像已不再持有
那一块时 `chars` / `tools` 为 `null`,`digest` 仍在。

`text_preview` 是回答文字的前 200 个字符,`input_preview` 是工具入参紧凑 JSON 的前 200 个字符(入参畸形的工具调用,`malformed` 为真,取原始入参文本的前 200 个字符),
被截断时两者末尾加 `…`(按字符,不按字节)。`result_is_error` 取自同一会话的下一次对话调用(`purpose` 为
`null`)带的消息里、`tool_use_id` 对得上的 `tool_result` 的 `is_error`;只在这次调用成功结束、且下一次调用确实接着它(那条调用之前最后一条 assistant 消息带的正是这次调用的全部工具调用 id)时才有,还没有下一次调用、对不上,或读它时有一条消息体超过 1 MiB 时整个字段省略。
这样画调用列表不必逐条 `session.call`。

`index` 是调用在录像里的位置,`session.call` 用它取全文。`from` 是起始的 `index`,`limit` 默认
200、上限 500,超了夹紧不报错。`total` 是整份录像的调用数,`damaged` 是读不出来、跳过的行数。

**跟随一个正在跑的会话。** 下一次从 `next` 读,只回新写的。`next` 停在第一条还在流式返回的
调用(`status:"in_flight"`)上,下一次连它一起再读一遍 —— 按 `index` 覆盖之前读到的那行。会话
不在内存时,没有结束的调用是 `unfinished`(写它的进程已经不在了),不挡 `next`。`ttft_ms` 从发出
请求到第一个响应事件,`duration_ms` 到调用结束;还没到的为 `null`。

**`generation`。** 录像被重写时变(daemon 重启后会话的第一次调用,见 `session.create`),只用来比较
相等。带上一次读到的 `generation` 而录像已经重写过 → `INVALID_PARAMS`,`data.generation` 是当前的
(§9.1),宿主从 0 重读。不带就不检查。

会话不存在 → `SESSION_NOT_FOUND`。

#### `session.call`

一次调用的全文:发出去的系统块、工具目录、消息和调用参数,以及回来的应答。

```jsonc
// → params
{ "session_id": "…", "index": 0, "generation": 1763200000000 }   // generation 可选,同 session.calls
// ← result
{ "session_id": "…", "index": 0, "generation": 1763200000000,
  "request": { "session_id": "…", "parent_session_id": null, "agent_type": null,
               "turn_id": "…", "turn": 1, "step": 0, "purpose": null,
               "provider": "anthropic", "model": "…", "max_tokens": 8192,
               "thinking": "off", "effort": null,
               "system": [ { "role": "system", "cache": "ephemeral", "source": "identity",
                             "origin": "kernel", "text": "…" },
                           { "blob": "s.0123456789abcdef", "bytes": 70000 } ],
               "tools": [ { "name": "Read", "description": "…", "input_schema": { } } ],
               "messages": [ { "role": "user", "content": [ { "type": "text", "text": "…" },
                                                             { "type": "image", "media_type": "image/png", "bytes": 5 } ] } ],
               "input_map": null },
  "response": { "status": "ok", "text": "…", "thinking_chars": 0,
                "tool_uses": [ { "id": "…", "name": "Read", "input": { "path": "/x" } } ],
                "stop_reason": "tool_use", "usage": { }, "ttft_ms": 800, "duration_ms": 4200, "error": null } }
```

`request.system` 是组装好的系统块,按顺序、逐字是发出去的那份:`source` 是块名
(`ARCHITECTURE.md` 的提示块名),`origin` 是谁放的(`kernel` / `config` / `plugin:<id>` /
`script:<路径>` / `host:<名>` / `config:<名>`;后两个是 `prompt.append` 数组的块,见 `session.setPrompt`),`cache` 是缓存标记。`messages` 的元素是发给模型的消息;图片与文档的 base64 不给,
只留 `type`、`media_type` 与解码后的字节数 `bytes`。工具参数解析失败的调用,`tool_uses` 里那一项的
`input` 是原始文本、带 `"malformed": true`。

**大块给引用。** 单帧有上限(§1),所以系统块、消息、整张工具目录这几种部分,哪一块的 JSON 超过
64 KiB、或者前面内联的已经累计到 4 MiB,就换成 `{"blob": "<s|m|t>.<16 位十六进制>", "bytes": n}`
(`s` 系统块、`m` 消息、`t` 工具目录;`bytes` 是 `session.callBlob` 回答的那份 JSON 的长度),用
`session.callBlob` 取。录像里已经没有的块是 `{"missing": "<id>"}`。`response` 总是内联。

`index` 超出录像 → `INVALID_PARAMS`;会话没有录像 → `INVALID_PARAMS`;会话不存在 → `SESSION_NOT_FOUND`。

#### `session.callBlob`

```jsonc
// → params
{ "session_id": "…", "blob": "s.0123456789abcdef" }
// ← result
{ "blob": "s.0123456789abcdef", "bytes": 70000,
  "value": { "role": "system", "cache": null, "source": "…", "origin": "plugin:p1", "text": "…" } }
```

`value` 就是 `session.call` 本该内联的那一块,形状相同。`blob` 只认 `session.call` 给出的形式,
别的(包括带路径分隔符的)→ `INVALID_PARAMS`;录像里没有这一块、或者它大到一帧装不下(15 MiB 以上)
→ `INVALID_PARAMS`。

#### `session.rerunCall`

把录像里的一次调用重新发给真实 provider:取第 `index` 次调用发出去的请求,先套上 `edits`,
再发,答新的应答与它和录下那次的差异。用来调提示词:改一块、重发、看差异。

```jsonc
// → params
{ "session_id": "…", "index": 0,
  "generation": 1763200000000,                 // 可选,同 session.call
  "edits": {                                   // 可选
    "system": [ { "index": 1, "text": "…" },   // 按位置换掉这一块的文字
                { "source": "memory.session", "text": null } ],  // 按块名;null 删掉这一块
    "drop_messages": [0, 1] },                 // 删掉这几条消息
  "provider": "…",                             // 可选,缺省是录下那次调用的 provider
  "model": "…" }                               // 可选,缺省是录下那次调用的模型
// ← result
{ "session_id": "…", "index": 0, "generation": 1763200000000,
  "provider": "anthropic", "model": "…",       // 实际接下这次调用的;没有 provider 接下时为 null
  "response": { "status": "ok", "text": "…", "thinking_chars": 0, "tool_uses": [ ],
                "stop_reason": "end_turn", "usage": { }, "ttft_ms": 800, "duration_ms": 4200, "error": null },
  "diff": { "same": false,
            "text": { "same": false, "recorded": "…", "live": "…" },
            "tool_uses": { "same": false, "steps": [
                { "status": "same", "recorded": "Read", "live": "Read" },
                { "status": "args_differ", "recorded": "Write", "live": "Write",
                  "args": [ { "key": "content", "same": false, "recorded": "…", "live": "…" },
                            { "key": "path", "same": true, "recorded": "a.c", "live": "a.c" } ] },
                { "status": "name_differs", "recorded": "Bash", "live": null } ] },
            "stop_reason": { "same": false, "recorded": "tool_use", "live": "end_turn" },
            "usage": { "recorded": { }, "live": { } } } }
```

**取哪一次。** `index` 与 `session.call` 的同一坐标,会话就是 `session.call` 读的那份录像(会话不在
内存时从日志读它记下的 `recorder`,不建引擎、不把会话带回内存)。一个会话的录像目录里只有一代:
`calls.jsonl` 在每次运行的第一次调用时重写(见 `session.create` 的 `recorder`),`generation` 就是
这一代开录的时刻(`session.calls` 的 `recording.generation`)。给了 `generation` 而录像已经重写过 →
`INVALID_PARAMS`,`data.generation` 是当前的;不给就用现在这一代。`session_id` 只按会话查,不当路径用,
格式不对 → `INVALID_PARAMS`。

录像目录由 `recorder.dir` 与 `recorder.name` 定。几个会话给了同一个固定的 `name`(与同一个 `dir`)就共用一个
目录、同一份 `calls.jsonl`:这时对其中一个会话 `rerunCall`,取到的可能是另一个会话录下的
调用,并经这个会话的(项目, 场景)对的 provider 重发。看得到的范围与 `session.call` 对这些会话已经给出的
一样,没有多出别的;要各看各的,就让每个会话用自己的 `name`(省略即用会话 id)。

**改动。** `edits.system` 每项用 `index`(系统块在这次调用里的位置,即 `session.call` 的
`request.system` 的下标)或 `source`(块名,即那里的 `source`)指定一块,两者只给一个;`text` 必须给,
是字符串就换掉这一块的文字(角色、缓存标记照旧),是 `null` 就删掉这一块。`drop_messages` 是
`request.messages` 的下标。所有位置都按录下的请求算,先全部解析再一起生效,所以删掉一块不会让后面
的 `index` 错位。以下都是 `INVALID_PARAMS`,什么都不发:`index` 越界、没有这个名字的块、几块同名
(改用 `index`)、同时给 `index` 与 `source` 或都不给、没给 `text`、两项改同一块;消息下标越界、重复,
或把消息删光;`edits` 里不认识的字段。删消息会不会留下没有应答的工具调用,由宿主负责;provider
拒收时见下。

**怎么发。** 走会话所在(项目, 场景)对的 provider 路由:从 `provider` 起按 `model.order` 换,模型
按 `model` 在各 provider 上的对应档位取,与会话自己的调用一样受各 provider 的 `quota` 约束、计入
配额。`provider` 没有定义、`model` 不是它收的模型 → `INVALID_PARAMS`;录下那次的 provider 现在已经
没有定义时同样,这时显式给 `provider`。别的请求参数(`max_tokens`、思考、推理强度、工具目录)照录下的。

**不改什么。** 不写录像(录像目录下的文件逐字节不变),不改会话的日志、上下文与状态,不计入会话
与它这一轮的花费(`session.get` 的 `usage`、token 预算);这次的用量只在结果里:`response.usage`,
以及 `diff.usage` 里录下那次与这次的对照。会话正在跑一轮时也可以调。

**结果。** `response` 与 `session.call` 的 `response` 同形,`status` 是 `ok`、`error`(没有 provider
接下、provider 拒收或中途出错,`error` 写明)或 `cancelled`(连接断开时取消)。不是 `ok` 时 `diff` 比的是
已经收到的那部分。`diff.tool_uses.steps` 按位置一一对照两边的工具调用:`same` 名字与参数都相同;
`args_differ` 同名而参数不同,`args` 列出两边全部参数键(某一边没有这个键时省略那一边的值);
`name_differs` 换了工具,或一边在这个位置没有调用(那一边为 `null`)。工具名、参数与 `stop_reason`
逐字比较;`text` 只说相同与否,两边原文都给。`diff.same` 是三者都相同。

会话没有录像 → `INVALID_PARAMS`;`index` 超出录像 → `INVALID_PARAMS`;录像里这次调用的某一块已经不在
(`session.call` 里是 `missing`)→ `INVALID_PARAMS`;会话不存在 → `SESSION_NOT_FOUND`。

#### `session.search`

按内容找会话:大小写无关、去掉首尾空白的子串,只比人说的话与模型答的文字。

```jsonc
// → params
{ "query": "postgres",
  "session_ids": null,      // 可选:只在这些会话里找,按给的顺序
  "limit": 20,              // 可选,有命中的会话数,上限 100
  "max_sessions": 500,      // 可选,至多读这么多个会话,上限 5000
  "deadline_ms": 2000,      // 可选,上限 10000
  "snippet_chars": 120 }    // 可选,片段的字符数,上限 500
// ← result
{ "hits": [ { "session_id": "…", "name": null, "seq": 12, "origin": "user",
              "ts": "2026-11-15T09:46:40Z", "snippet": "…postgres 的连接池…" } ],
  "scanned": 37, "truncated": false }
```

只比 `origin` 为 `user` / `steer` / `command` / `assistant` 的消息(`session.history` 的 `origin`)里的
文字块;夹在其中的 `<system-reminder>…</system-reminder>` 先剔掉,工具调用、工具结果、思考、
注入的消息都不比。每个会话至多一条命中,是日志里第一处;`seq` 与 `session.history` 同一坐标,
宿主能直接跳过去;`snippet` 是命中处前后共 `snippet_chars` 个字符,空白压成一个空格,截断处是 `…`;
`name` 是会话的名字(`session.rename` 或自动起名),没有为 `null`。

`session_ids` 省略时范围同 `session.list`:这个 daemon 服务过的项目里的会话,不含侧链,最近写过的
在前;给了就只找这些(侧链也找),不认识的 id 跳过,格式不对的 → `INVALID_PARAMS`。冷会话也找,
不会被带回内存;在内存里的会话,正在跑的那一步要等它写进日志才搜得到(同 `session.history`)。

三项上限任一先到就停,`truncated: true`:命中的会话数到了 `limit` 而后面还有没读的、范围里的会话
多于 `max_sessions`、或者过了 `deadline_ms`(在会话之间检查)。`scanned` 是读完的会话数。上限超了
夹紧不报错;`query` 是空的 → `INVALID_PARAMS`。

#### `session.resume`

把磁盘 transcript 投影回内存并建立运行实体。

```jsonc
// → params
{ "session_id": "…",
  "create_if_missing": false,   // 可选,默认 false
  "options": { } }              // 可选,同 session.create;会话已记录 options 时忽略
// ← result
{ "session_id": "…",
  "status": "already_active",   // already_active | resumed | created
  "active": true,
  "message_count": "…", "entry_count": "…",
  "name": null,
  "created_at": "…", "last_active": "…",
  "scene": "code",
  "project_trust": "untrusted", // trusted | untrusted | none,§3.6
  "trust_source": "default",    // default | user | host | null,§3.6
  "permission": { "mode": "default", "prompts": true, "prompt_timeout_secs": "…" } }
```

- 会话已经在内存里 → `already_active`,什么都不动,只报状态。
- 磁盘上有历史 → `resumed`。
- 磁盘上没有历史:`create_if_missing: false`(默认)时 `SESSION_NOT_FOUND`;
  为 `true` 时新建,`status` 是 `created`。"没有历史"只指转录文件不存在(或在但是空的)。
- 转录文件在但读不出来(没有读权限、中间有不是 UTF-8 或不是合法 JSON 的整行)→
  `INTERNAL_ERROR`,消息含 "transcript could not be loaded" 和读盘错误;带不带
  `create_if_missing` 都一样,不起引擎,也不往那份转录写任何东西。崩溃截断、没有换行结尾的最后
  半行不算读不出来,读的时候丢掉。`session.run_turn` 指名这样的会话同样如此。
- 场景不符 → `SCENE_MISMATCH`;没记场景 → 按当前场景恢复(§3.4)。
- 会话放在一个现在不受信的项目里(§3.6 末段)→ `PROJECT_UNTRUSTED`,消息含项目路径与
  "not trusted";不起引擎,不动那份历史。
- 目标是已终态的侧链 → `SIDECHAIN_TERMINAL`(§3.3)。
- 会话正在共用同一状态目录的另一个 daemon 里 → `INTERNAL_ERROR`,消息含
  "being written by another process";不会换 id 新建。`session.run_turn` 指名这样的会话
  同样如此。
- 会话刚被关闭或驱逐,它上一个引擎在宽限期(5 秒)内还没停下 → `SESSION_BUSY`,`data` 是
  `{session_id, reason: "previous_engine", current_turn_id: null}`;过一会儿再试。两个引擎不会同时写一份转录。
  `session.run_turn` 指名这样的会话同样如此。
- 目标是在 worktree 里干活的子 Agent 的侧链(`Agent` 的 `worktree`),而那个 worktree 已经不在、
  不直属于项目所在仓库根的 worktree 目录(缺省 `.wkts`)、或者它的 `.git` 链接不再指回仓库的 `.git` →
  `INTERNAL_ERROR`,消息含 "worktree" 与原因(如 "is gone")。不会在主检出里恢复它。侧链按它记下的
  项目恢复(`project_trust` 等与父会话的项目相同),在 worktree 里干活的仍在那个 worktree 里。
- 会话因别的原因建不回来(例如它记录的 `provider` 已经不在配置里)→ `INTERNAL_ERROR`,
  消息写明原因。同样不会换一个新 id 建会话顶替,`session.run_turn` 也一样。

`permission.prompts` 为 `false` 意味着这个会话跑在 `bypassPermissions` 下、部署方的
managed 层(§10)也没有能让它询问的东西,不会有任何 `prompt_type:"permission"` 的 `prompt`
帧。`bypassPermissions` 会话在 managed 层有 `ask` 或 `deny` 规则(`deny` 规则读不透的命令,
如 `$CMD x`,是询问而不是拒绝)、或写了 `permissions.disable_bypass: true` 时,
`prompts` 为 `true`:这样的会话仍可能被询问,宿主要能回答。

#### `session.fork`

```jsonc
// → params
{ "session_id": "…", "at_message": 12 }   // at_message 可选,省略 = 整段复制
// ← result
{ "session_id": "…",            // 新会话 id
  "parent_session_id": "…",     // 源会话
  "forked_at_message": "…",
  "message_count": "…",
  "entry_count": "…",           // 实际复制的日志条目数
  "source_message_count": "…",
  "active": false }
```

`at_message` 数的是**投影后的消息**(和 `session.history` 同一套坐标)。超过总数会被
夹到总数;`0` 是合法的("只要这个会话的开场,不要任何一轮"):切在第一条消息之前,在那之前
记下的规格(建会话时的 `options`、第一轮之前 `setModel` / `setPrompt` / `setSkillDirs` 设的)照样带过去。不是非负整数则
`INVALID_PARAMS` —— 这里**不**像 `limit` 那样宽容,分叉到错的地方会产出一个看着
像样的错误分支。

分叉的结果**写到磁盘就完了,不会被拉进内存**:源会话完全不受影响,新 id 表现得像
任何一个有历史的 id,下一次 `session.resume` / `session.run_turn` 带上它就是。

有意不复制的:**侧链条目**;源的 `Meta` 行(换成一条新的,记下 `parent_session_id`);
**文件 checkpoint 与回滚条目**(`agix.checkpoint`、`agix.rewind`)——分叉和源会话共用同一个
工作区,在分叉上回滚到分叉点之前会把源会话的文件改回去,所以分叉从"没有可回滚的写"开始;
以及被 `session.rewind` 作废的行——分叉里只有源会话此刻的对话视图。这些都不改变投影出的消息。

整段复制(省略 `at_message`,或它不小于消息总数)时复制到日志末尾,最后一条消息之后
写下的扩展条目(`session.appendEntry`)也在;从中间分叉时截在第 `at_message` 条消息
所在的那一行。

会话的 `options` 与 `session.setModel` / `session.setPrompt` / `session.setSkillDirs` 记下的值在规格条目里,
随切点之前的行一起复制。录制的 `recorder` 去掉 `name` 再记一条:分叉录在自己的 id 下,不去重写源会话的录像。

分叉记着源会话作 `parent_session_id`,但它是 `"session_kind":"primary"`,不是侧链:对源会话
调 `session.close` 或 `session.delete` 不会删它。`session.list {parent_session_id}` 会把它和
源会话的侧链一起列出,`session_kind` 照实标成 `primary`。

#### `session.export`

把一个会话在引擎里能迁移的全部状态——它和它的侧链各自的转录、消息里的图片、扩展条目——
在同一时刻打成一个归档,写到宿主给的路径。

```jsonc
// → params
{ "session_id": "…",
  "path": "/abs/dir/S1.agix.zip",            // daemon 所在机器上的绝对路径
  "format": "zip",                            // 可选:zip(默认)| dir
  "include": ["sidechains", "attachments", "tool_results"] }  // 可选,省略即这三项;录像只在列了 "recording" 时带,
                                              // 文件 checkpoint 只在列了 "checkpoints" 时带;
                                              // 显式列 include 而不列 "tool_results" 就不带外置结果
// ← result
{ "session_id": "…",
  "path": "…",
  "format": "zip",
  "format_version": 1,      // 归档格式的版本,与 manifest.json 的相同
  "bytes": "…",             // 归档在磁盘上的大小
  "cut_at": "…",            // 快照时刻
  "sessions": [ ],          // 每个会话一项:session_id、kind、parent_session_id、entry_count
  "attachments": 0,         // attachments/ 下的文件数
  "recording": false,       // 带没带录像
  "warnings": [ ] }
```

**一致的快照。** 这个会话和它下面任意深度的侧链,在同一时刻截断:之后才写下的条目、之后才
起的子会话都不在归档里。导出不占用会话——正在跑的一轮照常跑完,归档里有的是它到**最近一个
跑完的步骤**为止写下的内容(一轮的每一步结束都会落盘),清单里这个会话的 `active` 为
`true`、`turn_state` 为 `running`。导出不恢复、不创建会话,也不检查场景。

**路径。** 必须是绝对路径;它所在的目录必须已经存在;目标本身必须不存在(那里有一个符号链接
也算存在)——导出从不覆盖。归档先写到同目录下的一个临时名,写完才换到目标名上,失败时临时
文件会被删掉,目标名上不会留下半个归档。目标不能落在会话历史目录(`projects/`、`sessions/`,
以及路径所在项目的 `.agix/state/history/`)之下。路径在 **daemon 所在的机器上**:经 TCP / WebSocket 连上来的客户端拿到的是那台机器上的
文件。

**归档的布局**(`format: "dir"` 时是同样结构的一个目录):

```
manifest.json
sessions/<session_id>/meta.json          Meta 行,加上名字、标签、用量计数
sessions/<session_id>/transcript.jsonl   原始日志,逐行照存(含 agix.* 条目与侧链条目)
sessions/<session_id>/history.json       整段 session.history 的消息(同样的 seq / id / origin)
sessions/<session_id>/entries.json       session.entries 的那些行
sessions/<session_id>/session_memory.md  有才有
sessions/<session_id>/runtime.json       会话在内存里时才有:权限模式、todo、目标;"durable": false
attachments/<sha256>.<扩展名>             消息里的 base64 图片解码后的本体,同样内容只存一份
recordings/<name>/calls.jsonl, blobs/    只在 include 列了 "recording" 时
checkpoints/<sha256>                     只在 include 列了 "checkpoints" 时:根会话日志里 agix.checkpoint
                                         记下的写前内容,还在的每份一个文件(不论属于哪一轮)
tool-results/<name>                      只在 include 列了 "tool_results"(缺省)时:转录里外置标记行指着的
                                         超长工具结果文件,扁平存放(一个归档只有一个根会话目录)
```

`history.json` 里的图片块,`source` 换成 `{"attachment": "attachments/<sha256>.png",
"media_type": "image/png"}`;`transcript.jsonl` 保持原样(图片仍是内联 base64),所以同一张
图在归档里有两份。`include` 不含 `"attachments"` 时没有 `attachments/`,转录和 `history.json`
里的图片都换成一个写着媒体类型、大小与 sha256 的文本块,清单的 `importable` 为 `false`。

**清单** `manifest.json`:

```jsonc
{ "format": "agix.session-export", "format_version": 1,
  "engine_version": "…", "protocol_version": 5,
  "cut_at": "…", "root_session_id": "…",
  "include": ["sidechains", "attachments", "tool_results"],
  "importable": true,
  "sessions": [ { "session_id": "…", "kind": "primary", "parent_session_id": null, "depth": 0,
                  "scene": "code", "project_root": "…", "title": null, "dir": "sessions/…",
                  "entry_count": 12, "message_count": 4, "active": false, "turn_state": "idle",
                  "terminal": null,               // 侧链跑完时是 "completed" / "failed"
                  "transcript_sha256": "…" } ],
  "id_map": { "<session_id>": "sessions/<session_id>" },
  "attachments": [ { "file": "attachments/….png", "sha256": "…", "media_type": "image/png",
                     "bytes": 1234, "refs": [ { "session_id": "…", "entry_id": "…", "seq": 0 } ] } ],
  "url_images": [ ],        // 只有 URL 的图片:引擎手里没有本体
  "unresolved_blobs": [ ],  // 外置内容的引用,而这个 daemon 没挂那个存储
  "forks": [ ],             // 从这些会话分叉出去的会话 id,不进归档
  "recordings": [ { "dir": "recordings/…", "sessions": ["…"], "calls": 3, "covers": "latest_run" } ],
  "checkpoints": null,      // 列了 "checkpoints" 时 { "objects": 3, "bytes": 1234, "pruned_before": null }
  "tool_results": { "dir": "<导出机器上的根会话结果目录>",   // 没列 "tool_results" 时为 null
                    "files": [ { "name": "<sid>-<tool_use_id>.txt", "sha256": "…", "bytes": 31000 } ],
                    "missing": 0 },                        // 标记指着、而文件已经不在的个数
  "warnings": [ ] }
```

`refs` 的 `entry_id` 是转录行的 id,或 `history.json` 里那条消息的 `id`;`seq` 是那条消息在
`history.json` 里的位置,只在转录里而不在投影视图里的为 `null`。`format_version` 只在布局或
清单的含义变了时才加。

**带什么、不带什么。** 带:`Meta`、整份原始转录(含引擎自己的 `agix.*` 条目和宿主的扩展条目)、
任意深度的侧链、图片本体、会话记忆,以及会话在内存里时的那部分运行状态。不带:分叉出去的会话和
它分叉自的会话(只记 id)、"本会话内允许"的授权、设置 / provider / API key / OAuth、输入历史、
`prompt_state.json`、遥测、团队目录、调度、运行中的任务。录像只覆盖**最近一次运行**(每次
录制会重写 `calls.jsonl`)。根会话的录像按它记下的 `recorder` 找,整棵会话树的调用都在里面;
更早的版本给子 Agent 录在以子会话 id 命名的录像里,那些在根会话的录制根目录与 daemon 自己的
录像根下按会话 id 找,找到的也带上。

转录里的外置标记行(`[Output too long for one result: … Saved in full to <路径> …]`)指向**导出机器上**
的路径;`session.import` 把它改写成新根会话的结果目录。缺外置文件的归档照样能导入,标记保持原样,
旧结果被清理时只剩"输出已清"。

归档与转录一样敏感:它就是这些会话说过的全部内容。带了 `checkpoints` 的还有项目文件的历史内容,
体积也可能远大于转录,所以缺省不带。

`include` 里有不认识的项、`format` 不是 `zip` / `dir`、路径不合上面的规则 → `INVALID_PARAMS`;
会话不存在 → `SESSION_NOT_FOUND`;写盘失败 → `INTERNAL_ERROR`。归档装回一个 daemon 用 `session.import`。

#### `session.import`

`session.export` 的逆:把一个归档(zip,或 `format: "dir"` 导出的目录)装成**新**会话。

```jsonc
// → params
{ "path": "/abs/dir/S1.agix.zip",   // daemon 所在机器上的绝对路径:zip 文件或导出的目录
  "project_root": "/Users/x/proj",  // 可选;省略 = 清单里根会话的 project_root,null = 无项目会话
  "keep_ids": false }               // 可选;true 时沿用归档里的会话 id
// ← result
{ "root_session_id": "…",
  "id_map": { "<归档里的 id>": "<新 id>" },
  "sessions": [ { "session_id": "…", "kind": "primary", "parent_session_id": null, "entry_count": 12 } ],
  "warnings": [ ] }
```

**什么都在写之前查完。** 路径必须是绝对路径、是普通文件或目录(符号链接不行),且不在会话历史目录
(`projects/`、`sessions/`,以及路径所在项目的 `.agix/state/history/`)之下。归档只读它的布局里认得的几项:`manifest.json`、
`sessions/<id>/transcript.jsonl`、`sessions/<id>/session_memory.md`,以及清单 `tool_results.files` 列出的
`tool-results/<name>`(文件名只能是 `[A-Za-z0-9_-]` 加 `.txt`,sha256 要对得上,否则 `INVALID_PARAMS`);条目名带 `..`、绝对路径或是
符号链接的 zip、目录里有符号链接的目录一律拒绝,条目数上限 100,000、解开后总量上限 2 GiB、单个
文件 512 MiB。清单要 `format` 是 `agix.session-export`、`format_version` 不高于这个 daemon 读得懂的
(更高的是更新的 Agix 写的)、`importable` 为 `true`(没带附件导出的有损归档不能装);每个会话的
`transcript.jsonl` 要与清单里的 `transcript_sha256` 一致、第一行是 `Meta`、`Meta.schema_version`
与每一行的 `v` 不高于这个 daemon 的、每一行读得成日志行;每个侧链的父会话都在归档里、根是
primary、不成环;根会话的场景要在这个 daemon 上可用;`project_root`(传的或清单的)要存在且是目录
——不存在时请传一个。任何一条不满足都是 `INVALID_PARAMS`,什么都没写。

**写什么。** 每个会话得到一个新 id(`keep_ids: true` 时沿用,其中任何一个在这个 daemon 上已经有了
→ `SESSION_EXISTS`,什么都不写)。转录的每一行**原样**写入,行 id、时间戳、`parent_id`、
`is_sidechain`、`turn_id`、`origin` 都保留——压缩行靠这些 id 指向前面的行,所以投影出的
`session.history` 与原会话一样长、一样的内容;只改结构化的几项:每行的 `session_id`、`Meta` 的
`parent_session_id` / `cwd` / `project_root`(换成目标的;无项目会话的 `cwd` 是 daemon 的全局数据目录)。
消息正文、工具结果里提到的旧 id、旧路径**不改**;唯一的例外是外置标记行:归档带了 `tool_results` 时,
文件落到新根会话的结果目录(`<local_data_dir>/state/tool-results/<新根 id>/`,无项目时没有 `state/`),标记行改指那里;归档里没有
的文件,标记不改,`warnings` 记一条。会话记忆一并带上。不带:`runtime.json`、`history.json`、
`entries.json`(它们是转录的派生视图)、附件文件(图片本来就在转录里)、录像、checkpoint 的写前内容与
`agix.checkpoint` 记录——所以装进来的会话里,`session.rewind` 碰不到归档之前写的文件;也不带会话的
规格(`agix.session/spec`:建会话时的 `options`、`session.setPrompt` 等),归档决定不了装进来的会话用什么
提示词、权限规则、遥测文件,第一次 `session.resume` 或 `session.run_turn` 带的 `options` 生效。

**不起会话。** 和 `session.fork` 一样,装进来的会话只在磁盘上;之后对新 id 的 `session.resume` 或第一条
消息把它载入。导入不改变目标项目的信任状态。写到一半失败会把已写的会话全部删掉。投影出的消息数与
清单记的 `message_count` 不一致时,`warnings` 里记一条。

场景不可用、归档不合上面的规则、`project_root` 不存在 → `INVALID_PARAMS`;`keep_ids` 的 id 已存在 →
`SESSION_EXISTS`(`data.session_id` 是那个会话);写盘失败 → `INTERNAL_ERROR`。

#### `session.close`

```jsonc
// → params
{ "session_id": "…" }
// ← result
{ "closed": "…",              // 注意:是 session_id 字符串,不是布尔
  "sidechains_deleted": 0 }
```

销毁运行实体并释放资源。**这个会话自己的 transcript 保留**,之后仍可 resume ——
但**磁盘上记着它作父会话的那些会话全部被删除**:侧链,以及从它分叉出去的会话
(§3.3)。仍在跑的先取消再删;删除失败只写日志,不会让 `close` 失败。返回时这个会话
目标的每次变化都已记进日志,紧接着的 `session.goal` 从日志读到的就是关闭前的目标。

这个会话正在跑的一轮被截断:它在内存里的订阅者(发起者除外)收到这一轮的终止帧
`error{code:"session_closed"}`(§5.5),发起者的 `run_turn` 返回 `ENGINE_ERROR`,`message` 以
`session_closed:` 开头。截断之前这一轮已经推出 `turn_complete` 的,就以那一帧为准,不再补发。
`session.delete` 同样如此。

#### `session.delete`

```jsonc
// → params
{ "session_id": "…", "dry_run": false }
// ← result
{ "session_id": "…", "deleted": true,
  "sidechains_deleted": 0, "sidechain_ids": [ ] }
```

**真正删除磁盘 transcript 与 `sessions/<id>/` 整个目录(会话记忆、metadata、输入历史、
文件 checkpoint 的写前副本 `checkpoints/`;放进项目的会话是项目里的 `state/history/<id>/` 加全局的索引),并级联删除记着它作父会话的全部会话(侧链,以及它的
分叉)。不可逆。**

`dry_run: true` 只返回将要删除的清单(`deleted` 为 `false`),不实际删除 —— 供宿主
做二次确认。

#### `session.inspectContext`

这个会话**下一次**调模型会带什么,但不真的调。会话不在内存里就先恢复它;正在跑一轮、
正在重建、或另一个上下文请求还没答完时返回 `SESSION_BUSY`。从受理到答复,会话归这次请求
独占:期间到达的 `run_turn` 按 `on_busy` 处理 —— `reject` 得到 `SESSION_BUSY`(`reason` 为
`"context_operation"`),`queue` 等它答完再跑。`session.dropMessages`、`session.editMessage`
相同。

```jsonc
// → params
{ "session_id": "…" }
// ← result
{ "basis": "next_step_without_new_input",
  "model": "…",                             // `session.setModel` 换上的那个,没换过就是设置路由到的
  "provider": "…",                          // 下一次调用路由到的厂商 id;会话指名的厂商优先;没有可用的路由(没有配置厂商,或路由没建起来)时为 null
  "max_tokens": 0,
  "reasoning": { "effort": "high", "effort_source": "settings",   // 同 session.get 的 reasoning;sidechain 见下
                 "thinking": "on", "thinking_source": "settings" },
  "system": [ { "name": "skills.catalog",   // 块名,见引擎的扩展点文档
                "role": "user",             // system:进系统提示;user:作为对话开头的一条消息
                "origin": "kernel",         // kernel | config | {"plugin":"…"} | {"script":"…"} | {"host":"…"} | {"config":"…"}
                "tokens": 0, "text": "…" } ],
  "tools": [ { "name": "Bash", "source": "builtin", "schema_bytes": 0, "tokens": 0 } ],
  "plugins": [],                            // 会话实际用的插件包 [{name, version, required_by?}];因别的包依赖而补进来的带 required_by
  "messages": { "count": 0, "tokens": 0 },  // 坐标同 session.history
  "total_tokens": 0,                        // 本地估算(与 BPE 分词相差约 ±12%)
  "compact_threshold": 0, "hard_cap": null }
```

对 sidechain(子 Agent 会话)调用时,`reasoning` 的 `settings` 指子 Agent 自己的设置:从父会话的设置
派生,agent 类型写了 `effort` 的已经覆盖进去,所以 agent 类型给的 `effort` 也报为 `settings`。

`basis` 说明这是什么:**现在开始一步、不带新输入**时的请求。真正的下一轮还会多出
用户这句话、为它召回的记忆、提交时 hook 注入的内容,所以总量会更大。装配时脚本扩展照常
运行(它们决定系统块),所以脚本自己的统计会把这一次也算进去。

#### `session.dropMessages`

把用户消息或工具结果从**此后的**上下文里去掉。消息本身留着,内容换成
`[removed by the host]`(图片去掉):后面的位置不变,工具结果仍然回答它的那次调用。
模型的消息不能去掉 —— 带签名的思考与工具调用去掉一半会被模型服务拒绝。

```jsonc
// → params
{ "session_id": "…",
  "at": [2, 5],     // 位置,坐标同 session.history
  "total": 8 }      // 可选:你看到的消息数;对不上(期间新增或压缩过)就拒绝
// ← result
{ "dropped": [2, 5] }
```

改动先作用到引擎内存里的上下文,再由引擎以 `agix.context` 扩展条目写进 transcript(和它
写这个会话其余日志的是同一个写者),所以恢复之后照样生效;`session.history` 看到的是改过
之后的内容。会改动前缀,模型服务的提示缓存从这一点往后失效。位置不对、是模型的消息、
`total` 对不上 → `INVALID_PARAMS`;会话正忙 → `SESSION_BUSY`(见 `session.inspectContext`)。
改了却没写进日志 → `INTERNAL_ERROR`。

#### `session.editMessage`

把一条用户自己写的消息(只有文字,没有工具结果)改成 `text`。规则同上。

```jsonc
// → params
{ "session_id": "…", "at": 0, "text": "…", "total": 8 }
// ← result
{ "edited": 0 }
```

#### `session.rewind`

把文件、对话或两者退回到之前的某一点。由宿主发起,模型没有对应的工具。

```jsonc
// → params
{ "session_id": "…",
  "to_turn": "t1",          // 三者恰好一个:to_turn | to_tool_use | to_start: true
  "scope": "both",          // 必填:files | conversation | both
  "force": false,           // 可选:覆盖会话之外改过的文件
  "allow_partial": false,   // 可选:跳过回不去的文件,而不是整个拒绝
  "dry_run": false,         // 可选:只算计划和问题,什么都不写
  "fork_first": false }     // 可选:先 session.fork 一份当前全貌,再回滚
// ← result
{ "session_id": "…", "rewind_id": "…", "dry_run": false, "scope": "both",
  "cut": { "to_turn": "t1", "to_tool_use": null, "to_start": false },
  "conversation": { "messages_before": 6, "messages_after": 2,
                    "removed_turns": [ { "turn_id": "t2", "prompt": [ { "type": "text", "text": "…" } ] } ] },
  "files": { "restored": [ { "path": "src/a.rs", "to": "file", "sha256": "…",
                             "outside_project": false, "unverified": false } ],
             "skipped": [ ],
             "overwrote_external": [ ] },   // force 覆盖了会话之外改动的文件
  "fork_session_id": null }
```

**退回到哪里。** `to_turn` 是这一轮刚结束时;`to_start` 是第一轮之前;`to_tool_use` 是那次工具
调用(或 `RunCode` 程序在它名下的子调用,id 为 `<id>.<n>`)写任何东西之前,只能配
`scope: "files"`——对话从一轮中间切开会留下没有结果的工具调用。被先前的回滚作废的轮次不能作
目标。`to_turn: T` 也会撤掉 T 结束之后、下一轮之前的写(比如后台子 Agent 在父会话空闲时写的)。

**文件。** 只回滚**文件工具**的写:`Write`、`Edit`、`NotebookEdit`、声明了写路径的插件工具、
`RunCode` 程序里的这些调用,以及子 Agent、团队成员、`session.startAgent` 起的子 Agent 的这些
调用(都记在根会话里)。**命令的副作用不回滚**:`Bash` 里的重定向、`sed -i`、`git` 操作、包
安装、删除都不在其中;MCP 工具的写也不在。每个被写过的文件恢复成切点之后**第一次**写之前的
内容;切点之后才建的文件被删掉(留下的空目录不删)。按物理时间线算:已经被回滚作废的轮次里的
写照样算,所以多次回滚、只回对话后再回文件都回到正确的内容。恢复用原地写,已存在文件的权限位
保留;被删掉后重建的文件拿缺省权限。git 的分支、索引不动,只动工作树文件。

回滚前逐个检查,有问题就整个拒绝(`REWIND_PRECONDITION`,什么都不改),`data.issues` 列出全部:

| `issue` | 什么情况 | 放过它 |
|---|---|---|
| `modified` | 文件此刻不是会话最后写成的样子——用户手改、`Bash` 改、别的进程改;会话最后一次写它没有 `post` 记录时无从比较,文件还在就同样算 | `force: true` 覆盖 |
| `unavailable` | 写前内容没留下(超过 `max_bytes_per_file` 只记了哈希、被配额清掉)、读不出来、不是普通文件,文件此刻已大到回滚前无法留副本,或副本与哈希对不上 | `allow_partial: true` 跳过 |
| `denied` | 写路径策略此刻拒绝这个位置(凭据、引擎配置、系统目录……) | `allow_partial` |
| `symlink` | 它此刻是符号链接,或上面某一级目录解析到了别处;回滚**从不**跟随链接写 | `allow_partial` |
| `worktree_removed` | 它在一个已经移除的 worktree 里 | `allow_partial` |

会话最后一次写它之后没来得及记下结果的(进程在写完之后死掉),无从判断文件是不是被人动过,
所以要 `force` 才覆盖,覆盖后 `restored` 里标 `unverified: true`。`force` 覆盖掉的会话之外的改动
列在 `files.overwrote_external`(路径与 `restored` 里的一致),`rewound` 帧里是同名字段。项目外但当年被批准过的写照常恢复,标 `outside_project: true`。
回滚的写不走询问与 allow/ask 规则、不受权限模式影响(宿主本来就是全权,当年那次写也已过了
判定),但重跑硬性的写路径检查;也不触发 `PreToolUse` / `PostToolUse`。**回滚会写项目文件,
由宿主负责让用户确认**——`dry_run: true` 先给出计划与问题,`files.skipped` 里是全部问题。

**对话。** 切点之后的消息从 `session.history` 里消失(`total` 变小,`seq` 从切点接着编),
日志里**不删**,只追加一条作废标记(原样的行在 `session.export` 的 `transcript.jsonl` 里)。
被作废的范围里有压缩时,视图回到压缩前,下一轮可能因此重新压缩;提示缓存从切点往后失效。
计划模式与 todo 回到切点时的样子,目标不动。`removed_turns` 是被撤掉的轮次与开启它的那条消息,
供宿主放回输入框。只回对话时,那几轮改过的文件保持现状,下一轮开头会提醒模型;只回文件时,
下一轮开头提醒模型这些文件被恢复了,编辑前要重新读。想留着被回滚的那一支,用 `fork_first: true`,
`fork_session_id` 是那份分叉。

**原子性。** 要么全做了,要么什么都没变:恢复到一半失败,已恢复的会按倒序退回原样,对话不动,
返回 `REWIND_FAILED`;只有退回也失败时才是部分完成,`data.partial` 为 `true`,`not_rolled_back`
列出留在恢复后状态的文件。进程在回滚中途死掉的,`session.checkpoints` 报 `interrupted_rewind`,
再发同一个回滚是安全的。

会话不在内存就先恢复。一轮在跑、在重建、另一个上下文操作或回滚还没完 → `SESSION_BUSY`
(`reason` 分别为 `turn_running` / `rebuilding` / `context_operation` / `rewinding`);后台任务或
团队成员在跑 → `SESSION_BUSY`,`reason: "background_tasks"`(它们会在回滚中途写文件)。不排队:
回滚的目标是看着某个状态选的。回滚期间到达的 `run_turn` 按 `on_busy` 处理。`session.interrupt`
对回滚无效。目标是侧链会话、参数不是恰好一个目标、轮不存在或已被作废、`to_tool_use` 配了对话、
会话不记 checkpoint 却要回文件 → `INVALID_PARAMS`。成功后向该会话全部订阅者推一帧 `rewound`
(§7),`dry_run` 与失败不推。

#### `session.checkpoints`

给宿主做"回滚菜单":还在对话里的每一轮,以及每一轮里哪些文件被写过。只读,会话冷着也能答,
不把它拉进内存。

```jsonc
// → params
{ "session_id": "…" }
// ← result
{ "session_id": "…",
  "turns": [ { "turn_id": "t1", "started_at": "…",
               "prompt": "…",                                   // 开启这一轮的那条消息的前 200 个字符
               "files": [ { "path": "src/a.rs", "tool": "Write", "agent": null } ],  // agent:写它的子 Agent 会话
               "restorable": true } ],
  "pruned_before_turn": null,                                   // 早于它的轮次回不去了(副本被配额清掉)
  "interrupted_rewind": null,                                   // { "rewind_id", "ts" }:开始了却没结束的回滚
  "enabled": true }                                             // 这个会话记不记 checkpoint
```

`turns` 按时间顺序,只列没被回滚作废的轮:撤了对话的回滚(`scope` 为 `conversation` / `both`)之后
被撤的轮不再列出;只撤文件(`files`)的回滚不动对话,那些轮照样列出。压缩与 `session.dropMessages`
不影响这里:压缩前的轮在 `session.history` 里已经合成摘要,这里照样列出,回滚到它们仍然可以。
子 Agent 的写算在它写下时最近的那一轮里。

**配额。** 设置段 `checkpoints`:`enabled`(缺省 `true`;`false` 时文件工具照常写,但不记快照,
`session.rewind` 要回文件就是 `INVALID_PARAMS`)、`max_bytes_per_file`(缺省 16 MiB,更大的文件只记
哈希,回滚时是 `unavailable`)、`max_total_bytes`(缺省 512 MiB,一个会话保留的写前副本总量)。
超过总量时,引擎按最旧的先清,并在转录里追加一条 `agix.checkpoint` / `pruned` 条目
(`{"before": "<envelope id>"}`:此条目之前的 `pre` 的副本可能已删),最新一次写的副本不清。
被清掉的轮次在这里是 `restorable: false`,`pruned_before_turn` 是第一个还回得去的轮;对它们
`session.rewind` 的文件部分报 `unavailable`,`detail` 里写明是被配额清掉的。项目层的这三项只能收紧
(关掉、调小)。

#### `session.tasks`

这个会话里在会话之外继续跑的工作:子 Agent(前台的和后台的)、团队成员、执行中的
Shell 命令与监视器,被 MCP server 转成任务的工具调用,以及安排给它的定时任务(§6.7)。和模型用 `TaskOutput` / `TaskStop`
看到的是同一份,另外列出活着的常驻团队成员。

```jsonc
// → params
{ "session_id": "…" }
// ← result
{ "active": true,           // 引擎是否在内存里;不在时只有定时任务
  "tasks": [
    { "task_id": "…",
      "kind": "agent",      // agent | teammate | member | shell | monitor | cron | code | ext | mcp_task
      "status": "running",  // running | waiting | done | failed | stopped;定时任务总是 waiting;member 为 idle | running
      "started_ms": 0, "ended_ms": null,
      "detail": { },        // agent:{agent_type,description,background,tool_use_id,agent_session_id,model,provider?,tag?,worktree?}
                            // teammate:{team,member,agent_session_id,model,provider?};member:{team,member};shell:{command,tool_use_id}
                            // monitor:{command,description,persistent,tool_use_id}
                            // code:{description,background,tool_use_id}(RunCode 的程序,docs/ptc.md)
                            // ext:{description,command,background,tool_use_id}(RunExt 的命令,docs/runext.md;command 是带引号规整后的命令行,background 总是 false)
                            //     宿主调的插件函数(plugin.call):{description,function,via:"host",background:false,tool_use_id}
                            // cron:{cron,prompt,recurring,durable,expires_ms}
                            // mcp_task:{server,tool,tool_use_id,mcp_task_id,mcp_status,status_message?,poll_interval_ms?,ttl_ms?}
      "usage": { },         // agent / teammate:它自己的模型调用到此刻的合计(§3.5 四字段)
      "structured_output": { },  // 带 output_schema 起的子 Agent 交了才有
      "waiting_for": [ "…" ],    // 仅 waiting(定时任务除外):在等宿主回答的 prompt_id
      "error": "…" }        // 仅 failed
  ] }
```

`model` / `provider` 是子 Agent(团队成员)的调用去的模型与 provider:`model` 是解析过的模型 id(档位名、`opus` /
`sonnet` / `haiku`、`inherit` 都换成真正的 id),`provider` 是 provider id,会话没有路由时没有这个键。登记时
(`task_started`)写的是预测:它的路由排在最前的那家,以及向那家要的模型;每次调用结束按这次实际接的那家与模型改写,
变了的那次 `task_updated` 里就是新值(路由回退、`provider_switched`)。

`agent_session_id` 是子 Agent 的侧链会话,任务登记时就定了,`task_started` 里已经有;
侧链在子 Agent 建成后才落盘,之前 `session.history` 读它是 `SESSION_NOT_FOUND`。`tag`
只有 `session.startAgent` 给了才有。`worktree` 只在用 `Agent` 的 `worktree` 起的后台子 Agent 结束、
留下了它的 worktree 时才有:`{path,branch,holds}`,`holds` 是留下的原因(`"uncommitted changes"`、
`"<n> commit(s) since it was made"`;git 读不出状态时是 `"a state git could not read"`)。原样未动的
worktree 与分支在子 Agent 结束时已删,不记。前台子 Agent 留下的 worktree 写在 `Agent` 的结果里。

`usage` 只算这个子 Agent 自己的调用,与它的侧链会话各次调用之和相等;它再起的子 Agent
另算在它们各自的任务上。前台子 Agent 的用量在父会话这一轮 `turn_complete` 的 `usage` 里(`usage_breakdown.delegates`,
§7.2),后台的算到父会话这一轮结束为止的那部分。父会话 `session.get` 的 `usage` 是会话的累计,
每个子 Agent 的每次调用一结束就在里面,后台的也是。

子 Agent 有询问在等宿主回答(§10)时是 `waiting`,`waiting_for` 列出这些询问的
`prompt_id`;答完(或超时)回到 `running`。它仍在跑:`stopTask`、`messageTask` 照常。

`mcp_task` 在 MCP server 对 `tools/call` 回了任务时登记(客户端声明了 tasks 扩展),随这次调用
结束:调用在原地等任务,完成的结果就是这次调用的结果,server 报失败则是错误结果。`mcp_status`
是 server 报的原始状态(`working` / `input_required` / `completed` / `failed` / `cancelled`),
`status` 按 `running`(含 `input_required`;在等宿主回答 server 的询问时为 `waiting`)、`done`、
`failed`、`stopped` 映射;`error` 是 server 给的失败消息(server 的文字,不受信)。它不算后台任务,
不挡重建。

`member` 是一个活着的常驻团队成员(`id` 形如 `member-…`):它交给的每条消息各是一个 `teammate`
任务,`detail` 与阶段成员的同形(`{team,member,agent_session_id,model,provider?}`,`agent_session_id` 是这个成员的
侧链,各条消息相同),`team` / `member` 与它的一致的其中一个在跑时它是 `running`,否则 `idle`;`started_ms` 是它建起的时刻。活着的成员会挡住重建
(`SESSION_BUSY`,`reason: "background_tasks"`),`session.stopTask` 拿它的 `id` 停掉它,之后它不再列出。

正在跑的全列出,已经结束的保留最近 20 个。会话不存在 → `SESSION_NOT_FOUND`。

#### `session.taskOutput`

`session.tasks` 里一件任务到目前为止产出了什么 —— 和模型的 `TaskOutput` 读的是同一份,只读、
不等它结束。

```jsonc
// → params:二选一,同时给 INVALID_PARAMS
{ "session_id": "…", "task_id": "…", "tail_bytes": 8192 }   // 要最后多少字节,默认 8 KiB,最多 256 KiB
{ "session_id": "…", "task_id": "…", "from_byte": 0 }       // 从这个偏移读到末尾
// ← result
{ "output": "…",            // tail_bytes:最后这么多字节(在字符边界上截);from_byte:从那里到末尾
  "truncated": false,       // tail_bytes:前面还有没给的,或者早已丢掉的;from_byte:dropped > 0
  "status": "running",      // 同 session.tasks 的 status
  "from_byte": 0,           // output 第一个字节的偏移
  "next_byte": 0,           // output 之后的偏移,下一次 from_byte 从这里接
  "dropped": 0,             // 要的起点到 from_byte 之间已经不在了的字节数;tail_bytes 时为 0
  "final": false }          // 输出已经结束,next_byte 不会再长
```

偏移是这件任务有史以来全部输出的字节数,从 0 起,只增不减;每件任务只保留最后 256 KiB,所以
`from_byte` 读到的是从要的起点和仍保留的最早一个字节两者中靠后的那个开始、到末尾的全部(至多
256 KiB)。要的起点落在一个字符中间时往后挪到字符边界,挪过的也算进 `dropped`。恒有
`要的起点 + dropped == from_byte`、`from_byte + output 的字节长度 == next_byte`。
`from_byte` 与 `tail_bytes` 同时给、不是非负整数、或者超过当前末尾 → `INVALID_PARAMS`,超过末尾时
message 里写着此刻的 `next_byte`。

`final` 在这件工作自己收尾之后才是 `true`:之后 `next_byte` 不再长,读它得到空的 `output`。
`status` 已经是 `stopped` 而 `final` 还是 `false`,是工作被停掉、正在收尾 —— 还会写几行。

Shell 命令的输出逐行记下,stdout 与 stderr 按到达的顺序交错;监视器的 stdout 逐行记下,
stderr 按到达的块原样记下,两者交错。`RunCode` 程序记 `log()` / `notify()` 行、每次子调用的
一行和最后的结果。子 Agent 与团队成员的 `output` 是它们每一轮结束时的回答文字,
不是过程 —— 过程走它们的侧链会话。会话不存在 → `SESSION_NOT_FOUND`;引擎不知道这个任务
→ `TASK_NOT_FOUND`;定时任务没有输出,也是 `TASK_NOT_FOUND`。引擎被回收出内存、或者任务结束得早
已被忘掉(只保留最近 20 个结束的),之前读到一半的也会变成 `TASK_NOT_FOUND`。

**跟随一件任务的输出**(宿主的"跟随"视图):

```
1. session.subscribe {session_id}
2. session.taskOutput {task_id}(要从头就 from_byte: 0)  → 记下 next_byte,显示 output
3. 每来一帧 task_progress(按 task_id):
   - 帧的 next_byte <= 手上的        → 已经有了,丢掉
   - 帧的 from_byte <= 手上的        → 接上 appended 里从 (手上的 - from_byte) 字节起的部分
   - 帧的 from_byte >  手上的        → 中间漏了(skipped_bytes 或丢帧):
                                      session.taskOutput {from_byte: 手上的} 补齐
   手上的 = 处理完的 next_byte
4. task_finished 之后读一次 from_byte: 手上的,final 为 true 就停
```

补读的 `dropped > 0` 表示那段已经被挤掉,明示"中间丢了 N 字节"即可。断线、订阅被取消
(`subscription_dropped`)之后照 1、2 重来,第 2 步用手上的 `next_byte`。

#### `session.stopTask`

```jsonc
// → params
{ "session_id": "…", "task_id": "…" }
// ← result
{ "stopped": true }         // 没有这个任务,或者它已经结束 → false
```

和模型的 `TaskStop` 是同一个入口:取消这件工作所在的令牌 —— Shell 命令连同它的整个
进程组一起结束,子 Agent 在下一个检查点停下;定时任务则是取消安排。被停掉的工具调用
给模型回的是"已取消",这一轮照常继续。MCP 任务则向 server 发 `tasks/cancel`(协作式,server
可能仍以完成告终,模型这边照样回"已取消");这一轮被打断时同样发。

`session.tasks` 里它立刻是 `stopped`、带 `ended_ms`;`task_finished` 要等工作收完尾 —— 收尾时
写的输出(进程退出前的几行、程序交回的结果)先随最后一帧 `task_progress` 出去。工作 15 秒内
没收完尾的,到时照样发 `task_finished`,输出就此截止。

#### `session.messageTask`

```jsonc
// → params
{ "session_id": "…", "task_id": "…", "message": "…" }
// ← result
{ "delivered": true }
{ "delivered": false, "reason": "…" }   // 任务不存在、已结束、不是子 Agent,或者它此刻不在一轮里
```

和模型的 `TaskMessage` 是同一个入口:消息进后台子 Agent(含团队常驻成员正在处理的那条)
正在跑的这一轮,在它下一个步骤边界读到,和用户在一轮中途发给主 Agent 的消息一样。
Shell 命令、监视器没有收件箱。会话不在内存里 → `SESSION_NOT_FOUND`。

#### `session.startAgent`

宿主不经模型,在一个会话下起一个子 Agent。和模型调 `Agent` 工具、`background: true`
起的是同一种东西:登记成这个会话的任务(`kind: "agent"`),发 `task_started` /
`task_finished`,`session.tasks` / `taskOutput` / `messageTask` / `stopTask` 照常可用,
侧链会话带 `parent_session_id`,`session.list {parent_session_id}` 列得出。

```jsonc
// → params
{ "session_id": "…",                 // 父会话
  "prompt": "…",
  "description": "…",                // 可选,进 task.detail
  "subagent_type": "code-reviewer",  // 可选,按 agent 目录取模型、provider、工具、skills、scene、permission_mode(可放宽,插件带来的类型只能更严)
  "model": "…",                      // 可选,压过 agent 类型与 model.tasks.subagent 的 model(模型 id、档位名,或 opus / sonnet / haiku、inherit)
  "provider": "anthropic",           // 可选,providers 里的 id,压过 agent 类型的 provider:子 Agent 的调用先发给它,其余照 model.order 回退
  "allowed_tools": [ ], "disallowed_tools": [ ],  // 可选,在 agent 类型之上再收窄,只会更少
  "permission_rules": [ ],           // 可选,写法见 §4.6;只有 deny 生效
  "output_schema": { },              // 可选,同 session.create;交上来的进任务的 structured_output
  "notify_parent": true,             // 可选,默认 true:结束时给父会话投 <task-notification>
  "tag": "…",                        // 可选,宿主的关联串,原样出现在 task.detail.tag
  "token_budget": 200000 }           // 可选,正整数:这个子 Agent 自己的 token 上限,口径同 turn.token_budget(§4.8);0 → INVALID_PARAMS
// ← result
{ "task_id": "…", "agent_session_id": "…" }
```

- 父会话空闲、正在一轮里都能起,互不阻塞;不在内存里时先恢复,同 `session.run_turn`。
- `subagent_type` 必须是这个会话有的 agent 类型(内置、用户目录、项目、插件的,按会话的 `plugins.allow` / `enabled`
  收窄之后):不是 → `INVALID_PARAMS`,`message` 写明这个名字并列出会话有的类型名(按名字排序,最多 30 个),不登记任务。
  不写(或 `null`)是通用子 Agent;`general-purpose` 是内置类型,照常可写。模型一侧同一条规则:`Agent` 工具的
  `subagent_type`、`TeamCreate` 成员的 `agent_type` 不认识时是这次工具调用的错误,内容相同,不起子 Agent;`TeamCreate`
  在任何成员起来之前就失败。
- provider:给了 `provider`(或 agent 类型写了)时,子 Agent 有自己的一条路由:这个 provider 在前,其余照
  父会话的路由(这一对(项目、场景)的 `model.order`,父会话指名了 provider 的连同它),健康与配额和父会话共用;
  档位名、`inherit` 按它解析,模型 id 不在它那里时取它同一档的模型。这里的 `provider` 可以是这一对定义了的任何
  一家;不在定义里 → `INVALID_PARAMS`。
  没给 provider 而 `model` 是一个有确定归属的模型 id —— 父会话路由里的某个 provider 的 `models` 列着它,或把它
  作档位;只是"什么模型都收"的 provider 不算 —— 且父会话的调用不会先去那里时,路由从它的归属起。都不是时子 Agent
  与父会话共用路由,同 `Agent` 工具不带这两项。
- 只有宿主能点名父会话路由以外的 provider。`Agent` 工具的 `provider` 与 agent 类型的 `provider`(项目文件、插件、
  用户目录的都算)必须在父会话的路由里:`Agent` 工具点名的不在定义里、或定义了但不在路由里,是这次工具调用的错误
  (后者说它不在这个会话的 `model.order` 里);agent 类型的只记一条告警、当没写。未受信项目的 agent 类型写的
  `provider` 在项目受信前不生效,这样的类型列在 `project.status` 的 `requires_trust`。给活着的团队常驻成员发消息时
  点名的 provider(或类型、模型 id 的归属)与它在跑的那家不同,是这次 `Agent` 调用的错误:活着的成员保持它起来时的路由。
- 子 Agent 随父会话的运行体结束:父会话被 `session.close` 或 `daemon.shutdown` 时,它被停下
  (`stopped`)。还有子 Agent 在跑的会话不会被驱逐。
- 计入 `agents.max_concurrent`,也受委派深度限制;超了 → `TASK_LIMIT`,不排队。
- 权限:子 Agent 的工具调用照父会话的权限判;`permission_rules` 里的 deny 规则先判,命中
  就拒。allow / ask 规则不生效 —— 宿主起的子 Agent 只能比父会话更严。询问怎么答见 §10。
- `output_schema`:子 Agent 交的不满足 schema 时被退回重交;结束时在 `task_finished.task`
  与 `session.tasks` 的 `structured_output` 上。
- 预算:子 Agent 的每次调用(连同它的旁路调用与再起的子 Agent)照常计入父会话的 `usage` 与
  `session.token_budget`。给了 `token_budget` 时它另有一份自己的上限,挂在会话的上限下面:每次调用前取两者
  (以及它自己这一轮的 `turn.token_budget`)里剩得最少的,所以只会比父会话更紧。用完时子 Agent 这一轮以
  `budget_exceeded` 结束,父会话与别的子 Agent 照常。`session.messageTask` 发去的消息进它正在跑的这一轮,
  用的是同一份上限。
- 宿主起的子 Agent 被任何一份 token 预算截停(它自己的、会话的、它这一轮的)时,任务结局是
  `status: "failed"`,`error` 为 `budget_exceeded: ` 后接它那一轮 `turn_complete.message` 的原文(§4.8),
  写明是谁的预算:`the task's`(`token_budget`)、`this session's`(`session.token_budget`)或
  `this turn's`(它这一轮的 `turn.token_budget`)。用完时如
  `"budget_exceeded: the task's token budget of 200000 is used up"`;剩下的付不起下一次调用的输入时如
  `"budget_exceeded: the task's token budget of 200000 has 1500 left, too little for the next call's input of about 6000 tokens and its reply"`。
  截停前它说的话照样在任务输出里,`notify_parent` 的通知是 `<status>failed</status>` 带
  `<error>`。模型用 `Agent` 工具起的子 Agent 被预算截停仍是 `done`,结果末尾的说明同样写明是谁的预算。
- `notify_parent: true` 的通知在父会话的下一轮开头读到;父会话空闲时不会因此自己开一轮。

#### `session.respondToPrompt`

```jsonc
// → params
{ "session_id": "…", "prompt_id": "…",
  "prompt_type": "permission",          // 可选,默认 permission;取值同那一帧的 prompt_type
  "decision": { "type": "permit" } }    // 见 §4.7
// ← result
{ "prompt_id": "…" }
```

回答模型的提问(`prompt_type:"question"` 的帧)带 `answer` 而不是 `decision`:

```jsonc
// → params
{ "session_id": "…", "prompt_id": "…",
  "prompt_type": "question",
  "answer": { "type": "answer", "text": "a" } }   // 见 §4.7
```

回答 MCP server 的询问(`prompt_type:"mcp_elicitation"` 的帧)也带 `answer`:

```jsonc
// → params
{ "session_id": "…", "prompt_id": "…",
  "prompt_type": "mcp_elicitation",
  "answer": { "type": "accept", "content": { "color": "blue" } } }   // 见 §4.7
```

`prompt_type` 必须与这个 `prompt_id` 那一帧的相同:拿 `decision` 去答一个还悬着的
`question`(或反过来)→ `INVALID_PARAMS`。认不出的 `prompt_type`、缺了或形状不对的
`decision` / `answer` 也是 `INVALID_PARAMS`。

**谁都能答,先答者生效。** 提问广播给该会话的全部订阅者,任何一条连接都可以回答,
不必是发起 turn 的那条。`prompt_id` 未知(已超时、已被别人答掉)时**静默成功** ——
调用方要的是"这个提问被答掉",而它确实被答掉了。引擎收下回答后推一帧
`prompt_resolved`(`resolution: "answered"`,§7)给全部订阅者,别的 tab 据此撤下这个提问;
超时等没人答的情况推的是 `resolution: "withdrawn"`,`reason` 说是超时还是被中断。回答与超时
只有一个算数:回答先到引擎的,即使超时恰在此时到了,这个回答照样生效、推的是 `answered`;超时
先到的,之后的回答静默丢弃。

会话不在内存里 → `SESSION_NOT_FOUND`(这个和 `session.close` 不一样:答一个没在跑
的会话的提问不可能是"已经完成了")。`prompt_type` 为 `host_tool_call` → `INVALID_PARAMS`:
宿主工具的调用用 `session.respondToolCall` 回答;`hook_call` 同理,用 `session.respondHook`。

未在设置 `permissions.prompt_timeout_secs`(`agixd --permission-prompt-timeout` 是它的简写,
默认 300 秒)内回答的提问**失败关闭**:权限提问按拒绝
处理,turn 带着一条 error `tool_result` 继续;模型的提问以"用户没有回答"作为
`AskUserQuestion` 的结果交还模型;MCP server 的询问回 server `cancel`。都不会挂死。

#### `session.respondToolCall`

回答一帧 `host_tool_call`(§7.4)。`result` 与 `error` 恰好给一个:

```jsonc
// → params
{ "session_id": "…", "call_id": "…",
  "result": { "content": "echo: ping",       // 字符串,或 [ContentBlock](§4.1 的 text / image)
              "is_error": false } }          // 可选;true = 工具跑了、报告失败
// ← result
{ "call_id": "…" }
```

```jsonc
// → params
{ "session_id": "…", "call_id": "…",
  "error": { "message": "buffer not open" } }   // 宿主没能执行它
```

两种在模型那里都是 `is_error` 的结果,差别同内建工具:`result.is_error:true` 是"工具跑了、
说自己失败",不影响同批并发的工具;`error` 是工具没能执行,和别的工具执行失败一样取消同批
还在跑的调用,模型读到 `` host tool `HostEcho` failed: <message> ``。回答受 §1 的单帧上限约束,
进模型前照常经过工具结果的预算与外置。

两个都给、都不给、`content` 形状不对、图片的 `media_type` 不是 png / jpeg / gif / webp 或解码后超过 3.5 MB(同 MCP 结果里的图片) → `INVALID_PARAMS`,调用仍悬着,可以改了再答;`call_id` 是一帧还悬着的 `prompt`
的 `prompt_id` → `INVALID_PARAMS`。**先答者生效**,`call_id` 未知(已答、超时、被取消)时
**静默成功**,同 `session.respondToPrompt`。会话不在内存里 → `SESSION_NOT_FOUND`。

**连接提供的工具只有提供者能答。** `daemon.setHostTools` 提供的工具的调用,只有提供它的那条连接
的 `session.respondToolCall` 算数,别的连接 → `INVALID_PARAMS`;`options.host_tools` 的调用仍然
谁都能答、先答者生效。

#### `session.reportToolProgress`

宿主在处理一次 `host_tool_call` 的过程中报进度。

```jsonc
// → params
{ "session_id": "…", "call_id": "hc_…",
  "message": "row 42 of 120",
  "progress": 42,                // 可选
  "total": 120 }                 // 可选
// ← result
{ "call_id": "hc_…" }
```

键与 `tool_progress` 帧(§7.3)的 `message`、`progress`、`total` 完全相同。引擎为这次调用的
`tool_use_id` 向该会话的订阅者发 `tool_progress`,限流与别的工具一样(每次调用每秒至多一帧,
第一条立刻发,其后的合并);子 Agent 的调用进度包在 `subagent_progress` 里。两种宿主工具
(`options.host_tools` 与 `daemon.setHostTools`)都能用。

`call_id` 未知或调用已结束时**静默成功**。连接提供的工具,非提供者的连接调用 →
`INVALID_PARAMS`;`call_id` 是一帧 `prompt` 或 `hook_call` 的 id → `INVALID_PARAMS`。
会话不在内存里 → `SESSION_NOT_FOUND`。


#### `session.respondHook`

回答一帧 `hook_call`(§7.6)。

```jsonc
// → params
{ "session_id": "…", "call_id": "hk_…",
  "decision": "block",                     // 可选,"block" 或 "proceed"
  "message": "rm -rf is not allowed",      // 可选,拦下的理由
  "additional_context": "…" }              // 可选,见下
// ← result
{ "call_id": "hk_…" }
```

`session_id` 用帧外层 `session.event` 的(根会话;不带会话的 `plugin.call` 发出的,是那次调用的
临时会话 id,调用结束就不认了)。只有登记这条 hook 的连接能回答,别的连接 → `INVALID_PARAMS`。能起作用的字段只有三个,别的 hook 回答里的字段这里不收:

- `decision:"block"`:拦下,`message` 是理由。`PreToolUse`:工具不执行,模型读到
  `` Blocked by PreToolUse hook: <message> ``(前缀与设置里的 hook 一致),不再问权限;
  `PostToolUse`:结果换成 `` Denied by PostToolUse hook: <message> ``;`Stop`:这一轮不结束,
  `message` 作为提醒交还模型;`UserPromptSubmit`:这一轮以 `stopped_by_hook` 结束。省略或
  `"proceed"` 放行。
- `additional_context`:一段文字,在模型的下一步之前放进 `<system-reminder>`(`PreToolUse` 的
  在权限判定通过之后才送)。长度限制同设置里的 hook。

`decision:"approve"` → `INVALID_PARAMS`(宿主 hook 不能替人批准);出现 `updated_input`、
`continue`、`rewake` 等别的字段 → `INVALID_PARAMS`(宿主 hook 看得到输入,不能改写它)。
`call_id` 未知或已经结束(已答、超时、被取消)时**静默成功**,同 `session.respondToolCall`。

### 6.4 `config.*`

#### `config.get`

```jsonc
// → params
{ "scene": "code", "project_root": null,
  "tier": "effective",            // global | scene | project | cli | effective,默认 effective
  "include_secrets": false }
// ← result
{ "scene": "code", "project_root": null, "tier": "effective",
  "settings": { },
  "managed": null }
// ← result(tier 是 global / scene / project 时)
{ "scene": "code", "project_root": null, "tier": "global",
  "settings": { },                // 这一层的 settings.json
  "local": null,                  // 同一层的 settings.local.json,没有就是 null
  "revision": "r1:9f2c…",         // 这一层两个文件现在的样子,给 config.set 的 if_revision
  "managed": null }
```

`tier: "scene"` 读的是加载器为这个场景读的那个目录(daemon 启动时的场景是它的配置根,其余场景是
全局层下的 `scenes/<id>`)。`local` 与 `revision` 只在读 global / scene / project 时有;文件不存在时
`revision` 按空文件算。

`tier: "cli"` 读的是 `cli` 层——`agixd --set KEY=VALUE`(及其简写 `--max-tokens`、
`--permission-prompt-timeout`)或嵌入方的 `EmbedConfig.settings`——原样:`settings` 是这一层的 JSON 对象,
没有这一层时是 `null`;不带 `local` 与 `revision`,`managed` 为 `null`。这一层叠在每一对 `(项目, 场景)` 的
三层设置文件之上,按全局层的规矩合并(`app` 照收,不受项目信任门禁),`permissions` 的规则列表与各层的
并列生效;`config.reload` 重读的是它下面的文件,它本身不变。`config.set` 不写它。

`managed` 只在 `tier: "effective"` 时有内容:部署方的 managed 层
(`{ "origin", <它设的键,设置文件的形状>, "permissions": { "allow"?, "allow_exact"?, "ask"?, "deny"?, "disable_bypass", … } }`,
不能执行时另有 `error`、不带别的键),没有这一层时是 `null`。它设的值已经套进 `effective` 的 `settings`,压在各层之上;
规则列表不并进——合并后的 `settings` 不含 `permissions` 的规则列表,各层设置文件里的规则与 managed 层的规则都在
别处单列,裁决时一起生效。

**该层没有文件时 `settings` 是 `null`,不是 `{}`** —— "这层没配"和"这层配了个空的"
是两种状态,配置界面得能区分。`tier: "project"` 而没给 `project_root` 同理。
`tier` 取值非法 → `INVALID_PARAMS`。脱敏规则同 `scene.describe`,`local` 与 `managed` 也一样脱敏
(`managed.error` 里 URL 的密码与像令牌的词遮住)。

#### `config.explain`

一个设置键为什么是现在这个值:它经过的每一层、在那一层的值和结局。`config.get` 给的是各层原样或合并结果,
回答不了"这个值是从哪来的、为什么没用上我写的"。

```jsonc
// → params
{ "key": "turn.max_model_calls",       // 必填,点分;给一个块(如 "turn")就逐个解释其下的键
  "scene": "code",                     // 可选,省略 = 这个 daemon 的场景
  "project_root": "/Users/x/repo",     // 可选,省略 = 这个 daemon 自己的项目;null → INVALID_PARAMS
  "include_secrets": false }           // 可选,同 config.get
// → params(连会话自己那一层一起解释;这时不带 scene / project_root,用会话的)
{ "key": "turn", "session_id": "…" }
// ← result
{ "key": "turn.max_model_calls",
  "keys": [
    { "key": "turn.max_model_calls", "value": 25,
      "scope": "session",              // process | pair | session:在哪一级定下(键表 settings::keys)
      "read_at_start": false,          // 只在进程启动时读;为 true 时另有 restart_pending
      "layers": [
        { "tier": "default", "origin": "built-in", "value": 25, "outcome": "applied" } ] } ],
  "truncated": false }
```

每次调用都照加载时的样子把这一对 `(项目, 场景)` 的设置文件重读一遍(与 `config.reload` 读到的相同,但不替换
daemon 手里的那份),各层的判法就是加载器自己的判法,不另算一套。

`keys` 里每个键:

- `value`:现在生效的值;给了 `session_id` 时是会话的,否则是这一对的。
- `layers`:由低到高——`default`(内置缺省,`origin` 为 `built-in`)、各层设置文件按加载器折叠的顺序
  (`global` / `scene` / `project`,每层的 `settings.json` 在前、`settings.local.json` 在后,后者带
  `machine_local: true`;场景层或项目层的目录就是全局目录时按 `global` 报)、`cli`(`agixd --set` 与
  `EmbedConfig.settings`)、`host`(嵌入方在内存里改过的值,只在有改动时出现)。`origin` 是文件路径,或
  `built-in` / `cli` / `host`;`value` 是这一层写的值,没写时不带。
- `managed`:部署方的 managed 层写了这个键时才有,`{origin, value, outcome}`。
- `session`:给了 `session_id` 时才有,`{requested, applied, outcome}`:`requested` 是会话自己那一层
  (`options.settings`,及 `session.setModel` / `session.setPrompt` 改过的)要的值,没要时为 `null`;
  `applied` 是会话实际用的。
- `restart_pending`:只对 `read_at_start` 的键(`app.history`、`app.session_cap` 等):`value` 与这个进程
  启动时读到的不同,重启后才生效。
- `truncated`:块里的键超过 64 个,只解释了前 64 个。

`outcome`:

| `outcome` | 出现在 | 意思 |
|---|---|---|
| `applied` | 各层、`managed`、`session` | 生效的就是这一层的值。`providers` 与 `permissions` 的规则列表(`allow`、`allow_exact`、`ask`、`deny`)各层拼接,写了的层都是 `applied` |
| `overridden` | 各层、`session` | 更高的一层写了这个键;在 `session` 上是会话之后另有决定(`session.setPermissionMode`) |
| `merged` | 各层 | 对象值,更高的层把自己的键并了进来;解释里面的键看每个来自哪层 |
| `not_set` | 各层、`session` | 这一层没写 |
| `withheld_untrusted` | `project` | 项目未受信(§3.6),这是只有受信项目才能写(或放宽)的值,被扣下 |
| `dropped_not_global` | `scene`、`project` | 只从全局层读的键(`app.*`),写在了场景层或项目层 |
| `ignored` | 各层、`managed`、`session` | 值放不进这个设置,带 `error` 说明;managed 上是放不进它下面的值 |
| `managed_overrode` | 各层、`session` | managed 层把它换成了自己的值;managed 关掉 bypass(`permissions.disable_bypass`)时,写了 `bypassPermissions` 的那一层或会话按 `default` 跑,也报这个 |
| `held_tighter_below` | `managed` | managed 的值比下面的更松,有松紧次序的键留下层更严的那个 |
| `clamped` | `session` | 会话要的值比设置更松,而设置不许客户端放宽(`permissions.client_may_loosen`),夹回设置的值 |
| `not_session_scope` | `session` | 这个键按进程或按 `(项目, 场景)` 定,会话那一层不收 |

没有设置读这个键 → `INVALID_PARAMS`(改过名的键,消息里给出新名字);同时给 `session_id` 与 `scene` /
`project_root` → `INVALID_PARAMS`;会话不存在 → `SESSION_NOT_FOUND`;场景未知 → `SCENE_NOT_FOUND`。
值的脱敏同 `config.get`。`ignored` 的 `error` 是解析器的原话,常把值本身引在里面:值属于上面会脱敏的
(或键本身就在 `headers` / `env` 下、叫 `token` 之类)时,不带 `include_secrets` 的回答只说这个键的值放不进它的
设置、不给原话;其余的原话里 URL 的密码与像令牌的词遮住。`daemon.doctor` 报的 `ignored_values` 与 managed 层
同样处理。

#### `config.set`

把若干个设置键写进指定的一层,宿主不必自己读改写设置文件、也不必自备目录锁。

```jsonc
// → params
{ "tier": "global",                    // 必填:global | scene | project
  "scene": "code",                     // 可选,省略 = 这个 daemon 的场景;tier:"scene" 时写的就是它那一层
  "project_root": "/Users/x/repo",     // 可选,省略 = 这个 daemon 自己的项目;null → INVALID_PARAMS
  "changes": {                         // 必填,1 到 256 个键
    "permissions.mode": "acceptEdits",
    "web.allowed_domains": ["github.com"],
    "telemetry.otlp_endpoint": null }, // null = 从这一层删掉这个键
  "dry_run": false,                    // 可选
  "if_revision": "r1:9f2c…" }          // 可选:这一层的 revision 不再是它 → SETTINGS_CONFLICT
// ← result
{ "tier": "global", "dry_run": false,
  "written_to": "/Users/x/.agix/settings.json",
  // "secrets_written_to": "/Users/x/.agix/settings.local.json", // 只在改了 settings.local.json 时
  "changed": ["permissions.mode", "web.allowed_domains", "telemetry.otlp_endpoint"],
  "rejected": [ ],                     // 只有 dry_run 才可能非空
  "effective": { "permissions.mode": "acceptEdits", "web.allowed_domains": ["github.com"],
                 "telemetry.otlp_endpoint": null },
  "overridden": [ ],
  "revision": "r1:…",
  // "warnings": [ "…" ],              // 只在写进 settings.json 的值里看得出凭据时,见下
  "providers": [ ], "order": [ ],
  "routing": { "ok": true, "warnings": [ ], "error": null, "router_rebuilt": true,
               "router_error": null, "mcp_connected": [ ], "mcp_failed": [ ], "mcp_connecting": [ ] } }
```

**键**是点分的设置路径:`model.effort`、`sandbox.env.allow`、`model.tasks.subagent`、
`plugins.allow`。不支持通配,也不支持数组下标——列表整个写。同一次写里一个键是另一个的前缀
(`web` 与 `web.allowed_domains`)→ 拒绝。名字里带 `.` 的 map 条目(provider id、server 名)
经它寻址不到,它们归 `config.setProvider` 与 `mcp.*`。

**值**:这一层在这个键上的值变成它——对象整个替换这一层原有的那棵(与下面几层照常合并),
`null` 从这一层的两个文件里删掉这个键,删空的上级对象一并去掉。所以高层写不出字面的 `null` 去盖住
下层。`permissions` 的规则列表(`allow`、`allow_exact`、`ask`、`deny`)是各层各文件拼接的,这里只换这一层 `settings.json` 的那份,权限询问里
"永久允许"记进 `settings.local.json` 的规则不动(`null` 同样只删 `settings.json` 的)。

**密钥落 `settings.local.json`**:`mcp.servers.*.headers`、
`mcp.servers.*.env` 不管从哪个键写进来,都写进同一层的 `settings.local.json`
(0600);`settings.json` 里这些键原有的值在这次写入时挪过去(local 里已有的不覆盖)。其余键一律写
`settings.json`。挪的是整个键,不看值:URL 里的密码(`https://u:p@host`)或查询参数里的 token
(`?api_key=…`)、MCP server `args` 里 `--token` 后面的值、`url` 等字段照样写进 `settings.json`——
看值猜凭据会把同事要用的配置悄悄挪进不提交的文件,所以不做。这些情况答复里带 `warnings`:每个这样的键一句,
不复述值,判据与 `config.get` 的脱敏相同(密钥形状的键名、URL 里的密码与带 token / key / secret / password
字样的参数、`--token` 一类参数后的值)。凭据该放的位置是 MCP server 的 `headers` / `env`(自动落 local),
或者手写进 `settings.local.json`。

**全有或全无。** 任何一个键被拒,什么都不写,`INVALID_PARAMS`(全部只因项目未受信时是
`PROJECT_UNTRUSTED`),`data.rejected` 逐键列出理由(§9.1)。拒绝的种类(`kind`):

| `kind` | 什么情况 |
|---|---|
| `bad_path` | 不是点分的键,或与同一次写里的另一个键重叠 |
| `unknown_key` | 没有设置读这个键(值是对象时,对象里的键也逐个查) |
| `invalid_value` | 值放不进这个设置(按加载器的规则判:类型、枚举、一条一条的权限规则),或 `permissions.mode` 写了只能由程序设的模式 |
| `requires_trust` | 写进未受信项目的项目层,而它会被扣下(§3.6):未受信项目只能增加与收紧,判的是写入那一刻相对下面几层 |
| `not_at_tier` | `app.*` 只从全局层读,写在场景层 / 项目层 |
| `use_method` | 有专门的方法,`method` 指明:`mcp.servers` → `mcp.addServer` / `mcp.updateServer` / `mcp.removeServer`,`mcp.servers.<名>.enabled` → `mcp.setEnabled`,`app.trusted_projects` / `app.trust_all_projects` → `project.trust` |
| `not_writable` | `hooks`、`scripts`:它们执行代码,不经 RPC 写,宿主仍可直接改设置文件 |

宿主能连上这个 socket 就能直接改这些文件,所以按键拒绝不是安全边界,而是"写下去的东西加载器会照样
生效"的保证:手写进文件时会被加载器静默丢掉的值,在这里直接报出来。

**`dry_run: true`**:做写入之前的全部检查,在内存里套用、与其余各层合并并解析路由;不拿锁、不建目录、
不写。有键被拒时**照样成功返回**,`rejected` 列出来、`changed` 为空。路由解析不了不算拒绝,报在
`routing` 里(真写也一样:文件照写,已经在用的路由保留)。

**答复**:`effective` 是每个键写完之后在定位参数那一对 `(项目, 场景)` 合并结果里的值(密钥脱敏);
`overridden` 列出写进去了、但这一对的生效值不是它的键——更高的层、宿主覆盖或 managed 层盖住了它
(只对标量与列表算,`permissions` 的规则列表不算)。`revision` 是写完之后(`dry_run`:现在)这一层的
revision。其余字段同 `config.reload`。

**并发**:两个文件在那一层目录的锁下读改写(同 `config.setProvider`),锁内重新读文件、只套用这次
提到的键,所以两个连接同时写同一层的**不同**键都会留下;写同一个键时后到的赢。只有"先 `config.get`
再改一个列表再整个写回"会丢别人的更新:那时带上 `config.get` 回的 `revision` 作 `if_revision`,这一层
在这之间被改过(任何一个键、任何一个写入方)→ `SETTINGS_CONFLICT (-32028)`,`data.revision` 是现在的
revision,什么都不写,重读再写即可。锁等满 10 秒 → `SETTINGS_LOCKED`。

**写完之后**:daemon 自己那一对的设置重读,从这一层合出来的绑定作废(项目层只作废那个项目的,场景层
那个场景的,全局层全部),已有会话在下一轮前换上新配置。改动碰到 `sandbox.env` 或 `app.system_proxy`
时这一对的 MCP server 按新配置重连(这一对的 MCP 还没连过时不连,等它第一个会话),其余改动不动 MCP 连接。监视器不会把引擎自己刚写下的内容当成又一次
改动(第一次建出文件时例外,会多重读一次)。`plugins.*` 与手改文件一样,要到 `plugin.reload` 才影响已
加载的包。

其余失败:`scene` 未注册 → `SCENE_NOT_FOUND`;`tier:"project"` 而 `project_root` 不是目录 →
`PROJECT_NOT_FOUND`;项目层目录借用全局 / 场景层,或是指到项目之外(如链进另一个项目的 `.agix`)的符号链接
→ `INVALID_PARAMS`(同 `config.setProvider`、写 MCP 设置的方法);
已有的文件不是 JSON 对象或是符号链接 → `INVALID_PARAMS`,什么都不写。

#### `config.reload`

**无参**。重新读三层 settings.json、重新解析 provider 路由、重建 task router。

```jsonc
// ← result
{ "providers": [ ],           // 重载后定义的 provider id;一个都没定义时是引擎自带的 anthropic
  "order": [ ],               // 调用依次尝试的 provider(model.order;没写时是第一个定义的)
  "routing": {
    "ok": true,               // provider 路由得通:各档模型都在 models 里,order 里的 id 都有定义
    "warnings": [ ], "error": null,
    "router_rebuilt": true,   // 真的建出了一个可用的 router
    "router_error": null,
    "mcp_connected": [ ], "mcp_failed": [ ], "mcp_connecting": [ ] },
  "restart_required": [ ] }   // 值变了、但只在启动时读的键;重启 daemon 才生效
```

`restart_required` 列出只在启动时读的设置(`settings::keys` 表里标了 `read_at_start` 的键:`app.history`、`app.session_cap`、
`app.session_idle_timeout_secs`、`app.system_proxy`、`app.worker_threads`)里,现在读到的值与这个进程启动时不同的那些;
managed 设置文件(§10)也只在启动时读,它和启动时读到的不同时列一项 `managed`。这些键在运行中的进程里仍是启动时的值,
重启之后才生效;改回去之后就不再列出。`config.set` 与 `config.setProvider` 的结果带同一个字段。

`ok` 与 `router_rebuilt` 是**两件事**:配置可以路由得通(`ok: true`)却建不出客户端
(`router_rebuilt: false` + `router_error`),`api: openai_chat` 缺 `base_url` 就是这种。两种情况下
这一对都照旧用它上一次建成的 router;项目的信任在那之后变了(撤销信任时,受信时建的 router 可能用着项目自己的
provider),上一次的就不再用,按这时的设置走:定义了 provider 的不建会话,没定义的用引擎自带的。

`api` 取值：`anthropic`、`openai_chat`（Chat Completions）、`openai_responses`（OpenAI Responses API，`/v1/responses`，流式事件按类型解析；PDF 以 `input_file` 送出）。

重载按新配置重连 MCP server,最多等 10 秒:到点还没连完的列在 `mcp_connecting`,在后台接着连,
各自的结果随后以 `mcp_connected` / `mcp_connect_failed` 通知到达(同会话创建)。命令不存在、
没有执行权限、URL 解析不了的 server 不重试,直接进 `mcp_failed`。

```jsonc
// → params(都可选)
{ "scene": "code",            // 省略 = 这个 daemon 的默认场景
  "project_root": "/Users/x/repo-b" }   // 省略 = 这个 daemon 自己的项目
```

**没有按层失效的参数**:`tier` 不存在,重读永远是"这一对 `(项目, 场景)` 当前磁盘上
的三层整个重读一遍"。这个方法不知道动的是哪一层,所以缓存的绑定整个丢掉重建——全局层
与场景层是每个项目共同的下层,只失效一对是不够的。

`project_root` **不接受 `null`**:这个方法作用在**项目层**,而无项目会话没有项目层
(`config.get {tier: "project"}` 早就是这个答案)。传 `null` → `INVALID_PARAMS`。

失效**惰性生效**:已有会话在下一次 `run_turn` 之前若空闲则原地重建,忙碌的跳过本轮
下次再检查。**正在进行的 turn 不会被中断**;还有后台任务在跑的会话也算忙,这一轮照旧用原来的
配置,任务都结束之后才重建,所以后台任务不会因为配置变化被停掉。重建保留会话结束了的任务
(`session.tasks` / `session.taskOutput` 照查)和模型还没读到的 `<task-notification>`,也保留
`plan` 模式、todo 列表、`ToolSearch` 加载过的 deferred 工具、读过的文件记录;`plan` 之外的权限模式取新配置(在 `plan` 里的仍在 `plan` 里,退出后回到进入前的模式,§4.5)。重建时转录读不
出来,会话不重建、照旧在原来的引擎上跑,这次 `run_turn` 返回 `INTERNAL_ERROR`(见 `session.resume`)。

**多数情况下不需要显式调用**:daemon 监视每个项目的 `settings.json` /
`settings.local.json`,改动落盘后自己重读(去抖 300ms);文件内容没变的写入不算改动,引擎自己经
`config.set` / `config.setProvider` / `mcp.*` 写下、已经处理过的内容也不算。监视触发的重读知道是哪一层变了,
只作废从那一层合出来的绑定:项目层只作废同一项目的会话,场景层只作废那个场景的,全局层
作废全部。skills、agents 目录的改动不作废任何会话,新内容就地生效。不存在的设置文件与
skills、agents 目录监视不了;它被建出来之后(比如 `permit_always` 的 `local` 第一次写出
`settings.local.json`,项目第一次建出 `.agents/skills/` 或项目层目录的 `agents/`),下一次用到那一对 scene / 项目的
配置时(在上面新建会话,daemon 自己那一对还有 `commands.list` 等)会发现并重读,已经在跑的
会话要等到那时才作废。这个
方法留给监视起不来的时候(权限、内核 inotify 上限;`daemon.doctor` 会报),以及要已有会话
立刻看到一个新建文件的时候。

#### `config.getProvider` / `config.setProvider`

两者都接受 `scene` / `project_root` 定位参数,语义与 `config.reload` 一致:省略 = 这个
daemon 自己那一对,给路径 = 它服务的另一个项目,`null` → `INVALID_PARAMS`。
`setProvider` 改的是已经定义了这个 id 的那一层的 `settings.json`;这个 id 还没有定义时新建在 `tier` 指的那一层:
缺省 `tier:"project"`,写**被指定项目**的项目层目录里的 `settings.json`(`{project_root}/.agix/settings.json`),
`tier:"global"` 写全局层的 `settings.json`(`config.get` 的 `global` 那一份,对所有场景、所有项目生效),
`tier:"scene"` 写 `scene` 指的那个场景自己那一层(`config.get` 的 `scene` 那一份);
`written_to` 回的就是写的那个路径。`mcp.addServer` 与 `mcp.updateServer` 同样接受这两个参数。项目层目录借用了全局 / 场景层(仓库里提交的符号链接,或项目根就是放全局层的目录)时,`setProvider` 与 `mcp.addServer` 拒绝写,`INVALID_PARAMS` 并说明原因——写进去会落在对所有项目生效的那一份里;链到项目之外别处(如另一个项目的 `.agix`)同样拒绝——写进去改的是那个目录的设置。

**provider 路由是按 `(项目, 场景)` 解析的**:一个项目在自己的 settings 里配了
`providers` / `model.order`,它的会话就走它自己的路由,不是 daemon 那个项目的。

```jsonc
// → params (getProvider)
{ "scene": "code",                      // 都可选,见上
  "project_root": "/Users/x/repo-b",
  "provider_id": "deepseek",            // 可选:只要这一个;省略 = 全部
  "include_secrets": false }
// ← result (getProvider)
{ "providers": [ ],           // [ {id, vendor, api, base_url, api_key_env, models, tiers, quota, usage?, credential_present, builtin, defined_in?} ],按加载顺序
  "order": [ ],               // model.order 实际的路由顺序;没写时是第一个定义的 provider
  "tasks": { } }              // model.tasks:每类调用要的档位
```

`base_url` 里的密码与查询串默认换成 `***`(`https://u:***@host`);`include_secrets: true` 时原样给出。`quota` 是写进设置的上限;有 `quota` 的 provider 还带 `usage`:此刻所在的日、周、月各自已用的 token(未命中缓存的输入加缓存写入加输出,缓存命中不计)、上限与起始日(按本机时区,周从周一、月从 1 号)。用量记录文件读不出(不是 JSON、数值不对)时 `usage.error` 写明是哪个文件,这个 provider 按用尽算、文件不覆盖,修好或删掉后恢复。`defined_in` 是定义这一项的设置文件,内建的没有。`credential_present` 是布尔:引擎给这个 provider 建 client 时用的凭据来源(`api_key_env` 指的环境变量,再加宿主给的 `CredentialSource`)现在能不能给出非空的 key;只报有没有,任何情况下(含 `include_secrets: true`)答复里都不带 key 或环境变量的值。

`models` 的每一项是模型 id 字符串,或一个对象,两种写法可以混用;读回时原样是写进去的形状:

```jsonc
"models": [
  "deepseek-chat",                                   // 只给 id
  { "id": "deepseek-reasoner",
    "context_window": 128000,                        // 可选:上下文窗口(token)
    "max_output": 8192,                              // 可选:发给它的 max_tokens 上限
    "reasoning_levels": ["low", "high"],             // 可选:只记录、原样报出
    "input_types": ["text"] }                        // 可选:只记录、原样报出
]
```

`models` 非空时,显式指定的模型(`tiers` 里写的、`session.create` / `setModel` 的 `model`)必须是其中之一。对象里的值按**这个 provider 的 client 发出的调用**取:会话用的模型
(含子 Agent)在它的 provider 里有对象条目时——

- `max_output`:这一轮每次调用的 `max_tokens` 取 `model.max_tokens` 与它的较小值;回复在
  `max_tokens` 处被截断后的重试也以它为上限(先于按模型名查的表)。
  压缩、记忆提取、标题这些自带小上限的调用不受影响。
- `context_window`:`turn_complete` 的 `context.window` 报它,不再按模型名推断。压缩阈值仍由
  场景与 `compaction.*` 决定。
- `reasoning_levels`、`input_types`:引擎目前没有按它们做决定的地方,只保存并在
  `getProvider` 里报出。

对象缺 `id`、或某项既不是字符串也不是对象,`setProvider` 以 `INVALID_PARAMS` 拒绝。

没配 `providers` 时引擎自己提供的 `anthropic` provider(缺省客户端,向宿主要密钥时的 provider id 是 `default`),密钥依次取
`ANTHROPIC_AUTH_TOKEN`、`ANTHROPIC_API_KEY`、宿主的 `CredentialSource`。都没有时 daemon 照样启动,
走它的每一次模型调用(主轮、子 Agent、压缩、记忆提取)都不发请求,这一轮以 `no_credentials`
(§9.1)结束。`ANTHROPIC_BASE_URL` 指向不需要密钥的网关时也要给一个任意非空值,与 `providers`
里配的 provider 缺 key 时建不出来一致。宿主的 `CredentialSource` 答空串等于没给:缺省客户端照上面不发请求,
`providers` 里的 provider 与缺 key 一样建不出来(启动失败,运行中重建时进 `routing.router_error`)。

一对 `(项目, 场景)` 合并后的设置里定义了 provider、却解析不通(写错的 `api`、`tiers` 不在 `models` 里、
`model.order` 引用没定义的 id 等),而这一对之前没有能用的路由时,在这一对上 `session.create` 得
`INVALID_PARAMS`,消息里是原因,不会改用内建的 `anthropic`;之前有过能用的路由就继续用它。

```jsonc
// setProvider 的 params
{ "tier": "project",                                   // 可选;"project"(缺省)| "scene" | "global"
  "dry_run": false,                                    // 可选;true = 只试算,不写
  "provider_id": "deepseek",
  "config": { "vendor":"deepseek", "api":"openai_chat", "base_url":"…",
              "api_key_env":"DEEPSEEK_API_KEY",
              "tiers": { "strong":"deepseek-chat" },
              "quota": { "day": 20000000 } },          // 部分补丁,逐键 merge 进 providers 里这个 id 的那一项
  "delete": false }                                    // 可选;true = 删掉这个 provider
```

响应是 `config.reload` 的结果再加 `tier`、`dry_run`、`written_to`(写入的 settings.json),以及 `warnings`(写进 settings.json 的
provider 配置里看得出凭据时才有,如 `base_url` 里的密码;判据同 `config.set` 的 `warnings`)。`routing` 与其余字段
报的是定位参数指的那一对(scene, 项目)写完之后的样子。

`config` 是部分补丁:只提到的键被覆盖,没提到的保持原样;`config.id` 若写了必须等于 `provider_id`,
写了旧名字的键(如 `api_type`、`api_key`)以 `INVALID_PARAMS` 拒绝并说出新名字。`delete: true` 作用于
定义这个 id 的那一层。它与 `config.set` 是同一个写入实现,`config.set` 也能写 `model.order`、`model.tasks.*`。写成的样子按加载器的规则逐键检查,加载器会丢掉的值(比如
`models` 里既不是字符串也不是对象的一项)直接以 `INVALID_PARAMS` 拒绝,`data.rejected` 同 `config.set`,
什么都不写。`tier:"project"` 而 `project_root` 不是目录 → `PROJECT_NOT_FOUND`(不会替它建出目录)。

**设置里没有明文密钥。** provider 只写 `api_key_env`(密钥所在的环境变量名),或由宿主的
`CredentialSource` 提供。`delete: true` 从那一层的两个文件里都删掉这个 provider。

`tier:"project"` 而目标项目未被信任(§3.6)→ `PROJECT_UNTRUSTED`,什么都不写:项目层的
`providers` 在未信任时不生效。要给它配 provider,先 `project.trust`,或者用 `tier:"global"`
配在全局层——全局层是用户自己的,不看项目信任。写完之后 daemon 自己那一对的设置重读,从写的那一层
合出来的 `(scene, 项目)` 绑定作废(全局层是全部,场景层是那个场景的,项目层是那个项目的),已有会话在
下一轮前换上新配置,定位参数那一对的 MCP server 按新配置重连;这一对的 MCP 还没连过时不连,等它第一个会话
(`config.reload` 不同:它是显式的"全部连一遍",没连过的对也连)。

**`dry_run: true`**:做写入之前的全部检查(参数、项目信任、项目层目录是否借用全局 / 场景层),
读出那一层的两个文件、在内存里套上补丁,按会写成的样子与其余各层合并、解析 provider 路由、
建出 client(不发网络请求)。哪个文件都不写、不建目录、不拿锁,也不动正在用的配置。
套上之后路由解析不了或 client 建不出来(比如写错的 `api`、`openai_chat` 缺
`base_url`、`tiers` 里的模型不在 `models` 里、`model.order` 里有没定义的 id)→ `INVALID_PARAMS`,消息里是原因;能用时回的结果与真写一样,
`dry_run` 为 `true`,`written_to` 是会写的路径,`routing.mcp_*` 为空。
所以宿主可以先 `dry_run` 再写,确认这一对 `(项目, 场景)` 的配置能用。检查只覆盖这一对:`tier:"global"` 的改动同样会进入其他项目与场景的合并结果,它们自己那几层是否依赖这处改动,`dry_run` 不检查。真写不因路由解析不了而拒绝:
文件照写,`routing.error` / `routing.router_error` 报原因,已经在用的路由保留。

两个文件在那一层目录的锁下读改写(与 `config.set`、`mcp.addServer`、`project.trust`、权限询问里的"永久允许"
是同一把,跨进程也互斥;宿主自己改那一层的文件时对目录 `flock(LOCK_EX)` 就与引擎互斥),各自写临时文件再改名替换,先写 `settings.local.json`。
检查与写入在同一把锁里,写下去的就是检查过的内容。
引擎等这把锁最多 **10 秒**:宿主(或别的进程)一直握着它,`config.set` / `config.setProvider` / `mcp.addServer` /
`mcp.updateServer` / `project.trust` 以 `SETTINGS_LOCKED (-32026)` 回,消息形如
`the settings directory <dir> is locked by another process; nothing was written`,文件原样,放手后重发即可。
等锁的那一段不随请求取消:客户端在这 10 秒里断开或放弃了请求,锁一到手,写照样可能落盘。权限询问里的"永久允许"拿不到锁时,这一次调用与当前会话照样放行,
只是没有记进文件,turn 里发一条 `notification` 说明。设置文件是
符号链接的,不顺着写也不替换,以 `INVALID_PARAMS` 拒绝,消息里写明是哪个文件。

#### `config.listModels`

用 provider 已配置的凭据与 `base_url` 问它的端点有哪些模型。宿主不必自己拿着 key 去调
`/models`。定位参数 `scene` / `project_root` 同 `config.getProvider`,`provider` 是
那一对 `(项目, 场景)` 合并后 `providers` 里的 id。

```jsonc
// → params
{ "provider": "deepseek", "scene": "code", "project_root": "/Users/x/repo-b" }   // 后两个可选
// ← result
{ "models": [
    { "id": "deepseek-chat" },
    { "id": "claude-sonnet-4-6", "display_name": "Claude Sonnet 4.6" },
    { "id": "vendor/m", "context_window": 131072, "max_output": 8192,
      "input_types": ["text", "image"], "reasoning": true } ],
  "truncated": false }   // true = 端点还有更多页,这里只是前 50 页
```

- `anthropic`(或没写 `api`):`GET …/v1/models`,带 `x-api-key` 与 `anthropic-version`,
  按 `has_more` / `last_id` 翻页取全,最多 50 页;第 50 页之后还有时 `truncated` 为 `true`。
- `openai_chat` / `openai_responses`:`GET …/models`,带 `Authorization: Bearer`;路径与
  这个 provider 发对话请求的端点同级(`base_url` 写到主机、写到 `/v1`、写全端点都一样)。

每项只有 `id` 必有;其余字段端点给了才有,不按模型名猜:`display_name`(Anthropic)、
`context_window`(`context_window` / `max_input_tokens` / `context_length` / `max_model_len`)、
`max_output`(`max_output` / `max_tokens` / `max_output_tokens` / `top_provider.max_completion_tokens`)、
`input_types`(`input_types` / `input_modalities` / `architecture.input_modalities`)、
`reasoning`(布尔 `reasoning`,或 `supported_parameters` 里有没有 `"reasoning"`)。不缓存,每次都问端点。

请求与模型调用走同一条出口(同样的代理设置),整次最多等 30 秒;发起请求的连接断开、或 daemon
停下时放弃。失败:

- `provider` 不在配置里 → `INVALID_PARAMS`。
- 这个 provider 拿不到凭据 → 不发请求,`ENGINE_ERROR`,`data.kind` 为 `no_credentials`。
- 端点回 404 / 405 / 501,或回的不是带 `data` 列表的 JSON,或 `api` 是宿主自己注册的
  协议 → `MODELS_UNSUPPORTED`(§9),`message` 带端点的说明。
- 其余 HTTP 失败与连不上、超时按模型调用的失败分类,`ENGINE_ERROR`,`data.kind` 为 `auth`、
  `rate_limited`、`model_unavailable` 等(§9.1),`message` 带端点的说明(截到 500 字)。
- 一页超过 8 MiB → 不往下读,`ENGINE_ERROR`。

### 6.5 `mcp.*` / `plugin.*` / `import.*` / `commands.*`

`plugin.list` / `plugin.install` / `plugin.uninstall` / `plugin.enable` / `plugin.disable` / `plugin.reload` 与 `commands.list`(不带 `session_id` 时)**只作用于 daemon 自己的默认项目和默认场景**,不接受定位参数;`plugin.functions` / `plugin.call` 用 `session_id` 或 `cwd` 指目标(见各自一节),`plugin.cancel` 只认 `run_id`。
`mcp.status` / `mcp.addServer` / `mcp.updateServer` / `mcp.removeServer` / `mcp.setEnabled` /
`mcp.reconnect` / `mcp.auth.start` / `mcp.auth.logout` / `import.*` 接受 §6.4 的 `scene` /
`project_root`:省略 = 这个 daemon 自己那一对,给路径 = 它服务的另一个项目,`null` → `INVALID_PARAMS`。
`mcp.auth.complete` / `mcp.auth.cancel` 按 `flow_id` 找到登录,不需要定位;`mcp.auth.setIdentity`
作用于这个 daemon 的全部项目。

写 MCP 设置的四个方法(`addServer` / `updateServer` / `removeServer` / `setEnabled`)都收 `tier`:
`global` / `scene` / `project`,缺省 `project`,与 `config.setProvider` 一样——写定位参数那一对的那一层,
走 `config.set` 的同一个写入实现(锁、`SETTINGS_LOCKED`、密钥落 `settings.local.json`、全有或全无)。写完
只对**被写的那一个** server 对账:定位参数那一对当场处理并在答复里报出,这一层之下别的已经连着的对在后台
各自处理;其余 server 的连接不动。定位参数那一对的 MCP 还没连过时,只有 `addServer` 先按它第一个会话那样把
其余 server 连上(它要立刻试连新加的那个);`updateServer` / `removeServer` / `setEnabled` 只写文件、不启动
任何东西,答复的 `servers` 同 `mcp.status` 对没连过的对的答法(`not_started` 等),等这一对第一个会话再连。
`config.set` 不收 `mcp.servers` 下的键(含 `mcp.servers.<名>.enabled`),指向这几个方法。

#### `mcp.status`

```jsonc
// → params(都可选)
{ "scene": "code", "project_root": "/Users/x/repo-b" }
// ← result
{ "loaded": true, "servers": [ ] }
// 另有 "warnings": [ "…" ],仅当有配置警告时才出现,见下
// 元素:{ "name":"…", "transport":"…", "tool_count": 0, "state":"connected",
//         "limited": "kept 200 of 300 tools; …",   // 仅当 server 的工具表被截过
//         "auth": { "state":"authorized", "issuer":"https://…", "scopes":["…"],
//                   "expires_at":"2026-11-08T10:00:00Z" } }
```

`state`:

| `state` | 意思 |
|---|---|
| `connected` | 连上了 |
| `disconnected` | 连上过,连接后来断了(进程退出、管道关闭);下一个请求先重连。断在途中的工具调用报错,不重发 |
| `connecting` | 还在连。会话创建最多等 10 秒,没连完的在后台接着连,连上后发 `mcp_connected`,这个项目的会话在下一轮之前换上它 |
| `auth_required` | 远端 server 要求 OAuth 登录(回了 401),还没登录或令牌已失效;没有工具,`tool_count` 为 0。用 `mcp.auth.start` 登录 |
| `failed` | 没连上(进程起不来、握手失败或超时),或者配置不合法、没有启动(`error` 以 `invalid config:` 开头,`transport` 为 `unknown`);另带 `error` 说明原因 |
| `denied` | MCP 策略(`ARCHITECTURE.md` §6.4)拒绝了它,或者它是项目自己声明的、用了项目的 server 不能用的东西(daemon 环境里的密钥、本机的密钥文件、`"grant": "enterprise"` 的企业身份);`tool_count` 为 0,另带 `error` 说明是哪份策略、哪条规则 |
| `untrusted` | 项目自己声明的 server,项目未被信任(§3.6),没有启动;`tool_count` 为 0,另带 `error`。`project.trust` 之后启动 |
| `disabled` | 被 `enabled: false` 关了(`mcp.setEnabled`),没有启动;`tool_count` 为 0 |
| `not_started` | 这一对的 MCP 还没连过(`loaded: false`),它会在这一对第一个会话时连接 |

`warnings`:不是哪台 server 的状态、但值得知道的配置问题,每条一句。目前只有一种:某个包声明的 MCP server 与 `settings.json` 的 `mcp.servers` 同键,
包的那个顶替设置里的(该键只有一条记录),排除该包的会话两者都拿不到。

`loaded` 说这一对 `(项目, 场景)` 的 MCP 连过没有。连过的,`servers` 是连接的实际状态;没连过的
(`loaded: false`),`servers` 只按设置列出配了什么,**不建绑定、不连接、不启动任何进程**——查未受信项目的
状态不会执行项目里的命令:`untrusted`、`disabled`、`denied`(按当前的 MCP 策略判)、`failed`(配置不合法),
其余是 `not_started`,`auth` 只从已存的登录记录里读。`scene` 未注册 → `SCENE_NOT_FOUND`,`project_root`
不是目录 → `PROJECT_NOT_FOUND`。

`auth` 每个元素都有,是这个 server 的授权状态(`mcp_auth.md` §2):`state` 为
`none` / `required` / `authorizing` / `authorized` / `failed`;按需带 `issuer`、`scopes`
(已授予的;`required` 时是需要的)、`expires_at`(RFC 3339)、`error`(`failed` 时的原因)。
**从不带令牌。** 一个 `connected` 的 server 也可能是 `auth.state: "required"`:它要求更多
scope(403 `insufficient_scope`),现有令牌对别的请求仍然有效。

#### `mcp.addServer`

```jsonc
// → params
{ "name": "fs",
  "tier": "project",                // 可选;"project"(缺省)| "scene" | "global"
  "config": { "type": "stdio", "command": "…", "args": [], "env": {} } }
```

`config` 是一个 `McpServerConfig`,按 `type` 标签联合:`stdio` / `streamable_http` /
`sse` / `web_socket`(也认 `websocket`);`streamable_http` / `sse` 可以带 `oauth` 块
(`mcp_auth.md` §1),带 `oauth_provider` 的配置被拒绝(`INVALID_PARAMS`,message 给出 `oauth`
的写法)。每种都可以带 `startup_timeout_ms`(起进程加握手,以及工具调用以外的每个请求;默认取
环境变量 `MCP_TIMEOUT`,再缺省 30000)与 `tool_timeout_ms`(一次工具调用;默认取
`MCP_TOOL_TIMEOUT`,再缺省 600000),超时的请求在 server 端被取消。工具表的上限也在这里:
`max_tools`(缺省 200,多出的丢弃)、`max_description_chars`(缺省 4096,截断)、
`max_schema_bytes`(缺省 65536,超出的 schema 换成空对象 schema);截过就在 `mcp.status`
对应条目的 `limited` 里写明截了什么。写进 `tier` 那一层的 settings.json 并**立刻尝试连接**。
写进项目层的是项目层的 server:以项目根为工作目录启动,相对的 `command` 与参数里的相对路径按项目根
解析;配置里的 `$VAR` 只从它自己的 `env` 块展开,不读 daemon 的环境,
`oauth` 里的 `client_secret_env` / `private_key_jwt.key_env` 会让连接被拒(ARCHITECTURE.md
§6.4)——密钥要么写成字面值,要么把 server 声明在全局层。写进全局层 / 场景层的与手写在那两层一样:
不看项目信任、不收窄环境,在那一层的目录里启动。成功与否都会发一条
`mcp_connected` / `mcp_connect_failed` 的 `daemon.event`;要求登录的 server 发
`mcp_auth_required`,在响应的 `servers` 里是 `state: "auth_required"`。

**密钥不进 settings.json。** `config` 里的 `headers` 与 `env` 写进同目录的
`settings.local.json`(按约定不提交,unix 上以 0600 创建),其余字段写 settings.json;两份合并
之后才是这个 server 的配置,`$VAR` 照样从 `env` 块展开。settings.json 里这个 server 已有的
`headers` / `env`(早先版本写进去的)在这次写入时一并替换掉。

响应:`{ "written_to": "<路径>", "secrets_written_to": "<路径>", "servers": [ <同 mcp.status 的元素> ], "warnings": [ "…" ] }`,
`secrets_written_to` 只在改了 settings.local.json 时才有;`warnings` 只在写进 settings.json 的那部分配置里
看得出凭据时才有(`url` 里的密码或 token、`args` 里 `--token` 之后的值,判据同 `config.set` 的 `warnings`),
那部分不会被挪进 settings.local.json——改成 `headers` / `env`。
配置解析不了、名字为空、已有的 settings.json / settings.local.json 不是 JSON 对象 → `INVALID_PARAMS`。
MCP 策略拒绝这个 server → `MCP_SERVER_DENIED`,**什么都不写、不连接、不发通知**,`message`
写明是哪份策略、哪条规则。`tier:"project"` 而目标项目未被信任(§3.6)→ `PROJECT_UNTRUSTED`,同样什么都不做:
写进去的 server 也不会启动;`project_root` 不是目录 → `PROJECT_NOT_FOUND`。

同名的服务已经存在时是替换,不是再加一份——settings.local.json 里它原有的那部分也一起换掉,
新配置没有 `headers` / `env` 就删掉。这个项目上正在跑的会话在下一轮之前按新连接重建,进行中的
调用不受影响。被 `mcp.setEnabled` 关掉的 server 替换配置之后仍是关着的。

#### `mcp.updateServer`

参数、响应与 `mcp.addServer` 相同。区别只在于服务必须已经在那一层的 settings.json(或同目录的
settings.local.json)里声明过,否则 `INVALID_PARAMS` —— 用来换令牌、换请求头这类"改一个已有的",不会因为名字拼错而
悄悄多出一个服务。连接立刻重建,这个项目上的会话下一轮拿到新连接。

#### `mcp.removeServer`

```jsonc
// → params
{ "name": "fs", "tier": "global" }    // tier 可选,缺省 "project"
// ← result:同 mcp.addServer
{ "written_to": "…", "servers": [ ] }
```

从那一层的两个文件里删掉这个 server(settings.local.json 里它的 `headers` / `env` 一起删),同一层
它条目里的 `enabled` 也随之删掉。删完断开它;别的层仍声明它(删掉项目层的覆盖,全局那份还在)时按剩下
的配置重连,答复的 `servers` 里如实可见。那一层没有声明它 → `INVALID_PARAMS`,消息带那份 settings.json 的
路径;它是插件包声明的,消息指向 `plugin.disable` / `mcp.setEnabled`。未受信项目可以删自己层里的 server。

#### `mcp.setEnabled`

```jsonc
// → params
{ "name": "github", "tier": "project", "enabled": false }   // tier 可选,缺省 "project"
// ← result:同 mcp.addServer
{ "written_to": "…", "servers": [ ] }
```

在那一层写 `mcp.servers.<name>.enabled`(`settings.json` 里的 `{ "mcp": { "servers": { "github": { "enabled": false } } } }`,没写的缺省
开着;只有 `enabled` 的条目不声明 server,只是对别处声明的那个的开关)。开关按名字跨层合并:全局关、项目开 → 这个项目开着;对任何一层、任何插件声明的 server 都有效,
不改变它是谁的 server。关掉的不连接、不发 `mcp_connect_failed`,`mcp.status` 里是 `disabled`。
未受信项目只能关不能开:`enabled: true` 写进未受信项目层 → `PROJECT_UNTRUSTED`,什么都不写。
名字既不在这一对的配置里(含插件)、也不在那一层的文件里 → `INVALID_PARAMS`。

#### `mcp.reconnect`

```jsonc
// → params
{ "name": "fs", "scene": "code", "project_root": "/repo" }   // 后两个可选
// ← result
{ "server": { "name": "fs", "transport": "stdio", "tool_count": 3, "state": "connected",
              "auth": { "state": "none" } } }
```

不写文件。按这一对**当前的设置**重新判这个 server 并重连(手改了文件、监视器还没看到也照样吃到),只碰它
一个:别的 server 的连接与进程原样不动。发一条 `mcp_connected` / `mcp_connect_failed`;这一对的会话在下一轮
之前换上新连接,进行中的调用继续用旧的。等待登录的 server 没登录还是 `auth_required`。

- 这一对的 MCP 还没连过 → `INVALID_PARAMS`(它们会在这一对第一个会话时连接);不会为一个 server 单独建出
  "部分连接"。
- 关着的 → `INVALID_PARAMS`(指向 `mcp.setEnabled`);策略拒绝 → `MCP_SERVER_DENIED`;未受信项目自己的
  server → `PROJECT_UNTRUSTED`;配置里没有这个名字或配置不合法 → `INVALID_PARAMS`。

#### `mcp.auth.start`

发起对一个 MCP server 的 OAuth 登录(`mcp_auth.md` §3)。

```jsonc
// → params:两种 redirect 各一例
{ "server": "docs",
  "redirect": "loopback",          // 可省,默认 loopback;或 "external"
  "scopes": [ "files:read" ] }     // 可省;省略按 挑战 > 配置的 oauth.scopes > 资源元数据 取
{ "server": "docs",
  "redirect": "external",
  "redirect_uri": "https://…" }    // 只给 external,且必须给:https、回环上的 http、带 "." 的私有 scheme
```

```jsonc
// ← result
{ "flow_id": "…", "authorize_url": "https://as.example/authorize?…",
  "redirect_uri": "http://127.0.0.1:53817/callback", "expires_in": 600 }
```

把用户送去 `authorize_url`。`loopback` 时 daemon 在 `redirect_uri` 上等浏览器回来,自己完成
登录并发 `mcp_auth_completed`、`mcp_connected`(失败发 `mcp_auth_failed`);`external` 时宿主
接住回调,调 `mcp.auth.complete`。`expires_in` 秒内没完成,登录过期(`mcp_auth_failed`)。
同一个 server 再发起一次会取消前一次(并先释放它占着的回环端口,固定 `callback_port` 也能重来);
不同 server 的登录可以同时进行。

没有这个 server、它不用 OAuth(stdio、websocket、配了固定 `Authorization` 头的)、它是不需要
登录的 `client_credentials` / `enterprise` server、redirect 不合法 → `INVALID_PARAMS`;MCP 策略
拒绝它,或它的授权服务器不在策略的 `allowed_authorization_servers` 里 → `MCP_SERVER_DENIED`
(这时用户还没被送去任何地方);发现失败、授权服务器不支持 PKCE S256、它或它的端点不是 `https`
(回环地址上的除外)、授权端点不是网页(`file:`、`javascript:`、别的应用协议)、无法注册客户端 →
`MCP_AUTH_FAILED`,`authorize_url` 不会交出来。

#### `mcp.auth.complete`

```jsonc
// → params
{ "flow_id": "…", "callback_url": "https://app.example/oauth/done?code=…&state=…&iss=…" }
```

```jsonc
// ← result
{ "server": "docs", "scopes": [ "files:read" ] }
```

校验 `state`(以及授权服务器给了的 `iss`),用 PKCE 校验码和 `resource` 换令牌,存下,只重连
这一个 server,随后发 `mcp_connected`。`callback_url` 不是回到这次登录的 `redirect_uri` →
`INVALID_PARAMS`;`state` 不符 → `MCP_AUTH_FAILED`,**登录继续等**真正的回调;授权服务器在回调里
报错或换令牌失败 → `MCP_AUTH_FAILED`,登录结束,状态 `failed`。`flow_id` 不存在(已完成、已
取消、已过期)→ `INVALID_PARAMS`。

#### `mcp.auth.cancel`

`{ "flow_id": "…" }` → `{ "server": "docs" }`。放弃这次登录,server 的 `auth.state` 回到发起之前
(还没登录过就是 `required`)。`flow_id` 不存在 → `INVALID_PARAMS`。

#### `mcp.auth.setIdentity`

```jsonc
// → params:二选一
{ "id_token": "eyJ…" }          // 企业 IdP 签给这个客户端的 OIDC ID token
{ "refresh_token": "…" }        // 或企业 IdP 的刷新令牌
```

```jsonc
// ← result
{ "reconnected": [ "docs" ] }
```

交给 daemon 用户的企业身份(`mcp_auth.md` §4),只放在内存里。所有 `"grant": "enterprise"`
且还在等身份的 server 随即用它换令牌并重连(`reconnected` 是重连了的 server,结果看
`mcp_connected` / `mcp_connect_failed` 与 `mcp.status`);之后令牌到期时也用它再换。企业 IdP
拒绝这个身份时 server 留在 `auth_required`,`auth.error` 写明要换一个新的。两个都给或都不给、
没有配置企业 IdP → `INVALID_PARAMS`。

#### `mcp.auth.logout`

`{ "server": "docs" }` → `{ "server": "docs" }`。授权服务器提供吊销端点时吊销令牌
(RFC 7009),删除令牌(保留客户端注册),断开 server;它回到 `state: "auth_required"`、
`auth.state: "required"`。不发通知。

#### `plugin.list`

无参。

```jsonc
// ← result
{ "plugins": [ ] }
// 元素:{ "name":"…", "version":"…", "description":"…",
//        "source": "installed|host",          // host = 嵌入方自带的包
//        "enabled": true,
//        "disabled_reason": "…",             // 包不会加载的原因:不在 settings.plugins.allow 里、用了宿主没提供的库、宿主带了同名包
//        "shadowed_by": "host",              // 只在宿主带了同名包、这个装的包让位时有
//        "root": "/…/plugins/cache/<name>/<version>",
//        "script_faults": [],
//        "skills": [ "tour" ],
//        "skill_conflicts": [ ],
//        "commands": [ ],
//        "command_conflicts": [ ],
//        "functions": [ ] }
// skill_conflicts 元素:
//   { "skill": "tour", "shadowed_by": "user|project|plugin",
//     "path": "/…/skills/tour/SKILL.md",
//     "plugin": "…" }                       // 只在 shadowed_by 为 plugin 时有
// commands 元素:
//   { "name": "forge", "description": "…", "read_only": ["help", "spec list"], "writes": ["forge/**"] }
// functions 元素(调用名是 <包名>.<name>):
//   { "name": "check", "description": "…", "writes": ["reports/**"], "has_input_schema": true }
// command_conflicts 元素:
//   { "command": "forge", "shadowed_by": "plugin|skill",
//     "plugin": "…",                        // 只在 shadowed_by 为 plugin 时有
//     "path": "/…/skills/forge/SKILL.md" }  // 只在 shadowed_by 为 skill 时有
```

列出磁盘上的全部插件及启用状态,包括已停用的。`settings.plugins.allow` 非空时,不在名单里的包照常列出,
但 `enabled` 恒为 `false` 并带 `disabled_reason`——它不会被加载,`plugin.enable` 也改变不了这一点。

`root` 是解包后的目录。有它,宿主要读包里自己关心的东西(界面产物、图标)就不必自己
拼 `<plugins-dir>/cache/{name}/{version}/`——那样会把 daemon 的磁盘布局变成它 API 的
一部分。

`script_faults` 是这个包的 `[[script]]` 绑定里没能兑现的那些,每条带原因。来自包的绑定
是逐条降级的,所以这里非空不代表包没装上,只代表少了那几条贡献。

`skills` 是包的 `[skills]` 段带来的 skill 名。包里的 skill 用裸名、优先级最低(只压过内建):
磁盘上的同名 skill、或按包名排在前面的另一个包的同名 skill 占着这个名字时,它被遮住,
`skill_conflicts` 各记一条——`shadowed_by` 是占着名字的那份的来源,`path` 是它的
`SKILL.md`,另一个包时 `plugin` 给出包名。按这个 daemon 自己的项目和场景算;停用的包没有冲突。

`commands` 是包的 `[[command]]`(`runext.md`):命令名、描述、只读前缀与能写的范围。两个
启用着的包声明同名命令时按包名先到的保留,后到的不注册,它的 `command_conflicts` 记一条
`shadowed_by: "plugin"`、`plugin` 是占着名字的包;一个 skill 与命令同名时 `/名字` 归 skill
(`RunExt` 照样能跑),记 `shadowed_by: "skill"`、`path` 是那个 `SKILL.md`。

`functions` 是包的 `[[function]]`(`runext.md` §12),只给宿主调,不进模型的工具表。

`source: "host"` 的是嵌入方自带的包(`plugin_packages.md`「宿主自带的包」),协议登记不了;同名的装的
包让位,`shadowed_by: "host"`、`enabled: false`。

#### `plugin.install`

```jsonc
// → params
{ "name": "code-review-helper",
  "version": "1.0.0",
  "download_url": "file:///abs/path/plugin.zip",   // 或 https://…
  "checksum": "<sha256 hex>",
  "scope": "global" }                              // 可选,global(默认) | scene
```

**daemon 自己去取,没有上传通道。** 没有 marketplace 查询,直接从给的源装。

`name` 与 `version` 各是 cache 里的一级目录名:1–128 个 ASCII 字母、数字、`.`、`_`、
`-`、`+`,不以 `.` 开头;别的(`/`、`..`、绝对路径、非 ASCII)一律 `INVALID_PARAMS`,
什么都不读不删。`plugin.uninstall` 同此。包里 manifest 的 `[plugin] name` 必须等于
`name`,否则按读不回来处理。

**`checksum` 对 `http(s)://` 源是必填的**,缺了会在发起请求之前就拒绝;`file://`
源可以省略。流程:取包(最大 128 MiB)→ 校验 → 解包到临时目录(有总量、条目数、压缩比
上限)→ 在临时目录里按发现时的方式把 manifest 读回来、检查披露 → 改名就位。读不回来、
或披露里有超限的文本,这次安装就在改名之前被拒,报 `INVALID_PARAMS`,cache 里什么都不会
出现。带 `[[wasm]]` 或 `[scene.own]` 段的包在这一步被拒——引擎不跑 WebAssembly 组件,
场景改用 `[scene] dir`。

压缩包的 SHA-256 记在版本目录的完成标记里。同一版本已经装过时,若这次给的 `checksum`
与记下的不一致,cache 里的那份就不算要装的这个包,会被取来的新包替换;一致(或这次没给
`checksum`、或标记是旧版本留下的空标记)就按"已装过"处理,同样读回、同样披露,读不回来就
报错,但不删它。

响应:`{ "success": true, "message": "…", "enabled": false, "disclosure": … }`。
`enabled` 是装完之后这个包按启用状态解析出的结果:`plugins.enable_on_install` 缺省 `false`,
新装的包(这一层没有它的启用记录)装完保持禁用,宿主读过披露后要显式调 `plugin.enable`,
包才会往会话里贡献东西;设为 `true` 时装完即启用。已经有启用记录的名字(包括升级同名包)
保持原状。`disclosure` 是这个插件会往模型上下文里塞的文本(含它带的 skill 的
描述与正文)、它带的 skill 名(`skills`)与命令名(`commands`)和它要起的 MCP 服务器(命令行或地址),让调用方
在依赖它之前先看见;字段见 `plugin_packages.md`。包里有一个 `SKILL.md` 校验不过,同样在
改名之前被拒,报 `INVALID_PARAMS`,消息里写明是哪个 skill、哪一项;`[[command]]` 或 `[[function]]`
的入口不是源码(QuickJS 字节码)、`[[function]]` 的 `input_schema` 不是合法的 JSON Schema、`uses`
里有宿主没提供的库(``needs host library `codix`, which this host does not provide``)同样拒装。
`disclosure.functions` 是包的函数调用名。

#### `plugin.uninstall`

```jsonc
// → params
{ "name": "…", "version": "1.0.0", "scope": "global" }   // version 省略 = 全部版本
```

响应 `{ "success": true, "message": "…" }`。宿主自带的包报 `INVALID_PARAMS`
(``` `<name>` is the host's own package; it cannot be uninstalled through the protocol ```),
`plugin.enable` / `plugin.disable` 同样;这一层装着被宿主包遮住的同名包(`shadowed_by:"host"`)时,
`plugin.uninstall` 删的是装的那个。

#### `plugin.enable` / `plugin.disable`

```jsonc
// → params
{ "name": "…", "scope": "global" }
```

响应 `{ "name": "…", "enabled": true, "scope": "global" }`。启用状态按层记录,
不改动已安装的文件。那一层的 `enabled.json` 读不了或解析不了时报错、不覆盖它——这时这一层
的包全部按禁用算,见 `plugin_packages.md`。

#### `plugin.reload`

无参。重扫插件目录并重新加载,响应与 `plugin.list` 同形。

上面四个方法各自会自动刷新,**这个是给它们盖不到的情况用的**:有人手工把插件放进
目录、或者开发时原地改了包的内容。

#### `plugin.functions`

```jsonc
// → params(可选,至多给一个)
{ "session_id": "…" }   // 或 { "cwd": "/abs/project" };给了的必须存在
// ← result
{ "functions": [
  { "name": "xflow.check",              // 调用名 <包名>.<name>
    "plugin": "xflow", "plugin_version": "1.0.0",
    "description": "门禁检查",
    "writes": ["reports/**"],
    "input_schema": { … } }             // 包里的 JSON Schema;没有为 null
] }
```

启用着的包的全部 `[[function]]`。给 `cwd` 时函数表是池级的:只检查目录在不在(`PROJECT_NOT_FOUND`)。给
`session_id` 时会话不在是 `SESSION_NOT_FOUND`,在则只列这个会话用得到的包的函数(会话层的 `plugins.allow` /
`plugins.enabled` 收窄过的,见 §6.3 `options.settings.plugins`);会话不在内存里时按日志里记的规格算。

#### `plugin.call`

```jsonc
// → params
{ "function": "xflow.check",             // 只接受调用名,不接受命令行
  "input": { "change": "c1" },           // 交给函数的 input,省略为 null;有 input_schema 时先校验
  "session_id": "…",                     // 在这个会话的环境里跑;不带会话时换成 "cwd": "/abs/project"(绝对路径),两者只给一个
  "writes": false,                       // 缺省 false
  "permission_rules": [ ],               // 写法见 §4.6
  "timeout_ms": 30000,                   // 缺省 120000,至多 600000(能力位 `plugin_call_long`)
  "run_id": "gate-7" }                   // 可选:plugin.cancel 用它指这次调用,同一连接内唯一
// ← result
{ "ok": true,  "value": <函数的返回值>, "logs": ["…"] }
{ "ok": false, "error": { "code": "…", "message": "…" }, "logs": ["…"] }
```

`session_id` 指定的会话不用函数所属的包(被 `options.settings.plugins` 排除)时,`plugin.call` 以 `INVALID_PARAMS` 失败
(`session … does not draw on plugin …`)。

宿主调用作为一次内部工具调用 `PluginCall {function, input}` 走会话的整条工具管线——权限判定、
`PreToolUse`、`tool.around`(看到的 `via` 是 `"host"`)、执行、`PostToolUse`、遥测——但不进
模型的工具表,也不进对话。函数在 `RunExt` 的运行时上跑,签名 `async function check(input, ctx)`,
`ctx` 是冻结的 `{ via:"host", projectRoot, cwd, plugin:{name,version}, function, session, readOnly }`
(`session` 是会话 id,不带会话时为 `null`)。

**权限**:调用本身算宿主批准——会话要问的视同允许(部署的托管层规定只能本人答的除外,宿主调用不答复它);deny 规则、plan 模式照样拦(这时
`code:"refused"`)。规则写 `PluginCall(xflow.check)`。函数里的子调用与插件命令的子调用同一套
(`tool.around` 看到 `via:"PluginCall:xflow.check"`):`writes:false` 时经文件工具的写入、会改东西
的命令一律失败;`writes:true` 时经文件工具只写得了函数声明的 `writes`(命令的写入按规则走,不受 `writes` 限制),配置路径照样不让写;**要问人的一律
失败**,错误说明是哪次调用、为什么(没人能替宿主调的函数答应,用这次调用的 `permission_rules`
放行)。`permission_rules` 追加在会话或场景的规则之后,只管这次调用和它的子调用:deny 任何模式
都拒,allow 只回答模式本来要问的。

**带 `session_id`**:用那个会话的工具表、MCP 连接、规则、工作目录与信任;不在内存里的会话先
带回来。不开新一轮、不进对话、不排进输入队列,可以和正在跑的一轮同时进行。调用登记为会话的一件
`kind:"ext"` 任务(`detail`:`{description, function, via:"host", background:false, tool_use_id}`),
`session.tasks` 看得到,`session.stopTask` 让它停下(函数可以借 `stopRequested()` 收尾)。
会话正在重建或回滚时不起调用,报 `SESSION_BUSY`(`reason` 为 `rebuilding` / `rewinding`)。

**不带 `session_id`**:在 `cwd` 下用 daemon 的场景、这个目录的工具与规则跑,引擎是临时的:
不写日志、不进 `session.list`、不连 MCP、不向订阅者发会话事件,调用结束(成功、失败或取消)就收掉。
例外是连接用 `daemon.setHooks` 登记的 hook:临时引擎照样问它们(否则任何连接都能借不带会话的
调用绕过别的宿主的 `PreToolUse` 闸门),`hook_call` 帧发给登记者,外层 `session_id` 是这次调用的
临时会话 id,帧带 `plugin_call:{function, run_id?}`(`run_id` 是这次调用的,没给就省略;§7.6),
登记者用这个 `session_id` 回 `session.respondHook`。登记者已不在、连接在调用中途断开、帧发不出去时,
当场按 hook 的 `on_error` 了结(`allow` 继续,`block` 拦下,结果是 `refused`),不等 `timeout_ms`。

与 `RunCode` / `RunExt` 共用同时运行脚本的名额;不带会话的调用先等到名额再建临时引擎,同时在的临时引擎
因此不超过名额数。名额是整个 daemon 的(各会话共用,本机 CPU 数的两倍),一次调用从开始到真正结束
一直占着一个——`timeout` 之后还在后台收尾的那段也算;带 `session_id` 时这整段还占着会话:它不算空闲,
不被重建、回滚或驱逐。`timeout_ms` 可到 600000,几个长调用同时在跑就能把名额占满,这时模型的
`RunCode` / `RunExt` 只能等名额,等到它们结束或那一轮被打断。宿主自己限制同时在跑的长调用数。`code` 取值:

| `code` | 含义 |
|---|---|
| `invalid_input` | `input` 不符合函数的 `input_schema`,函数没跑 |
| `refused` | 管线没让它跑,或拦下了它的结果:deny 规则、plan 模式、hook、`tool.around`、会话已结束 |
| `answered_by_middleware` | 交回来的不是函数自己的结果:`tool.around` 用 `respond` 替它作答,或 `tool.result` 改写了它的结果。函数可能已经跑过,但结果一律不作数 |
| `threw` | 函数抛了错;`message` 是错误消息,后面跟它已经做了的改动 |
| `timeout` | 跑过了 `timeout_ms`。到点后让它停下,至多再等 2 秒收尾;还没收完(比如 hook 在跑)就先答 `timeout`,剩下的在后台跑完,带会话时会话一直算忙到它真正结束 |
| `cancelled` | `plugin.cancel`、连接断开,或会话结束;还在等名额时被取消也是它 |
| `stopped` | 被 `session.stopTask` 停下又没在宽限期内收尾 |
| `not_started` | 入口读不出来、名额等不到等,函数没开始 |

请求本身不成立时是 JSON-RPC 错误而不是 `ok:false`:没有这个函数、`session_id` 与 `cwd` 都给或都
不给、`cwd` 不是绝对路径、`timeout_ms` 为 0 或超过上限、`run_id` 在这条连接上正被占着
(`INVALID_PARAMS`);会话不在(`SESSION_NOT_FOUND`);目录不在(`PROJECT_NOT_FOUND`);
构建里没有 `RunCode` 的运行时(`METHOD_NOT_FOUND`,`data.module` 为 `"ptc"`)。

#### `plugin.cancel`

```jsonc
// → params
{ "run_id": "gate-7" }
// ← result
{ "cancelled": true }   // false:这条连接上没有以此为名、还在跑的调用
```

`run_id` 按连接登记:别的连接取消不了它,调用结束后名字就释放。发起调用的连接断开时,
它还没完的调用一律取消。

**关于 `scope`**:只有 `global`(全局根下的 `plugins/`,跨场景共享)和 `scene`
(场景根下的,只对这个 daemon 的场景生效)两个值,默认 `global`,别的值返回
`INVALID_PARAMS`。

**什么时候生效**:装/卸/启用/停用/重扫**只影响之后新建的会话**。插件贡献的工具是在
会话创建时注入工具表的,已经在跑的会话保持原样。受影响的场景与斜杠命令会立即刷新,
所以 `scene.list` / `commands.list` 在调用返回之后就是新的。包带的 skill 例外:技能目录是
就地换掉的,已经在跑的会话在下一轮开始时就看到包的 skill 出现或消失(发 `skills_changed`)。

> 插件子系统可以在编译期整个裁掉(daemon 的 `plugin-packages` feature,默认开启)。
> 裁掉或被策略关掉的构建里,上面的方法(`plugin.cancel` 除外)一律返回 `PLUGINS_DISABLED (-32016)` ——
> 不是"未知方法",因为方法是存在的,只是这个构建不提供它。用 `daemon.doctor` 的
> `plugins.status` 判断当前是哪种情况。

#### `commands.list`

```jsonc
// → params(可选)
{ "session_id": "…" }   // 省略 = daemon 自己的那一组
// ← result
{ "commands": [ ] }
// 元素:{ "name":"…", "description":"…",
//        "kind":"prompt|local|mcp-prompt|ext",   prompt = 展开后继续问模型,local = 直接执行,mcp-prompt = MCP server 的 prompt,ext = 插件命令(经 RunExt 跑,不问模型,source 为 plugin)
//        "source":"builtin|user|project|plugin|host|mcp",
//        "tier":"global|scene",              仅 source 为 user 且来自磁盘上的用户层目录时有:global = {global}/skills,scene = 场景层的 skills
//        "dir":"/abs/…" }                    文件来源的 skill 所在目录的绝对路径;builtin、内建 skill、mcp 没有
```

给了 `session_id`,返回这个会话自己的命令表(它的 skill 视图,含宿主目录里的 skill;场景的 `bundled_skills`
没列的内建也不在其中;MCP 的 prompt 建会话时登记,之后的走实时的 MCP 回退)。只有内存里的会话:
否则 `SESSION_NOT_FOUND`,消息指向先 `session.resume`。

`kind:"ext"` 的命令发 `/名字 参数` 给 `session.run_turn`:参数就是命令行的其余部分,用户敲下即算
批准(本来要问的不再问,deny 与 plan 模式照样拒),输出以 `text_delta` 推出,这一轮的
`stop_reason` 是 `command_executed`,不调模型、不进对话。同名 skill 存在时 `/名字` 是 skill。

#### `import.list`

探测某个项目里可导入的外部配置来源。

```jsonc
// → params(可选)
{ "project_root": "/Users/x/repo-b" }   // 省略 = 这个 daemon 自己的项目
// ← result
{ "sources": [ ] }   // 元素:{ "source": "claude_code|codex|cursor", "description": "…" }
```

#### `import.run`

```jsonc
// → params
{ "source": "claude_code",
  "project_root": "/Users/x/repo-b" }   // 可选,同 import.list
```

响应 `{ "source": "claude_code", "actions": [ … ] }`。`source` 必须是**同一个项目**
`import.list` **当前**还探测得到的那些之一,否则 `INVALID_PARAMS`。没有 `dry_run`。

导入读写的是那个项目自己的文件(`.claude/` / `.codex/` / `.cursor/`),所以
`project_root` 与 `config.*` 一样不接受 `null`。

导入完成后会记下这个决定,启动时的自动探测不再重复提示。

### 6.6 还没有的东西

`agent.*` 和 `team.*` 在早期设计里定过接口,**没有实现**,调用返回
`METHOD_NOT_FOUND`。起子 Agent 走 `session.startAgent`;列举、读输出、发消息给、停止运行中的子 Agent、团队成员走 `session.tasks` /
`session.taskOutput` / `session.messageTask` / `session.stopTask`(§6.3);要看一个子 Agent 干了什么,走它的侧链会话:`session.list
{parent_session_id}` 拿 id,再 `session.history`。

### 6.7 `schedule.*`

定时任务:到点时把一段 `prompt` 作为某个会话的一轮发给它。由 daemon 自己的调度器
每分钟检查一次 —— 没有连接也照样跑;会话不在内存里就先恢复。它走的就是
`run_turn` 那条路:这个会话正在跑时**排队**,开始和结束发 `turn_started` /
`turn_completed`(`initiator:"schedule"`,§8),帧推给这个会话的订阅者。daemon 没在跑
的那段时间里错过的触发**不补**。

模型自己也能经 `CronCreate` / `CronList` / `CronDelete` 工具管理它所在会话的任务,和
这里是同一份;模型建的周期任务 7 天后最后触发一次就删除,这里建的不会过期。模型建的另受设置约束:
一个会话里它建的、还在的任务不超过 `schedule.max_model_jobs`(缺省 20),周期任务两次触发至少隔
`schedule.min_model_interval_minutes` 分钟(缺省 1,即不额外限制);超了 `CronCreate` 返回错误、写明
是哪个设置。子 Agent 建不了定时任务。这里建的不计入、也不受这两项约束。任务带 `by_model: true` 表示
是模型建的(宿主建的没有这个键)。

#### `schedule.create`

```jsonc
// → params
{ "session_id": "…",        // 必填,必须是这个 daemon 已有的会话
  "cron": "0 9 * * 1-5",    // 必填,5 段:分 时 日 月 周,按 UTC
  "prompt": "…",            // 必填
  "recurring": true,        // 默认 true;false = 下一次匹配时跑一次然后删除
  "durable": true }         // 默认 true;写进磁盘,daemon 重启后还在
// ← result
{ "job": { "id": "…", "cron": "…", "prompt": "…", "recurring": true, "durable": true,
           "session_id": "…", "created_ms": 0 } }
```

job 里没写在例子中的字段只在有值时出现:`last_fired_ms`(上次触发的 Unix 毫秒)、`expires_ms`(此后的下一次触发是最后一次;模型建的周期任务才有)、`agent_id`(团队成员建的)、`by_model`。

`cron` 不合法 → `INVALID_PARAMS`;会话不存在 → `SESSION_NOT_FOUND`。

#### `schedule.list`

```jsonc
// → params(可选)
{ "session_id": "…" }        // 只列这个会话的;不给就列全部
// ← result
{ "jobs": [ { "id": "…", "cron": "…", "prompt": "…" } ] }   // 元素同 schedule.create 的 job
```

#### `schedule.delete`

```jsonc
// → params
{ "id": "…" }
// ← result
{ "deleted": true }          // 不存在时为 false
```

会话被 `session.delete` 删掉时,它的定时任务一起删除。

### 6.8 `project.*`

项目信任(§3.6)。两个方法都接受 `scene` / `project_root` 定位参数,语义同 `config.reload`:
省略 = 这个 daemon 自己那一对,给路径 = 它服务的另一个项目,`null` → `INVALID_PARAMS`。
`scene` 只影响 `requires_trust` 的判断(比如项目的 `permission_mode` 比哪一层的更宽),信任本身
与场景无关。

#### `project.status`

```jsonc
// → params(都可选)
{ "project_root": "/Users/x/repo-b", "scene": "code" }
// ← result
{ "project_root": "…/work",
  "trust": "untrusted",        // trusted | untrusted | none
  "matched": null,             // 命中的 app.trusted_projects 条目;null = 由 default 决定
  "trust_source": "default",   // default | user | host | null,§3.6
  "default": "untrusted",      // app.trust_all_projects 的结果
  "requires_trust": [ ] }      // 见下
```

`requires_trust` 是这个项目里**要信任才生效**的东西,不论现在信不信任都列出来——
`trust` 是 `untrusted` 时它们正被扣下,`trusted` 时它们在生效、一旦不信任就会停:

| 条目 | 意思 |
|---|---|
| `hooks`、`scripts` | 项目设置里的 hook、脚本绑定 |
| `mcp.servers.<名字>` | 项目设置里声明或改写的 MCP server |
| `permissions.mode`、`permissions.client_may_loosen`、`skills.allow_shell` | 比下面几层更松的值 |
| `permissions.allow(<规则>)`、`permissions.allow_exact(<规则>)` | 项目设置里的一条 allow 规则 |
| `sandbox.<键>`(含 `sandbox.cache_dirs.<组>`、`sandbox.env.allow`)、`files.<键>`、`web.allowed_domains` | 放宽沙箱与读写范围的值 |
| `providers.<id>` | 项目设置里的一个 provider(新加的,或改写已有的) |
| `model.order` | 排了项目自己的 provider(下面几层没有的) |
| `sandbox.network.proxy`、`telemetry.otlp_endpoint`、`recorder`、`prompt.instruction_files` | 沙箱命令能连的代理、遥测送到哪里、录像写到哪里、读进提示词的文件 |
| `plugins.enabled`、`plugins.allow` | 比下面几层放进更多插件的值 |
| `agents/<名字>` | 替换同名类型、或要求比会话更松的模式 / 更高的调用上限的项目 agent 类型 |
| `skills/<名字>` | 正文里有 `` !`cmd` `` / ` ```! ` 的,或 `allowed-tools` 不空的项目 skill |

#### `project.trust`

```jsonc
// → params
{ "project_root": "/Users/x/repo-b",   // 可选,省略 = daemon 自己的项目
  "trusted": true }                     // 必填
// ← result
{ "project_root": "…/work", "trust": "trusted", "matched": "…/work",
  "trust_source": "host", "default": "untrusted", "requires_trust": [ ],
  "written_to": "…/settings.local.json" }
```

`trusted: true` 把这个项目根加进全局层 `settings.local.json`(0600)的
`app.trusted_projects`;`false` 把指向同一目录的条目删掉。记下的是**解析之后的真实目录**
(`..`、符号链接都展开),其中的 glob 元字符(`*`、`?`、`[`、`]`、`{`、`}`)用 `[…]` 转义——
`/x/Project [v2]` 记成 `/x/Project [[]v2[]]`,只命中它自己。结果是写完之后的
`project.status`,外加 `written_to`。**`false` 只删这一条**:另一条 glob 或
`app.trust_all_projects: true` 仍然信任它时,结果里照样是 `trusted`(`matched` 说明是哪一条),
要改就改全局设置。缺 `trusted`、`project_root` 是相对路径、`trusted: true` 而目录不存在 →
`INVALID_PARAMS`;全局层目录 10 秒内拿不到锁 → `SETTINGS_LOCKED`,什么都没写(§6.4)。

写完之后这个项目的会话在下一轮之前按新的信任状态重建,它的 MCP 连接重连,并发一条
`project_trust_changed`(§8)。

---

## 7. 流式帧:`session.event`

推给该会话的全部订阅者(§5.4)。**非终止** —— turn 的终止标志是 `run_turn` 的最终
响应。

```jsonc
{ "jsonrpc":"2.0", "method":"session.event",
  "params": { "session_id":"…", "turn_id":"…", "event": { "kind":"…" } } }
```

| `kind` | 载荷 | 说明 |
|---|---|---|
| `turn_start` | `{}` | 本轮开始,是本轮的第一帧 |
| `step_start` | `{step}` | 本轮要调一次模型;`step` 从 0 计。同一个 `step` 再来一次,说明上一次调用失败、被恢复策略重发(§7 表下):这一步此前推过的 `text_delta` / `thinking_delta` 作废 |
| `model_retry` | `{step,attempt,delay_ms,reason}` | 这一步的调用在流已经开始之后失败(断流、流中途空闲、流里的过载事件),等 `delay_ms` 毫秒后原样重发;随后是同一个 `step` 的 `step_start`。`attempt` 是这次调用的第几次重发,从 1 计;`reason` 是失败的说明文字。等待期间取消,这一轮照常以取消结束 |
| `step_end` | `{step,stop_reason,tool_calls,usage,seq,served?}` | 这次调用结束,它的回复和它要的工具结果都已落盘;`usage` 只是这一次调用的;`seq` 是此刻 transcript 的消息数(§5.6);`served` 是 `{provider,model}`:这次调用实际接的 provider 与向它要的模型,没经路由的调用没有 |
| `text_delta` | `{text}` | 模型输出增量 |
| `thinking_delta` | `{text}` | 模型思考过程的增量,与正文分开,宿主可以折叠或不显示 |
| `tool_use_start` | `{id,name}` | 模型开始写一次工具调用,参数还在流;同 `id` 的 `tool_use` 随后到达。界面可以先画出工具行 |
| `tool_args_delta` | `{id,partial_json}` | 这次调用参数的原始片段,按到达顺序拼起来就是 `tool_use.input` 的 JSON 文本(高频) |
| `tool_use` | `{id,name,input}` | 模型发起工具调用,参数已完整。参数不是完整 JSON 的(多半被输出上限截断)`input` 为 `{}`,不执行,随后的 `tool_result` 是说明这一点的错误 |
| `tool_progress` | `{tool_use_id,name,message,progress?,total?,dropped}` | 运行中的工具调用报的进度,`tool_use_id` 是那次 `tool_use` 的 `id`;每次调用每秒至多一帧,`dropped` 是上一帧以来合并掉的条数。不进 transcript,不保证送达(§7.3) |
| `tool_result` | `{id,name,content,is_error,file_changes?}` | 工具返回。`Write` / `Edit` / `NotebookEdit` 带 `file_changes`(§7.5),其余没有这个键 |
| `prompt` | `permission`:`{prompt_type,prompt_id,tool_name,message,paths[],tool_use_id?,origin?,permit_always,task_id?,agent_session_id?}`;`question`:`{prompt_type,prompt_id,tool_use_id,question,header,options:[{key,label}],task_id?,agent_session_id?}`;`mcp_elicitation`:`{prompt_type,prompt_id,server,mode,message,requested_schema?,url?,url_host?,tool_use_id?,task_id?,agent_session_id?}` | 需宿主回答,用 `session.respondToPrompt`;`tool_use_id` 是被问的那次 `tool_use`(`question` 的 `prompt_id` 就是它)。`permission` 的 `origin` 只在插件命令的子调用上出现:`{via:"RunExt:<命令名>",tool_use_id,command}`,`tool_use_id` 是那次 `RunExt` 的、`command` 是它的命令行,子调用自己的 `tool_use_id` 是 `<RunExt 的>.<n>`(`docs/runext.md` §6);`permit_always` 为 `false` 时不提供"总是允许"(§4.7)。三种 `prompt` 都带可选的 `task_id` 与 `agent_session_id`(没有就不出现):子 Agent 发起的询问,它们是那个子 Agent 的任务 id 与会话 id,与 `session.startAgent` 或 `Agent` 工具返回的值对得上,同一父会话下并发的几个子 Agent 按它们分开;这时 `tool_use_id` 是子会话里那次调用的 id。模型自己的询问不带这两个字段。`question` 的 `header` 可为 `null`,`options` 为空表示自由回答。`mcp_elicitation` 的 `mode` 为 `form`(带 `requested_schema`)或 `url`(带 `url` 与它的主机 `url_host`,只有 `https` 和本机的 `http` 会推到宿主,别的直接回 server `decline`);`message`、`requested_schema`、`url` 都是 **MCP server 写的**,宿主要标明来自 `server`、显示完整 URL 与 `url_host`、不自动打开 —— Agix 自己从不打开它 |
| `prompt_resolved` | `{prompt_id,resolution,reason?}` | 那一帧 `prompt` 不再悬着:`resolution` 为 `answered`(引擎收下了一个回答,不论是哪条连接答的)或 `withdrawn`(引擎不再等)。`withdrawn` 时带 `reason`:`timed_out`(提问超时没人答)或 `cancelled`(这一轮被中断或结束、会话在关闭);`answered` 时没有这个字段。每个 `prompt` 至多一帧,推给全部订阅者,包括答题的那条连接;收到就把这个提问从界面上撤下 |
| `host_tool_call` | `{call_id,tool_use_id,name,input,timeout_ms,task_id,agent_session_id,cwd,depth}` | 模型调用了宿主声明的工具,等宿主用 `session.respondToolCall` 回答(§7.4)。`call_id` 是 daemon 生成的,回答用它;`tool_use_id` 是引出它的那次 `tool_use`;`input` 已按声明的 schema 校验过;`task_id`、`agent_session_id` 在子 Agent 发起时是它的任务与侧链会话,否则为 `null`;`cwd` 是发起调用的会话的工作目录(绝对路径),`depth` 是它的委派深度(根会话 0) |
| `host_tool_call_resolved` | `{call_id,resolution,reason?}` | 那一帧 `host_tool_call` 不再悬着:`answered`(引擎收下了一个回答)或 `withdrawn`,后者带 `reason`:`timed_out`、`cancelled`、`no_host`(发起时没有订阅者)、`host_gone`(订阅者走光了)。每个调用一帧,推给全部订阅者,连接提供的工具再加上提供者(§7.4) |
| `hook_call` | `{call_id,hook_event,input,timeout_ms,task_id,agent_session_id,cwd,depth,plugin_call?}` | 宿主经 `daemon.setHooks` 登记的 hook 触发了,等登记它的连接用 `session.respondHook` 回答(§7.6)。**只发给登记者**;`input` 是这一刻的 hook payload;`task_id`、`agent_session_id` 在子 Agent 发起时是它的任务与侧链会话,否则为 `null`;`cwd`、`depth` 同 `host_tool_call`,与 `input` 里的同名字段一致;`plugin_call` 只在不带会话的 `plugin.call` 发出时有 |
| `hook_call_resolved` | `{call_id,resolution,reason?}` | 那一帧 `hook_call` 不再悬着:`answered` 或 `withdrawn`,后者带 `reason`:`timed_out`、`cancelled`、`host_gone`、`no_host`。只发给登记者 |
| `subagent_progress` | `{agent_label,agent_session_id,agent_type,parent_turn,parent_tool_use_id,task_id,event}` | 子 Agent 事件镜像,`event` 是嵌套的本表结构(同样不带 `turn_id`;子 Agent 的事件本表里没有的,整帧不发);`parent_tool_use_id` 是父会话里发起它的那次 `Agent` 调用,不知道时为 `null`;`task_id` 是它在 `session.tasks` 里的任务 |
| `team_progress` | `{team,team_id,stage,stage_index,stage_count,status,members,failed}` | 团队阶段生命周期 |
| `skills_changed` | `{added,removed}` | 会话可用的 skill 与它上次看到的不同:skill 文件在两轮之间增删后,下一轮开始时发一次(同一份 skill 目录上的每个会话各发各的;宿主目录的改动只到列了它的会话)。缓存了 `commands.list` 的客户端该重取 |
| `provider_switched` | `{from,to,reason,task}` | 这次模型调用去了与本会话上一次调用不同的 provider(`model.order`):`from` 配额用完、没有密钥、密钥被拒、连不上或重试用尽,`reason` 逐个说明排在 `to` 前面的为什么没接;或排在前面的又能用了。`task` 是调用的种类(`main`、`subagent`、`compact`、`memory`、`title`、`web_search`、`web_fetch`);与父会话共用路由的子 Agent 的也报在父会话上,有自己路由的(`session.startAgent` / `Agent` / agent 类型的 `provider`,或有确定归属的模型 id,§6.3)报在它自己的事件里,经 `subagent_progress` 转出;无论哪种,子 Agent 任务的 `detail.model` / `provider` 随下一帧 `task_updated` 更新 |
| `notification` | `{message,status}` | 模型用 `PushNotification` 要用户回来看:宿主该把 `message` 以通知的形式送到用户面前(桌面通知、消息推送……)。`status` 是 `"proactive"`(模型主动发的)或 `null`;引擎自己的提醒(如 `permit_always` 的 `local` 没能写下)也走这一帧,`status` 为 `null`。同时触发 `Notification` hook,`reason` 即 `message` |
| `compact_started` | `{trigger,tokens_before,strategy?}` | 一次改写历史的压缩开始了,`PreCompact` hook 之前发;随后恰好一帧 `compact`(压成了)或 `compact_ended`(没压成)。修剪不发(§7.1) |
| `compact` | `{strategy,messages_before,messages_after,tokens_before,tokens_after,dropped_rounds?,dropped_messages?,estimated_tokens_saved?,trim?}` | 上下文被压缩或修剪了(§7.1) |
| `compact_ended` | `{outcome,reason?}` | `compact_started` 宣布的那次压缩没压成,历史原样(§7.1) |
| `user_message_delivered` | `{message_id,content}` | 运行中送来的消息(`on_busy:"steer"`)已加进对话,模型下一次调用会读到 |
| `user_message_refused` | `{message_id,content,message}` | 运行中送来的消息被 `UserPromptSubmit` 拒收,不在 transcript 里;`message` 是 hook 给的理由,可为 `null` |
| `user_messages_returned` | `{messages:[{message_id,content}]}` | 运行中送来、但本轮结束前没读到的消息,不在 transcript 里,交还客户端 |
| `turn_complete` | `{stop_reason,api_calls,tool_calls,seq,usage,usage_breakdown,last_call_usage,context?,structured_output?,message?,truncated?,persisted}` | 本轮结束,`persisted` 为 `true` 时 transcript 已落盘,`seq` 是此刻的消息数;最终响应随后到达 |
| `error` | `{code,message,retryable,retry_after_ms,persisted}` | 本轮出错结束。只发给**发起这一轮之外**的订阅者 —— 发起者从 `run_turn` 的错误响应得知。`code`、`message`、`retryable`、`retry_after_ms` 与发起者收到的 `ENGINE_ERROR` 的 `data.kind`、`message`、`data.retryable`、`data.retry_after_ms` 相同(§9.1;字段叫 `code` 是因为帧里已经有 `kind:"error"`),`message` 同样是 "<kind>: <描述>";`persisted` 同 `turn_complete` 的(§7.2)。会话在这一轮中途被关闭或删除时 `code` 为 `session_closed`,引擎在一轮中途退出时为 `engine_stopped`,这两种 `persisted` 为 `false`(引擎没来得及报这一轮写到了哪里) |
| `task_started` | `{task}` | 这个会话的一件任务开始了(`task` 同 `session.tasks` 的条目)。Shell 命令与监视器只在跑过 1 秒后才报,短命令不报;MCP 任务在 server 建任务时立即报;不属于任何一轮,`turn_id` 为 `null` |
| `task_updated` | `{task}` | 一件报过开始、还没结束的任务变了:子 Agent 每调完一次模型(`task.usage`,以及这次调用实际去的 `detail.model` / `detail.provider`),有询问开始或结束等待(`task.status` 在 `waiting` 与 `running` 之间);`turn_id` 为 `null` |
| `task_progress` | `{task_id,appended,skipped_bytes,from_byte,next_byte}` | 一件报过开始的 Shell 命令、监视器、`RunCode` 程序或 MCP 任务(状态行 `[<mcp_status>] <status_message>`,每次变化一行,不受每秒一帧限制)新打出来的输出,`appended` 是偏移 `from_byte..next_byte` 那段(偏移同 `session.taskOutput`)。每件任务每秒至多一帧,结束时在 `task_finished` 之前补最后一帧;一帧至多 8 KiB,多出的只发结尾,`skipped_bytes` 是跳过的字节数,恒等于本帧 `from_byte` 减上一帧 `next_byte`;漏帧或跳过的用 `session.taskOutput {from_byte}` 补;`turn_id` 为 `null` |
| `task_finished` | `{task}` | 一件报过开始的任务结束了,`task.status` 是结局,`failed` 时 `task.error` 写明原因(宿主起的子 Agent 被 token 预算截停是 `budget_exceeded: <谁的预算…>`,即它那一轮 `turn_complete.message` 的原文,§6.3 `session.startAgent`);此后没有它的 `task_progress`,输出不再长,`session.taskOutput` 的 `final` 为 `true`。被停掉的任务等工作收尾后才发(§6.3 `session.stopTask`);`turn_id` 为 `null` |
| `goal_changed` | `{goal}` | 会话目标变了(`goal` 同 `session.goal`,清除后为 `null`);`turn_id` 为 `null` |
| `extension_entry` | `{entry}` | 宿主经 `session.appendEntry` 写了一条扩展条目(`entry` 同 `session.entries` 的条目);`turn_id` 为 `null` |
| `permission_mode_changed` | `{mode,source}` | 会话的权限模式变了:`mode` 是变成的模式(§4.5),`source` 为 `host`(`session.setPermissionMode`,或 `session.rewind` 回到了 `plan` 里)或 `model`(模型的 `EnterPlanMode` / `ExitPlanMode`)。会话重建或恢复时按设置得到的模式不推,要知道读 `session.resume` 的 `permission.mode`。推给全部订阅者,包括发起切换的那条连接;`turn_id` 为 `null` |
| `rewound` | `{rewind_id,scope,cut,seq,files_restored,files_skipped,overwrote_external}` | 宿主用 `session.rewind` 回滚了这个会话(`scope`、`cut` 同它的回执);`seq` 是回滚后的消息数,与 `session.history` 的 `total` 相同。收到就丢掉 `seq` 之后画出来的消息并重取 `session.history`。推给全部订阅者,包括发起的那条连接;`turn_id` 为 `null` |
| `subscription_dropped` | `{reason}` | 这条连接对该会话的订阅被取消了(`reason` 目前只有 `"overflow"`,§5.4);不属于任何一轮,`turn_id` 为 `null`。重新 `session.subscribe` 并追平 |

`prompt`、`host_tool_call`、`host_tool_call_resolved`、`hook_call`、`hook_call_resolved`、`subagent_progress`、`team_progress`、`skills_changed`、`tool_progress`、`notification` 在两轮之间也会发:
后台子 Agent 在父会话空闲时照样会问、会推进度。这时它们不属于任何一轮,`turn_id` 为 `null`。

**这张表是白名单。** 引擎内部还有其它事件,但只有上面这些会被转成帧发出来。特别地:
没有 `turn_state` / `agent_state` 事件 —— 判断 turn 是不是在跑要问 `session.get` 的
`turn_state`。

**但客户端必须忽略不认识的 `kind`。** 这张表只会往下长,而且加一种事件不该是一次
破坏性变更:碰到没见过的 `kind` 就跳过它,不要报错、不要断开、更不要把整条连接当成
协议错误。

一轮帧的次序是:`turn_start`,然后每次模型调用一组 `step_start` … `step_end`,这次调用
的 `text_delta` / `thinking_delta` / `tool_use` / `tool_progress` / `tool_result` / `prompt` 都落在这一组
里,最后是 `turn_complete`(或 `error`)。同一次调用的 `tool_progress` 都在它的 `tool_use` 之后、
`tool_result` 之前。本地命令(`/clear` 之类)不调用模型,只有
`turn_start` 和 `turn_complete`。

并发安全的工具(读文件、搜索、抓网页……)在它的 `tool_use` 到齐时就开始跑,模型还在接着
写,所以它的 `tool_result` 可以夹在同一步后面的 `text_delta` / `tool_use` 之间到达。

**一次调用在回复要工具之前失败**(过载、尺寸被拒、流中途的 `error` 事件、连接在消息结尾
之前断开),它这一步推过的文本不会写进 transcript;恢复策略决定重发(换备用模型、压缩后
重试、等一会儿原样重发)时,先再发一次同 `step` 的 `step_start`,宿主应丢掉这一步已经画出来
的文本;原样重发之前还有一帧 `model_retry`。一次调用至多恢复 4 次。不重发,这一轮就以
`ENGINE_ERROR` 结束。请求在首字节之前就失败的(429、过载、连不上),client 已经按退避阶梯
重试过,恢复策略不会再原样重发:`data.retryable` 为 `true`,服务端说了要等多久的,
`data.retry_after_ms` 带着它(比 client 肯等的 60 秒还长也照样带出),什么时候再发由宿主决定。
回复已经要过工具的,工具已经跑了,回复连同结果写进 transcript,这一轮以 `ENGINE_ERROR`
结束,不会重发,`data.side_effects` 为 `true`。

`prompt_type` 有两种:`"permission"` 问一次工具调用能不能跑;`"question"` 是模型用
`AskUserQuestion` 问用户的,只发给在 `session.create` 的 `options.prompt_types` 里列了
`question` 的会话(§6.3)。两种共用同一帧、同一回答方法与同一个超时。

### 7.1 `compact`、`compact_started`、`compact_ended`

```jsonc
{ "kind":"compact", "strategy":"Snip",
  "messages_before":84, "messages_after":22,
  "tokens_before":152000, "tokens_after":41000,   // 引擎对历史大小的估算
  "dropped_rounds":9, "dropped_messages":61,      // 可选,压缩策略给不出就不出现
  "estimated_tokens_saved":31200,                 // 同上
  "trim":true }                                   // 可选,只在修剪时出现
```

`strategy` 是引擎选中的那一档:`Snip`(丢最旧的轮次)、`MicroCompact`(清空旧工具结
果的正文)、`CollapseContext`(把旧轮次折成摘要)、`FullCompact`(LLM 生成全文摘要)。
这份名单会随引擎增删,**按未知值处理它**,别把没见过的策略当成错误。

上下文被压掉是使用者**看得见结果、看不见原因**的一件事 —— "它怎么把前面说过的忘
了"。这条帧是唯一说出原因的时机:事后从 transcript 里推不回来,`session.get` 也不
答它。压缩发生在轮次中间,所以它**不是终止帧**,后面照常有 `text_delta` 与
`turn_complete`。

每一次真正改了对话的压缩或修剪都发这一帧。**压缩**改写历史、丢消息:上下文过了阈值、
`/compact`、请求因体积被厂商拒绝后的压缩重发;它触发 `PreCompact`/`PostCompact`,帧里没有
`trim`。**修剪**只把旧工具结果的正文换成占位:开了 `compaction.reactive` 时轮中提前清掉旧
工具结果的那一趟,以及按时间清掉旧工具结果的那一趟(会话不少于 100 条消息时,15 分钟以前的);
两者都是 `MicroCompact`,消息条数不变,`tokens_after` 变小,帧上带 `"trim": true`,不触发
hook。没有可丢的、或者丢掉的还不如压缩后补回去的提醒多时,什么都不改,也不发这一帧。
压缩的 `tokens_after` 算的是压缩后的整段对话,包括补回来的恢复消息(压缩前读过的文件、
调用过的 skill、计划模式与后台任务的提醒)。

后三项是策略相关的:给不出的策略就不带这三个键,不是带 `null`。

**压缩有首尾两帧**(能力位 `compact_events`)。写摘要要调一次模型,可能要十几秒;这段时间里
这一轮没有别的输出,开始帧让宿主知道引擎在压缩:

```jsonc
{ "kind":"compact_started", "trigger":"threshold",   // threshold | manual | overflow
  "tokens_before":152000,                            // 引擎对历史大小的估算,口径同 compact 帧
  "strategy":"FullCompact" }                         // 可选,只在动手前就定下来时出现
{ "kind":"compact_ended", "outcome":"failed",        // declined | nothing_to_compact | failed | cancelled
  "reason":"summarization failed: …" }               // 可选,failed 时说明原因
```

每次**压缩**(上面说的改写历史的那种,无论阈值逼出来的、`/compact` / `session.compact` 要的、
还是请求因体积被拒后的压缩重发)在 `PreCompact` hook 之前先发一帧 `compact_started`,之后
**恰好一帧**结束:压成了是 `compact`(不带 `trim`),没压成是 `compact_ended`。两帧之间可能
夹着 `PreCompact` 的 `hook_call`、摘要请求的 `provider_switched` 之类,不会夹着另一对压缩帧。修剪是瞬间的,只发带 `trim` 的
`compact`,不发开始帧,也不发 `compact_ended`。

`trigger`:`threshold` 是上下文过了阈值,`manual` 是 `/compact` 或 `session.compact`,`overflow`
是请求因体积被厂商拒绝、压缩后重发(交给 `PreCompact` 的 `trigger` 仍是 `auto`)。`strategy`
只在带 `at` 的手动压缩上出现(一定是 `FullCompact`);别的压缩动手前还不知道会走哪一档,看结尾的
`compact`。

`outcome`:`declined` 是 `PreCompact` hook 拒了;`nothing_to_compact` 是没有可丢的,或丢掉的
还不如补回去的多(上面说的不改、不发 `compact` 的那种);`failed` 是 compactor 或摘要失败
(带 `at` 时摘要失败或没有摘要模型也是),`reason` 是说明文字;`cancelled` 是压缩途中被
`session.interrupt` 中断。四种都不改历史。值的名单只会增加,不认识的按"没压成"处理。带 `at`
却找不到那一点时还没开始压,这两帧都不发。

子 Agent 侧链里的压缩同样有这两帧,和 `compact` 一样经 `subagent_progress` 转出。

### 7.2 `turn_complete`

```jsonc
{ "kind":"turn_complete", "stop_reason":"end_turn", "api_calls":2, "tool_calls":1, "seq":14,
  "usage":{…},                 // 本轮所有调用的合计,含子 Agent 与旁路调用
  "usage_breakdown":{ "own":{…}, "side":{…}, "delegates":{…} },  // usage 的拆分
  "last_call_usage":{…},       // 本轮最后一次调用
  "context":{ "tokens":120000, "compact_threshold":150000, "window":200000 },
  "structured_output":{…},     // 可选
  "message":"…",               // 可选
  "truncated":true,            // 可选,只在为 true 时出现
  "persisted":true }
```

`context.tokens` 是最后一次调用发出去的上下文大小,按厂商的计数(输入加两项缓存),不是
本地估算。`compact_threshold` 是自动压缩开始的位置,`window` 是模型的上下文窗口 ——
做一个"上下文用了多少"的指示条,这三个数就够了。本轮没有调用模型(被 hook 拦下、本地
命令)时没有 `context` 键。

`turn_complete` 的 `usage` 是完整的四字段用量(§3.5),
**是这一轮所有模型调用的合计**,不是最后一趟。一轮可能来回好几趟
(`api_calls` 就是趟数),按最后一趟计费会少算——用它做预算的宿主尤其要看清这一点。

它与 `turn.token_budget` 算的是同一组调用(§4.8):这个会话自己的、它在旁边发的(压缩、记忆、
`WebSearch` / `WebFetch` 的模型调用)、以及这一轮起的子 Agent 与团队成员的(连同它们各自的旁路调用与再起的
子 Agent)。`usage_breakdown` 把它分成三份,各是四字段用量,三份相加等于 `usage`:`own` 是这一轮主循环
自己的调用,`side` 是旁路调用,`delegates` 是子 Agent 与团队成员的。拿 `usage` 的 `input_tokens` 加 `cache_creation_input_tokens` 加
`output_tokens` 就能和预算对上。
`turn_complete` 发出时还没结束的调用不在里面:轮末起的记忆抽取、这一轮起的后台子 Agent 此后的调用照样算进
这一轮的预算,但报不进已经发出的这一帧;它们的用量在各自的任务上(`session.tasks`、`task_finished` 的
`task.usage`)。

`structured_output` 是本轮模型最后一次成功交给 `StructuredOutput` 的内容,会话带
`output_schema` 时它一定满足那份 schema;本轮没有调用这个工具就没有这个键。

`message` 只在截停这一轮的一方给了理由时出现 —— 比如 hook 返回的 `message`;`stop_reason` 此时是 `stopped_by_hook`。

`truncated: true` 表示本轮有一次模型调用的流在厂商标明结束之前就关了:OpenAI 兼容的流
既没有 `[DONE]` 也没有 `finish_reason`。不少网关两样都不发,所以这种流照常当作回复结束,
收到的内容就是整个回复,`stop_reason` 按 `end_turn` 报;但连接中途断开看起来一模一样,
回复可能不完整,宿主可以据此提示或重发。其余情况没有这个键。

`persisted: false` 表示这一轮写日志失败了(磁盘满、存储不可用):这一轮确实跑完了,工具跑过、
文件改过,所以照样以 `turn_complete` 结束、`run_turn` 照样成功 —— 不要把它当失败重发。但
transcript、目标、计划模式与 todo 的落盘落在后面,内存里的会话是唯一完整的一份:daemon 不会
因空闲或腾位置驱逐它,需要重建的调用(`session.setPrompt`、换厂商或推理设置的 `session.setModel`)先让
引擎补写,补写失败就拒绝,`ENGINE_ERROR`,`kind` 为 `persist_failed`,会话照旧。以 `error`
结束的一轮写日志失败时也是这样,旁观者收到的 `error` 帧报 `persisted: false`;发起者收到的
`ENGINE_ERROR` 不带它,要看 `daemon.doctor` 里会话的 `log.behind`。下一轮开头会
自动补写,之后某一轮报 `persisted: true` 就说明日志已经追上。`session.close` 照常关闭,引擎
退出前再试一次补写。

**`stop_reason` 和 `usage` 只在这里出现**,`run_turn` 的最终响应里没有。

### 7.3 `tool_progress`

```jsonc
{ "kind":"tool_progress", "tool_use_id":"toolu_01…", "name":"ImportRows",
  "message":"row 42 of 120",   // 工具给的一句话,至多 4 KiB
  "progress":42,               // 可选,工具自己的单位(行、字节、步)
  "total":120,                 // 可选
  "dropped":3 }                // 上一帧以来合并掉的条数
```

工具在运行时经 `Tool::call` 收到的进度发送器报进度(`docs/extension_points.md`「报进度」),
每一条都对应这次调用的一行工具。内建工具里 `Bash` 每打出一行输出报一次:`message` 是这一行,
`progress` 是到此为止的行数,没有 `total`;其余内建工具目前不报。插件登记的工具报什么由
插件决定。`progress`、`total` 没有就不带这个键,不是带 `null`。两者是 JSON 数:整数值发成
整数(`42`,不是 `42.0`),只有工具报的本来就是小数(比如插件报 `0.5`)时才带小数部分,所以
`Bash` 的行数可以按整数解析;拿不准来源的宿主按浮点数读,两种都能接住。

`RunCode` 程序里调用的工具不报 `tool_progress`:宿主没见过这些子调用的 `tool_use`,它们的
id 也不出现在别的帧里。程序自己的进度(`log()` 的输出、调用了几次工具)走它的任务,
以 `task_progress` 推出,任务的 `tool_use_id` 就是那次 `RunCode` 调用。

**限流**:每次调用每秒至多一帧。一秒内的第一条立刻发;其后的只留最后一条,这一秒到了再发,
被它顶替掉的条数记进 `dropped`,随这一帧带出,之后从 0 重新计。调用结束时还留着一条的,
在 `tool_result` 之前补发。所以 `dropped` 加上帧数就是工具报过的总条数(漏帧的除外)。
调用返回之后再报的进度直接丢掉。

**不进 transcript,不保证送达**:它不写进会话日志,`session.history`、恢复与导出里都没有;
订阅跟不上时(§5.4)它和别的帧一样会丢,丢了也补不回来。要事后看的输出用任务输出
(`task_progress`、`session.taskOutput`)。客户端只该拿它画"这次调用正在做什么",结果以
`tool_result` 为准。

宿主工具的进度由宿主用 `session.reportToolProgress`(§6.3)报:引擎收下后为这次调用的
`tool_use_id` 发同样的帧,键就是上面的 `message`、`progress`、`total`,限流与别的工具一样。

子 Agent 里的工具报的进度不单独成帧,包在 `subagent_progress` 里:`event` 是
`{kind:"tool_progress",tool_use_id,name,message,progress?,total?,dropped,turn_id}`,
`turn_id` 是子 Agent 自己的一轮,`parent_tool_use_id` 是父会话里那次 `Agent` 调用。

### 7.4 `host_tool_call`

```jsonc
{ "kind":"host_tool_call",
  "call_id":"hc_01J…",          // 回答用它
  "tool_use_id":"toolu_01…",    // 引出它的那次 tool_use(子 Agent 的是子会话里的 id)
  "name":"HostEcho",
  "input":{ "text":"ping" },
  "timeout_ms":120000,
  "task_id":null,               // 子 Agent 发起时为它的任务
  "agent_session_id":null,      // 子 Agent 发起时为它的侧链会话
  "cwd":"/work",                // 发起调用的会话的工作目录
  "depth":0 }                   // 发起调用的会话的委派深度,根会话 0

{ "kind":"host_tool_call_resolved", "call_id":"hc_01J…",
  "resolution":"withdrawn", "reason":"timed_out" }
```

键是 `call_id` 而不是 `tool_use_id`:厂商的 `tool_use_id` 只在一条回复里唯一,OpenAI 兼容厂商
的 id 形如 `call_0`,父会话与子 Agent 完全可能同号,而子 Agent 的调用也落在父会话的流上。

每个调用有且只有一种结局,一轮永远能收尾:

| 结局 | 模型读到的 `tool_result` | `host_tool_call_resolved` |
|---|---|---|
| 回答了 `result` | 宿主给的内容,`is_error` 照宿主 | `answered` |
| 回答了 `error` | `` host tool `HostEcho` failed: <message> ``,错误 | `answered` |
| `timeout_ms` 到了没人答 | `` the host did not answer `HostEcho` within 120s ``,错误 | `withdrawn` / `timed_out` |
| 发起时该会话一个订阅者都没有 | `` no host is connected to answer `HostEcho` ``,错误 | `withdrawn` / `no_host` |
| 订阅者在回答之前走光了(`session.unsubscribe`、断开、慢连接被摘掉) | 同上 | `withdrawn` / `host_gone` |
| 连接提供的工具:提供者在回答之前断开,或投递给它失败(出站队列满) | 同上 | `withdrawn` / `host_gone`;断开的,之后再调这个工具立刻 `no_host` |
| 连接提供的工具:发起时提供者已断开或已撤回它 | 同上 | `withdrawn` / `no_host` |
| 这一轮被中断 | 同别的工具被取消 | `withdrawn` / `cancelled` |

没人连着的轮次(定时任务、目标的自动轮)里的调用因此立刻失败,不会每次都等满超时。只有旁观
的连接还在、真正实现工具的宿主崩了时,调用才要等到超时。取消当下就返回,不等工具的取消
宽限;宿主看到 `cancelled` 应自行停手,它之后的回答静默成功、不生效。

**多个订阅者**:帧广播给全部订阅者,任何一条连接都能答,先到引擎的算数,随后的
`host_tool_call_resolved{answered}` 推给所有人。约定是**只有实现了这个工具的宿主去执行它**,
旁观者只画"等待宿主"。两个都实现了的宿主会都去执行;看到 `resolved` 而自己没被采纳的一方
撤销副作用,或者一开始就只在自己创建的会话里执行。daemon 不仲裁。中途订阅的连接从
`session.subscribe` 的 `pending_host_calls` 拿到还悬着的帧,照样能答。

**连接提供的工具**(`daemon.setHostTools`,§6.1):帧发给提供者——**即使它没有订阅这个会话**——
再加上这个会话的订阅者(并集);只有提供者的 `session.respondToolCall` 算数,别的连接答 →
`INVALID_PARAMS`。「没有订阅者就 `no_host`」对它改成「提供者不在就 `no_host`」:提供者断开或
`daemon.setHostTools []` 之后,已建好的会话里还挂着这个工具的,调用立刻以 `no_host` 结束。

**子 Agent**:声明了 `subagents:true` 的工具,子 Agent(含团队成员)也拿得到,再受它的 agent
类型的工具列表收窄;没声明的子 Agent 看不到。子 Agent 的调用经父会话的通道发出,帧带
`task_id` 与 `agent_session_id`,用父会话的 `session_id` 回答。父会话空闲时后台子 Agent 照样能
调,这时帧的 `turn_id` 为 `null`。

**工作目录与深度**:`cwd` 是发起调用的那个会话的工作目录,`depth` 是它的委派深度:根会话 0,
它起的子 Agent 1,子 Agent 再起的 2,依此类推。在 worktree 里跑的子 Agent(`Agent` 的
`worktree`)的 `cwd` 是那个 worktree(`<仓库根>/<worktree 目录>/<名>`,缺省 `.wkts`),宿主工具该在那里读写。
子 Agent 结束时 worktree 若原样未动就删掉,否则连同分支 `agix/worktree-<名>` 留下(后台子 Agent
的任务 `detail.worktree` 记着,见 §6.3 `session.tasks`),所以之后的帧里这个 `cwd` 可能已经不存在,也可能
还在。`EnterWorktree` 不改会话的工作目录,之后的调用 `cwd` 照旧。

### 7.5 `file_changes`

`Write`、`Edit`、`NotebookEdit` 的 `tool_result` 帧带它们写了什么,模型收到的文字不受影响:

```jsonc
"file_changes":[
  { "path":"/Users/x/repo/src/lib.rs",    // 绝对路径,已解析的写入目标
    "op":"modify",                        // create | modify
    "before_hash":"sha256:9f86…",         // create 时为 null
    "after_hash":"sha256:60303…",
    "before_bytes":1824,                  // create 时为 null
    "after_bytes":1851,
    "added":3, "removed":1,               // 变更行数;没有 diff 时为 0
    "unified_diff":"--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -10,4 +10,6 @@\n…",
    "truncated":false,
    "omitted":null } ]                    // null | too_large | binary | redacted
```

- `unified_diff` 是标准的 unified diff,3 行上下文:路径在会话工作目录里时写
  `a/<相对路径>` / `b/<相对路径>`,不在时两边都是绝对路径;新建文件的旧侧是 `/dev/null`。
- 哈希只对 ≤ 16 MiB 的内容算,更大的为 `null`。diff 只在前后都 ≤ 1 MiB 且都是 UTF-8 时生成,
  否则 `unified_diff` 为 `null`、`omitted` 写 `too_large` 或 `binary`,哈希照给。diff 超过
  256 KiB 或 2000 个变更行时截在 hunk 边界,`truncated:true`;`added` / `removed` 仍是整个变更的。
  这些上限与模型那边 diff 文字的 100 行各算各的。
- 结果转换器(`docs/extension_points.md`「一个工具结果能长什么样」)改过这次调用的文字时,diff
  里可能正是被它去掉的内容:帧上只留路径、`op`、哈希与字节数,`unified_diff` 为 `null`、
  `omitted:"redacted"`。
- **不进 transcript。** `session.history` 里仍是文字;断线漏掉的 diff 拿不回来,要核对就用哈希
  对盘。`subagent_progress` 里嵌套的 `tool_result` 同样带它。`Bash` 改了哪些文件不知道,不报;
  MCP 与宿主工具也不报。

---

### 7.6 `hook_call`

```jsonc
{ "kind":"hook_call",
  "call_id":"hk_01J…",            // 回答用它
  "hook_event":"PreToolUse",
  "input":{ "hook_event_name":"PreToolUse", "session_id":"…", "cwd":"/work",
            "permission_mode":"default", "depth":0, "tool_name":"Bash",
            "tool_input":{ "command":"git push" }, "tool_use_id":"toolu_01…" },
  "timeout_ms":10000,
  "task_id":null,                 // 子 Agent 发起时为它的任务
  "agent_session_id":null,        // 子 Agent 发起时为它的侧链会话
  "cwd":"/work",                  // 同 input.cwd
  "depth":0,                      // 同 input.depth
  "plugin_call":{ "function":"xflow.check", "run_id":"gate-7" } }
                                  // 只在不带会话的 plugin.call 发出时有;run_id 没给就省略

{ "kind":"hook_call_resolved", "call_id":"hk_01J…",
  "resolution":"withdrawn", "reason":"timed_out" }
```

`daemon.setHooks`(§6.1)登记过 hook 的连接收到的帧:引擎走到登记的事件,就把这一刻的 payload
放在 `input` 里,等宿主用 `session.respondHook` 回答。`input` 的字段同设置里 hook 的 payload
(`docs/hooks.md`「payload」):`hook_event_name`、`session_id`、`cwd`、`permission_mode`、`depth`,
以及这个事件的 `tool_name`、`tool_input`、`tool_use_id` 等。外层 `session.event` 的 `session_id`
是根会话;子 Agent 的调用带 `agent_session_id`,而 `input.session_id`、`input.cwd` 与
`input.depth` 都是子 Agent 自己的,帧上的 `cwd`、`depth` 同它们(§7.4「工作目录与深度」)。

不带会话的 `plugin.call`(§6.5)发出的帧多一个 `plugin_call`:`function` 是调用的函数名,
`run_id` 是那次调用给的(没给就省略);外层 `session_id` 与 `input.session_id` 都是这次调用的临时
会话 id,回答用它,调用结束就不认了。别的帧没有这个字段。

**只发给登记的那条连接**(哪怕它没有订阅这个会话),不发给别的订阅者,`session.subscribe` 的
待答列表里也没有。一条连接登记了 hook,就等于被交出了这个 daemon 里每个会话的每次相应工具
调用与用户消息(§6.1 `daemon.setHooks` 的"影响面")。

每个调用有且只有一种结局,一轮永远能收尾:

| 结局 | 对调用的效果 | `hook_call_resolved` |
|---|---|---|
| 宿主回答了 | 按 `session.respondHook` 的回答 | `answered` |
| `timeout_ms` 到了没人答 | 按 `on_error` | `withdrawn` / `timed_out` |
| 这一轮被中断 | 同别的 hook 被叫停,按 `on_error` | `withdrawn` / `cancelled` |
| 登记者的连接断开 | 按 `on_error` | `withdrawn` / `host_gone` |
| 帧发不出去(连接已关或出站队列满) | 按 `on_error` | `withdrawn` / `host_gone` |
| 发起时登记者已不在 | 按 `on_error` | `withdrawn` / `no_host` |

`on_error` 是登记那一条 hook 时写的:`allow`(缺省)放行,`block` 拦下。回答的字段与效果见
`session.respondHook`。

## 8. 异步通知:`daemon.event`

推给所有调用过 `daemon.subscribeEvents` 的连接,不属于任何会话或 turn。

```jsonc
{ "jsonrpc":"2.0", "method":"daemon.event", "params": { "kind":"…" } }
```

| `kind` | 载荷 | 什么时候 |
|---|---|---|
| `mcp_connected` | `{server, transport, tool_count}` | 启动连接、`config.reload` 重连、`mcp.addServer` / `mcp.updateServer` / `mcp.removeServer`(别的层仍声明它时)/ `mcp.setEnabled`(打开时)/ `mcp.reconnect` 成功 |
| `mcp_connect_failed` | `{server, error}` | 同上,失败时,`error` 是原因(进程起不来、握手失败、超过启动时限……)。另外,启动、重连、另一个项目第一次起会话时要连的 server 里,配置解析不了的、被 MCP 策略拒绝的(`ARCHITECTURE.md` §6.4)各发一条,`enabled: false` 关掉的不发;会话创建时还没连完、之后在后台连上或失败的 server 也各发一条 `mcp_connected` / `mcp_connect_failed` |
| `mcp_auth_required` | `{server, scopes}` | 远端 server 要求登录:没有令牌、令牌被拒且刷新不了,或要求更多 scope(这时 server 保持连接);`enterprise` server 还没有可用的企业身份(宿主该调 `mcp.auth.setIdentity`)。`scopes` 是要请求的(可能为空)。同一个 server 从已登录/未涉及变成需要登录时发一次 |
| `mcp_auth_completed` | `{server}` | 登录完成,令牌已存;随后是这个 server 的 `mcp_connected` / `mcp_connect_failed` |
| `mcp_auth_failed` | `{server, error}` | 登录失败:授权服务器在回调里报错、换令牌失败,或 10 分钟内没完成 |
| `import_detected` | `{sources:[{source,description}]}` | 启动时探测到还没处理过的可导入配置。它在任何客户端连上之前就已产生,所以是本节唯一会补发的事件:对 daemon 自己项目的 `import.run` 回答之前,每个新订阅者订阅后先收到它,且只收到一份;对别的项目跑 `import.run` 不算回答。daemon 没有"不导入"的方法:不理它即可,它不阻塞任何操作,只在 daemon 运行期间对新订阅者补发、重启后再问 |
| `session_created` | `{session_id, scene, project_root}` | 一个新会话进了内存 |
| `session_resumed` | `{session_id, scene, project_root}` | 一个已有的会话被恢复进内存 |
| `session_evicted` | `{session_id, reason}` | 会话被移出内存,transcript 还在,可以恢复。`reason`:`idle`(空闲超时)、`capacity`(池子满了,挤掉最久没动的)、`rebuild`(原地重建时旧引擎 5 秒内没停下,见 `session.setModel`;或旧引擎停下之后新引擎没能读回日志,发起重建的调用失败)、`engine_fault`(daemon 里跑这一轮的部分出了故障,这一轮没能把会话交还;发起方的 `run_turn` 得到 `ENGINE_ERROR`、`data.kind` 为 `internal`,其余订阅者收到这一轮的 `error` 帧)。正在跑一轮、或还有后台任务在跑的会话不会被驱逐;池子满了而个个都忙时,新会话照样建,池子暂时超出上限。再用被驱逐的会话时,先等它旧的运行体停下(最多 5 秒;它的 `SessionEnd` 等只通知的 hook 在后台跑,不算在内)再重建,两个引擎不会同时在跑 |
| `session_closed` | `{session_id, reason}` | 会话被关闭。`reason`:`closed`(`session.close` / `session.delete`)、`engine_stopped`(引擎在一轮中途退出) |
| `session_deleted` | `{session_id, parent_session_id?}` | 会话连同 transcript 被删除;级联删除的侧链带 `parent_session_id` |
| `turn_started` | `{session_id, turn_id, initiator}` | 一轮开始。`initiator`:`client`(某条连接的 `run_turn`)、`schedule`(定时任务,§6.7)、`goal`(目标的自动轮) |
| `turn_completed` | `{session_id, turn_id, initiator}` | 这一轮结束,无论成功、出错还是被中断 |
| `events_lagged` | `{missed}` | 这条连接读得太慢,有 `missed` 条通知没送到。需要的话用 `session.list` 等对账 |
| `project_trust_changed` | `{project_root, trust}` | `project.trust` 写完之后;`trust` 是写完后的状态(§3.6) |

会话与轮的事件让宿主**不必订阅每个会话**就知道哪些会话在内存里、哪些在跑 ——
包括不是它发起的轮(定时任务,或者别的客户端)。没有 `config_reloaded` /
`scene_activated` 之类的通知:场景激活、配置重载不会广播。

客户端必须忽略不认识的 `kind`,这张表只会往下长。

---

## 9. 错误码

| 码 | 名称 | 含义 |
|---|---|---|
| -32600 | INVALID_REQUEST | 帧是合法 JSON、带着字符串或数字的 `id`,却不是合法请求(缺 `method`、字段类型不对,§1);`session.startAgent`:会话装配时没有子 Agent(没有模块提供 `Delegator`) |
| -32601 | METHOD_NOT_FOUND | 未知方法;或方法所属的模块没有编入这个构建,此时 `data.module` 指明模块(§6.1 `daemon.ping`) |
| -32602 | INVALID_PARAMS | 参数缺失/类型错/取值非法 |
| -32603 | INTERNAL_ERROR | 服务端内部错误(写盘失败、会话创建失败等) |
| -32000 | SESSION_NOT_FOUND | 会话不存在,或该方法要求它在内存里而它不在 |
| -32002 | ENGINE_ERROR | turn 执行期错误,或引擎没有接下、没有回答一个回合之外的请求(`session.inspectContext` / `dropMessages` / `editMessage` / `rewind`),或重建之前补写日志失败(`session.setPrompt`、换厂商或推理设置的 `session.setModel`);`message` 形如 `"<kind>: <描述>"`,`data` 见 §9.1 |
| -32003 | UNAUTHORIZED | TCP / WebSocket 握手缺失或失败,开了 `--unix-socket-auth` 时 Unix socket 也一样;Unix socket 上的对端是 daemon 自己起的进程(§5.1)。不需要握手的连接上发 `daemon.auth` 得到的是 `METHOD_NOT_FOUND`(-32601) |
| -32004 | SCENE_NOT_FOUND | 场景未注册,或(`session.create`)未激活 |
| -32006 | SCENE_MISMATCH | 会话记录的场景与这个 daemon 的不符 |
| -32009 | SCENE_HAS_ACTIVE_SESSIONS | `scene.deactivate` 被内存中的会话挡住 |
| -32010 | PROJECT_NOT_FOUND | `project_root` 不存在或不是目录 |
| -32012 | PROJECT_REQUIRED | 场景 `requires_project` 为真,但传了 `project_root: null` |
| -32014 | SIDECHAIN_TERMINAL | 侧链会话已终态收尾,不可 resume |
| -32015 | SESSION_BUSY | 会话正忙,`data.reason` 说它在忙什么(§9.1):已有 turn 在跑且 `on_busy` 是 `reject`(§5.3),正在原地重建,有后台任务挡着需要重建的调用,正在做上下文操作或回滚,或要把它调回内存时它上一个引擎还没停下(`session.resume`),或 `session.create` 要的 id 正被 `session.import` 写入 |
| -32016 | PLUGINS_DISABLED | 插件子系统不可用 —— 被编译裁掉或被策略关闭 |
| -32017 | TOO_MANY_IN_FLIGHT | 这条连接同时在途的请求已达上限(§5.2) |
| -32018 | TASK_NOT_FOUND | `session.taskOutput`:引擎不知道这个任务(没跑过、结束得早已被忘掉,或引擎不在内存里) |
| -32019 | TASK_LIMIT | `session.startAgent`:会话的子 Agent 已到 `agents.max_concurrent`,或委派已到最深 |
| -32020 | MCP_SERVER_DENIED | `mcp.addServer` / `mcp.updateServer`:MCP 策略拒绝这个 server,没有写入任何东西;`mcp.reconnect`:这个 server 被策略拒绝;`mcp.auth.start` / `mcp.auth.logout`:这个 server 被策略拒绝,或(`start`)它的授权服务器不在 `allowed_authorization_servers` 里 |
| -32021 | MCP_AUTH_FAILED | `mcp.auth.*`:授权服务器或登录本身失败——发现、注册、PKCE、回调 `state` 不符、换令牌;`message` 可以直接给用户看 |
| -32022 | PROJECT_UNTRUSTED | `mcp.addServer` / `mcp.updateServer` / `config.setProvider`(`tier:"project"`):目标项目未被信任(§3.6),写进它项目层的 server / provider 不会生效,所以什么都没写;`config.set` / `mcp.setEnabled`:被拒的键全部是因为项目未受信(`setEnabled` 是在未受信项目里打开),什么都没写,`data.rejected` 逐键列出(§9.1);`mcp.reconnect`:未受信项目自己的 server;`session.resume` / `session.history` / `session.export` / 带 `session_id` 的 `session.create` / `session.run_turn` 等:会话放在现在不受信的项目里(§3.6) |
| -32023 | REWIND_PRECONDITION | `session.rewind`:有文件不能照要求恢复(会话之外改过、写前内容没留下、写路径策略拒绝、是符号链接、worktree 已删),什么都没改;`data.issues` 列出全部(§9.1) |
| -32024 | REWIND_FAILED | `session.rewind` 执行到一半失败;已恢复的文件退回了原样,除非 `data.partial` 为 `true`(§9.1) |
| -32025 | SESSION_EXISTS | `session.import` 带 `keep_ids`:归档里的某个会话 id 在这个 daemon 上已经有了,什么都没写;`session.create` 给的 `session_id` 是另一个场景、项目或 sidechain 的会话,或建它时的 `options` 与这次不同,什么都没做(§9.1) |
| -32026 | SETTINGS_LOCKED | `project.trust` / `config.set` / `config.setProvider` / `mcp.addServer` / `mcp.updateServer` / `mcp.removeServer` / `mcp.setEnabled`:设置目录被别的进程握着,等满 10 秒仍没拿到锁,什么都没写;放手后重发(§6.4) |
| -32027 | MODELS_UNSUPPORTED | `config.listModels`:provider 的端点不列模型(回 404 / 405 / 501,或回的不是模型列表),或它的 `api` 没有引擎知道的列法(§6.4) |
| -32028 | SETTINGS_CONFLICT | `config.set` 带 `if_revision`:这一层在那之后被改过,什么都没写;`data.revision` 是现在的 revision(§6.4、§9.1) |

`PARSE_ERROR (-32700)` 只在一行不是合法 JSON 时回,`id` 为 `null`。`-32001` 保留不用:会话数到顶时驱逐最久未用的空闲会话,不拒绝新建。

### 9.1 `error.data` 的形状

**只有十一个错误码带结构化 `data`**,客户端可以依赖:

```jsonc
// ENGINE_ERROR
{ "kind": "rate_limited", "retryable": true, "retry_after_ms": 30000, "side_effects": false }

// SESSION_BUSY
{ "session_id": "S1", "reason": "turn_running", "current_turn_id": "t1" }
// 需要重建的调用(setPrompt、换厂商或推理设置的 setModel)被后台任务挡住时,多一项
{ "session_id": "S1", "reason": "background_tasks", "current_turn_id": null, "running_task_ids": [ "a1" ] }

// SCENE_MISMATCH
{ "session_id": "S1", "recorded_scene": "harnix", "requested_scene": "code" }

// SIDECHAIN_TERMINAL
{ "session_id": "S1c1", "parent_session_id": "S1", "final_state": "completed" }  // completed | failed

// INVALID_PARAMS,只在 options 里有读不出来的字段时
{ "invalid_options": [ { "field": "settings.permissions.mode", "error": "…" } ] }
// 另有只有建出引擎才知道的:host_tools 的重名,以及场景 tools.deny 了 StructuredOutput 时的 output_schema;
// 还有 skill_dirs 通不过的检查(session.create 等建新会话时,以及 session.setSkillDirs,field 为 skill_dirs[i].path)
// INVALID_PARAMS,session.calls / session.call 带的 generation 不是录像当前的时
{ "generation": 1763200000000 }

// INVALID_PARAMS / PROJECT_UNTRUSTED,config.set、config.setProvider 与写 MCP 设置的方法有键被拒时(§6.4、§6.5)
{ "rejected": [ { "key": "sandbox.network.mode", "kind": "invalid_value", "reason": "…" },
                { "key": "mcp.servers.github", "kind": "use_method", "method": "mcp.addServer", "reason": "…" } ] }

// SETTINGS_CONFLICT
{ "revision": "r1:…" }

// METHOD_NOT_FOUND
{ "module": "mcp" }   // 只在方法所属的模块没有编入这个构建时;未知方法没有 data

// REWIND_PRECONDITION
{ "issues": [ { "path": "a.txt", "issue": "modified", "detail": "…", "overridable_by": "force" } ] }

// SESSION_EXISTS
{ "session_id": "S1" }
// session.create 带的 session_id 是别的会话时,多出那个会话是什么;只是 options 不同时另带 options_differ
{ "session_id": "S1", "scene": "code", "project_root": "/abs/repo", "session_kind": "primary",
  "options_differ": [ "settings" ] }

// REWIND_FAILED
{ "rewind_id": "…", "failed": { "path": "b/b.txt", "error": "…" }, "rolled_back": [ "a.txt" ],
  "not_rolled_back": [ ], "partial": false, "commit_not_recorded": false, "conversation_changed": false }
```

`ENGINE_ERROR` 的四项总是都在:

- `retryable`:同一个请求晚些再发可能成功。为 `false` 的(换了 key、充了值、改了请求之前)
  再发也是同样的结果。
- `retry_after_ms`:服务端要求等多久(毫秒),没说时为 `null`。只是下限,宿主可以等得更久。
- `side_effects`:这一轮已经有工具跑过,结果在 transcript 里。再发这条消息是新的一轮,
  模型可能把那些工具再调一遍。
- `kind`:

| `kind` | 什么情况 | `retryable` |
|---|---|---|
| `rate_limited` | 模型服务限流(429),client 的退避阶梯已用完,或服务端要求等的比阶梯肯等的长 | `true` |
| `overloaded` | 模型服务过载(503、529),没有备用模型,或已经换过 | `true` |
| `model_unavailable` | 连不上、超时、流中途断开,或 408、500、502、504 | `true` |
| `context_too_long` | 请求因过长被拒,压缩之后还是不行 | `false` |
| `model_rejected` | 请求被拒:其余 4xx、模型不存在、厂商做不了这件事 | `false` |
| `auth` | 401、403 | `false` |
| `no_credentials` | 这一轮要用的 provider 没有可用的密钥,请求没有发出,提示词没有离开本机;`message` 说明是哪个 provider(缺省客户端是 `default`)缺什么、去哪配 | `false` |
| `billing` | 402、余额不足 | `false` |
| `egress_denied` | 出口策略不允许连这个模型服务 | `false` |
| `model_error` | 其余 5xx,或响应读不懂 | `false` |
| `session_closed` | 这一轮中途会话被关闭或删除 | `false` |
| `engine_stopped` | 引擎在一轮中途退出;会话已移出内存,下一次调用(包括排在这一轮后面等着的)从 transcript 恢复它。引擎 panic 也走这里,但只在会 unwind 的构建里(dev、`release-fast`);`--release` 构建是 `panic = "abort"`,panic 直接结束整个 daemon | `true` |
| `engine_unreachable` | 引擎没有接下、没有回答一个回合之外的请求 | `true` |
| `providers_unavailable` | `model.order` 里的 provider 都接不了这次调用:配额用完、没有密钥、密钥被拒、连不上或重试用尽;`message` 逐个列出原因。只有一个 provider 时报它自己的错误种类 | `true` |
| `persist_failed` | 会话的日志缺了它在内存里的一部分(上一轮写日志失败,如 `turn_complete` 报 `persisted: false`),重建前补写又失败;会话没有重建,照旧在跑 | `true` |
| `internal` | 引擎内部错误 | `false` |

以后可能增加取值;不认识的 `kind` 按 `retryable` 处理。

`SESSION_BUSY` 的 `reason` 说的是会话此刻在忙什么,不是哪个方法被拒:

| `reason` | 会话在做什么 | `current_turn_id` |
|---|---|---|
| `turn_running` | 有一轮在跑(`run_turn` 的 `reject`、`setModel`、`setPrompt`、`inspectContext` 等都报这一项) | 那一轮的 id,必定有值 |
| `rebuilding` | 正在原地重建(`setPrompt`、换厂商或推理设置的 `setModel`、配置变更后下一轮开始前) | `null` |
| `background_tasks` | 空闲,但有后台任务或宿主的 `plugin.call` 在跑,挡住了需要重建的调用或回滚;另带 `running_task_ids`(调用还没起任务时可能为空);有宿主的 `plugin.call` 在跑时 `message` 写明这一点 | `null` |
| `previous_engine` | 刚被关闭、驱逐或重建,上一个引擎在宽限期内还没停下 | `null` |
| `context_operation` | 一个 `inspectContext` / `dropMessages` / `editMessage` 还没答完,会话归它独占 | `null` |
| `rewinding` | 一个 `session.rewind` 还没答完,会话归它独占 | `null` |
| `importing` | 一个带 `keep_ids` 的 `session.import` 正在写这个 id(`session.create` 带 `session_id` 时) | `null` |

`rebuilding`、`previous_engine`、`context_operation`、`rewinding` 和 `importing` 的 `data` 形状与第一行相同。`session.rewind`
被后台任务挡住时报 `background_tasks`,同样带 `running_task_ids`。以后可能增加取值,不认识的
`reason` 按"稍后再试"处理。

其余错误码**没有 `data`**,信息全在 `message` 里 —— 包括
`SCENE_HAS_ACTIVE_SESSIONS`(挡路的会话 id 是拼在 message 文本里的)、
`PROJECT_NOT_FOUND`、`PROJECT_REQUIRED`,以及不是上面两种的 `INVALID_PARAMS`。要展示给用户就直接展示
`message`,不要试图解析它。

---

## 10. 权限默认值

daemon 会话的默认权限模式是 **`default`(ask)**,不是放行一切。

工具调用触发询问时,daemon 推送 `session.event {kind:"prompt"}` 并**阻塞该次工具
调用**,直到:收到 `session.respondToPrompt`;或超过设置 `permissions.prompt_timeout_secs`
(`agixd --permission-prompt-timeout` 是它的简写,默认 300 秒)→ **按拒绝处理**;或会话被关闭 → 按拒绝处理。

超时**永远失败关闭**,不会自动放行 —— 无人应答不等于同意。

模型的提问(`prompt_type:"question"`,§7)用同一个 `permissions.prompt_timeout_secs` 计时(0 为
不限);超时、会话关闭或宿主 `decline`,模型都从 `AskUserQuestion` 的结果里读到用户没有回答,
不会把沉默当成回答。子 Agent 没有 `AskUserQuestion`。

MCP server 的询问(`prompt_type:"mcp_elicitation"`)也用这个计时;超时、会话关闭或这一轮被
中断,server 都不会拿到人没给过的回答:超时与没人能答回 `cancel`,中断时整个工具调用被丢弃。
2026-07-28 的 server 一轮一轮地问,工具调用的时限按轮算,人想的时间不算在内;老版本 server 的
`elicitation/create` 发生在它的 `tools/call` 里,人想的时间也算进工具调用的时限。老版本 server 被几个会话共用、同时
有不止一个调用在途时,它的询问认不出是哪个调用的,一律回 `cancel` 而不猜。

**子 Agent 的询问在父会话上问。** 子 Agent(模型起的、`session.startAgent` 起的、前台
的、后台的)的工具调用照父会话的权限判;要问时,父会话推一帧 `prompt`,`message` 以
`[sub-agent <侧链会话 id>]` 开头。用 `session.respondToPrompt` 回答,`session_id` 填
**父会话**,不是侧链会话。子 Agent 调的 MCP 工具问用户时也一样。父会话空闲时这帧照样推(`turn_id` 为 `null`,§7)。等回答
期间子 Agent 的任务是 `waiting`,`waiting_for` 里有这个 `prompt_id`(§6.3 `session.tasks`、
§7 `task_updated`)。超时与关闭的规则同上。子 Agent 与团队成员有人无人值守同它的根会话(`options.attended`):
根会话无人值守时它们的询问直接被拒绝,不推 `prompt`。

`permission_mode: "bubble"` 不是另一条路:子 Agent 的询问无论它是什么模式,到宿主的都是
上面这条父会话的 `prompt` 帧,团队成员也一样。判的模式是子 Agent 自己的:agent 类型的 `permission_mode` 可以比父会话宽(插件带来的类型
只能更严),但宽不过 managed 层的 `permissions.mode`,managed 关掉 bypass 时也不是 `bypassPermissions`;团队的 `permission_mode` 只能更严;父会话在 `plan` 或 `dontAsk` 时取两者中更严的那个,父会话
是 `bypassPermissions` 时随父会话。插件带来的、未信任项目里的 agent 类型写了 `permission_mode` 的不随:每次
调用都取它与父会话此刻模式中更严的那个,父会话起它之后被切成 `bypassPermissions` 也一样。它的询问等多久同父会话的(`permissions.prompt_timeout_secs`);答 `permit_always`
记下的是这一条命令或路径。

宿主自己已做沙箱、不需要第二层权限时显式声明放行:任意层 settings.json 的
`"permissions": {"mode": "bypassPermissions"}`(daemon 范围),或 `options.settings.permissions.mode:
"bypassPermissions"`(单会话范围)。

会话级只能**收紧**、不能放宽,除非 settings 里开了 `permissions.client_may_loosen`
(§4.5)。

部署方的 managed 设置(`ARCHITECTURE.md` §6.1)在这两者之上:它的 `deny` / `ask` 在
`bypassPermissions` 下照样拒绝 / 询问,`PreToolUse` hook 的 `approve` 也答不了它的 `ask`——这一问
一定以 `prompt_type:"permission"` 的 `prompt` 帧发给宿主(无人值守时拒绝);它写了 `permissions.disable_bypass: true` 时,
上面两种声明都只得到 `default`。

---

## 11. 从连接到跑完一轮

### 11.1 最小客户端骨架

帧格式是 NDJSON,一行一个 JSON 对象。下面覆盖了最容易写错的三件事:发请求、按 `id`
收响应、把其余的行当事件分发出去。

```ts
import * as net from "node:net";
import * as readline from "node:readline";

class DaemonClient {
  private socket: net.Socket;
  private nextId = 1;
  private pending = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void }>();
  public onEvent: (method: string, params: any) => void = () => {};

  constructor(socketPath: string) {
    this.socket = net.createConnection(socketPath);
    readline.createInterface({ input: this.socket })
      .on("line", (line) => this.handleLine(line));
  }

  private handleLine(line: string) {
    if (!line.trim()) return;
    const msg = JSON.parse(line);
    // 有 id 且带 result/error → 是某个 call() 的响应;否则是推送帧。
    if (msg.id !== undefined && ("result" in msg || "error" in msg)) {
      const p = this.pending.get(msg.id);
      if (!p) return;                       // 未知 id:丢弃,不抛错
      this.pending.delete(msg.id);
      if (msg.error) p.reject(Object.assign(new Error(msg.error.message), msg.error));
      else p.resolve(msg.result);
    } else {
      this.onEvent(msg.method, msg.params); // session.event / daemon.event
    }
  }

  call(method: string, params: unknown = {}): Promise<any> {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.socket.write(JSON.stringify({ jsonrpc: "2.0", method, params, id }) + "\n");
    });
  }
}
```

`pending` 必须是按 `id` 索引的 map,不能当队列使 —— 响应会乱序回来(§5.2)。
TCP / WebSocket 上,第一件事是 `call("daemon.auth", {token})`。

### 11.2 走一遍

```jsonc
// 1. 发现:读 ~/.agix/daemon/instances.d/*.json,联合判活后连它的 socket

// 2. 看有哪些场景,决定要不要弹"选择项目"
→ {"jsonrpc":"2.0","method":"scene.list","id":1}
← {"jsonrpc":"2.0","id":1,"result":{"scenes":[
     {"scene":"code","name":"Agix Code","description":"…","active":true,
      "sessions":0,"requires_project":true,"supports_team":true}]}}

// 3. 订阅 daemon 级通知(可选)
→ {"jsonrpc":"2.0","method":"daemon.subscribeEvents","id":2}
← {"jsonrpc":"2.0","id":2,"result":{"subscribed":true}}

// 4. 建会话
→ {"jsonrpc":"2.0","method":"session.create","id":3,
   "params":{"scene":"code","project_root":"/Users/x/repo-a"}}
← {"jsonrpc":"2.0","id":3,"result":{"session_id":"S1","scene":"code",
   "session_kind":"primary","project_root":"/Users/x/repo-a",
   "created_at":"2026-08-11T10:00:00Z"}}

// 5. 跑一轮。事件帧和最终响应走同一条连接。
→ {"jsonrpc":"2.0","method":"session.run_turn","id":4,
   "params":{"session_id":"S1","turn_id":"t1","message":"看看 permissions crate"}}

← {"jsonrpc":"2.0","method":"session.event","params":{"session_id":"S1","turn_id":"t1",
   "event":{"kind":"text_delta","text":"我先看一下"}}}
← {"…":"…","event":{"kind":"tool_use","id":"tu1","name":"Grep","input":{"pattern":"…"}}}
← {"…":"…","event":{"kind":"tool_result","id":"tu1","name":"Grep",
   "content":"…","is_error":false}}

// 6. 需要授权时:提问是事件,回答是一次普通调用,在同一条连接上发就行(§5.2)
← {"…":"…","event":{"kind":"prompt","prompt_type":"permission","prompt_id":"p1",
   "tool_name":"Bash","message":"运行 cargo test?","paths":[]}}
→ {"jsonrpc":"2.0","method":"session.respondToPrompt","id":5,
   "params":{"session_id":"S1","prompt_id":"p1","decision":{"type":"permit"}}}
← {"jsonrpc":"2.0","id":5,"result":{"prompt_id":"p1"}}

// 7. 结束。stop_reason 和 usage 在事件里,不在最终响应里。
← {"…":"…","event":{"kind":"turn_complete","stop_reason":"end_turn","api_calls":3,
   "usage":{"input_tokens":12345,"output_tokens":678,
             "cache_creation_input_tokens":2048,"cache_read_input_tokens":30720}}}
← {"jsonrpc":"2.0","id":4,"result":{"session_id":"S1","turn_id":"t1",
   "name":null,"api_calls":3}}

// 8. 会话还开着时:查它派生过哪些子 Agent(走磁盘,daemon 重启也有效)
→ {"jsonrpc":"2.0","method":"session.list","id":6,"params":{"parent_session_id":"S1"}}
← {"jsonrpc":"2.0","id":6,"result":{"sessions":[
   {"session_id":"S1c1","name":null,"preview":null,"message_count":0,
    "created_at":"","last_active":"","status":"inactive",
    "session_kind":"sidechain","parent_session_id":"S1","resumable":false}]}}

// 9. 读那个子 Agent 的内容 —— 和主线会话同一个 API
→ {"jsonrpc":"2.0","method":"session.history","id":7,"params":{"session_id":"S1c1"}}

// 10. 删除对话(级联删侧链),先预演
→ {"jsonrpc":"2.0","method":"session.delete","id":8,
   "params":{"session_id":"S1","dry_run":true}}
← {"jsonrpc":"2.0","id":8,"result":{"session_id":"S1","deleted":false,
   "sidechains_deleted":1,"sidechain_ids":["S1c1"]}}
```

### 11.3 错误路径

```jsonc
// A. 会话忙 —— 用户在生成过程中又点了发送
← {"jsonrpc":"2.0","id":10,"error":{"code":-32015,"message":"session is busy",
   "data":{"session_id":"S1","reason":"turn_running","current_turn_id":"t1"}}}
// 处理:先 session.interrupt 再重发。UI 应该在 turn_state != "idle" 时置灰发送键,
//       让这个错误根本不会发生。

// B. 场景需要项目但传了 null
→ {"jsonrpc":"2.0","method":"session.create","id":11,
   "params":{"scene":"code","project_root":null}}
← {"jsonrpc":"2.0","id":11,"error":{"code":-32012,
   "message":"scene `code` requires a project; pass project_root"}}
// 处理:先看 scene.list 的 requires_project,该弹项目选择框就弹。

// C. 恢复到了错的场景 —— 最该被拦住的一类
← {"jsonrpc":"2.0","id":12,"error":{"code":-32006,"message":"scene mismatch",
   "data":{"session_id":"S9","recorded_scene":"harnix","requested_scene":"code"}}}
// 处理:用 data.recorded_scene 找到对的 daemon(或先 scene.activate)重试。
//       不要当普通失败吞掉 —— 它意味着 UI 里那条会话挂在了错误的场景下。

// D. 停用场景被会话挡住
← {"jsonrpc":"2.0","id":13,"error":{"code":-32009,
   "message":"scene harnix has active sessions: s1, s2"}}
// 处理:没有结构化 data,会话 id 在 message 文本里。想稳妥就自己用 session.list
//       找出该场景的会话逐个 close 后重试。

// E. 权限提问超时 —— 注意这不是 RPC 错误
← {"…":"…","event":{"kind":"prompt","prompt_type":"permission","prompt_id":"p9",…}}
// (客户端 300 秒内没有调 session.respondToPrompt)
← {"…":"…","event":{"kind":"tool_result","id":"tu9","name":"Bash","is_error":true,…}}
← {"…":"…","event":{"kind":"turn_complete","stop_reason":"end_turn",…}}
// turn 正常走完,只是那次工具调用被拒。失败关闭,永不自动放行。
```

### 11.4 集成自检清单

- [ ] 发现逻辑用 `(pid, pid_start_time)` 联合判活,不是只比 `pid`(§2)
- [ ] 响应按 `id` 匹配,不假设先发先回(§5.2)
- [ ] `tool_result` 按 `id` 配对渲染,不按数组下标(§5.5)
- [ ] `thinking` / `redacted_thinking` 块转发时原样保留(§4.1)
- [ ] 发送按钮看 `session.get` 的 `turn_state`,不靠等 `SESSION_BUSY`(§5.3)
- [ ] `stop_reason` / `usage` 从 `turn_complete` 事件取,不是从 `run_turn` 的响应取
- [ ] 断线重连按 subscribe → history → 补帧的顺序,不颠倒(§5.6)
- [ ] 权限提问超时后 UI 能显示"已拒绝",不会一直转圈(§10)
- [ ] 实现了宿主工具的,只在自己声明过的会话里执行 `host_tool_call`,看到 `host_tool_call_resolved` 就撤下(§7.4)
- [ ] 关闭对话前想清楚要不要保留子 Agent 侧链记录 —— `session.close` 会删掉它们(§3.3)
