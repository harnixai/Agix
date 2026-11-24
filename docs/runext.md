# RunExt：插件命令

`RunExt` 运行插件包声明的**命令**：模型写一行命令行（`forge spec list --status draft`），引擎
找到声明它的插件，在内嵌的 QuickJS 里执行插件带来的 JavaScript，把结果交回对话。

它是给"机械的、按规则就能做完的活"准备的：维护结构化数据、校验、统计、按状态机推进、批量改。
这些活交给模型，模型要读一堆文件、在上下文里算、再一段段写回去；交给命令，模型只发一行、拿回
结果。判断与写作留给模型，规则与搬运交给程序。

和 `RunCode`（`ptc.md`）是一对：`RunCode` 跑模型现写的程序，`RunExt` 跑装好的程序。两者共用
一套运行时和脚本 API，入口、信任与授权各自独立。

## 1. 形态

| | |
|---|---|
| 工具 | `RunExt`，一个。装了多少插件、多少命令都是这一个 |
| 语言 | JavaScript（ES2023），内嵌 QuickJS，**作为脚本**执行（不是模块），可以顶层 `await`：入口文件包在一个异步函数里求值，所以它的顶层声明不是 `globalThis` 的属性 |
| 生命周期 | 一次调用一个全新的运行时，跑完即弃，不留状态。状态在文件或远端服务里 |
| 何时出现 | 会话里至少有一个启用着的插件声明了 `[[command]]` 时才注册，此时常驻；没有就不注册，零开销。会话建好时的命令表为准，子 Agent 继承它（仍受 agent 类型的工具白名单约束） |
| 编译 | 要 `ptc` 与 `plugin-packages` 两个特性；两者都在时 `daemon.ping` 的能力位有 `runext` |
| 场景 | 场景的工具白名单照常生效；`code`、`harnix`、`mini` 都放行 `RunExt` |

### 参数

```jsonc
{ "command": "forge spec list --status draft",  // 命令行
  "input": { … },                               // 可选：大段正文、批量数据，命令自己约定形状
  "description": "…" }                          // 可选：一句话，任务列表里显示
```

### 工具描述

描述只写用途和已装命令的清单，每个命令一行，用法让模型自己取：

```
Run a command an installed plugin provides. Write it as a command line; long text or
batches go in `input`. `<command> help` prints its usage. This is not a shell: no
pipes, redirection, variables or globbing.

Installed:
  forge — 需求、规格与变更流程
```

## 2. 声明命令

```toml
[[command]]
name = "forge"                       # 命令名；模型写 `forge …`
entry = "forge.js:main"         # <文件>:<函数>，文件相对插件根目录，与 [[script]] 同一种写法
description = "需求、规格与变更流程"   # 进工具描述——安装时披露
read_only = ["help", "req list", "req get", "spec list", "spec get", "spec coverage",
             "flow show", "flow next"]
writes = ["forge/**"]                # 这个命令经文件工具能写的范围，相对项目根
```

| 字段 | 规则 |
|---|---|
| `name` | 1–32 个小写 ASCII 字母、数字、`-`，以字母开头。不能与内建斜杠命令重名（安装时拒） |
| `entry` | 必须在包内，解析后不经符号链接指到包外；文件必须是 UTF-8 文本的 JavaScript（不含 NUL），至多 4 MiB。**包里带来的 QuickJS 字节码一律拒装**（§8）。`<函数>` 是 JavaScript 标识符 |
| `description` | 单行，至多 500 字符，同其它披露文本 |
| `read_only` | 子命令前缀：命令行的参数部分以其中一项开头（整词匹配）时，这次调用算只读（§6）。空串 `""` 只匹配不带参数的调用（只敲命令名）；只有空白的前缀安装时拒 |
| `writes` | 命令经文件工具（`Write`、`Edit`、`NotebookEdit`）能写的范围，glob，相对项目根，收窄规则同插件 skill 的 `allowed-tools` 路径：解析后必须在项目之内，`**`、`../` 越出、绝对路径不生效（`ARCHITECTURE.md` §6.1）。配置路径（`.git/**`、`.agix/agents/**` 之类）写进来也不生效（§6）。不写就是经文件工具不写任何文件。安装时就拒绝绝对路径、`~`、带 `..` 的和第一段就是通配的（`**`、`*.md`）；用到时再按会话的项目解析一次，经符号链接越出项目的那一条不生效 |

**同名**：两个启用着的包声明了同名命令，按包名排序先到的保留，后到的不注册，`plugin.list` 在
那个包的 `command_conflicts` 里写明被谁占着（`{"command", "shadowed_by": "plugin", "plugin"}`）。
同一个包里声明两次同名命令拒装。

**披露**：`plugin.install` 的 `disclosure.capabilities` 每个命令一条
（``adds command `forge` that may write forge/** through the file tools; its other calls (Bash, MCP,
web) get what the session's rules allow without asking``，不写文件的把 `may write …` 换成
`writes no files`：`writes` 只约束文件工具，§6），
`description` 进 `model_visible`（来处 ``command `forge` description``），`disclosure.commands`
列出命令名。`plugin.list` 每个包带 `commands`（`name`、`description`、`read_only`、`writes`）。

## 3. 命令行

`command` 按 shell 的引号规则切词（单引号、双引号、反斜杠转义），第一个词是命令名，其余交给
命令。**它不是 shell**：未加引号的 `|`、`>`、`<`、`;`、`&`（含 `&&`、`||`）、`` ` ``、通配符
`*`、`?`、`[`、换行，以及后面紧跟着任何非空白字符的 `$`（`$VAR`、`${…}`、`$(…)`、`$?`、`$5`、
`$$`、`$'…'` 都算）整条拒绝；双引号里也一样拒绝 `` ` `` 与这样的 `$`（双引号里的 `?`、`[` 原样），
单引号里一切原样。单独的 `$`（后面是空白或行尾，双引号里也可以是收尾的 `"`）照收。错误说明"RunExt 不是 shell，参数原样传给
命令；大段内容放进 `input`"。

命令名不认识时，错误列出已装的命令。这两种错误在权限检查之前报（输入校验），不会去问人。

权限规则匹配的是切好的词拼成的一行：不带引号，词里词间的每一段空白都算一个空格
（`forge flow 'archive '`、`forge  flow "archive"` 都按 `forge flow archive` 匹配），同一组词怎么
加引号都撞得上同一条规则。询问时给人看的是带引号的写法（`forge req add 'two words'`）。

## 4. 脚本里有什么

入口文件先被求值，再调用 `entry` 指名的函数：

```js
async function main(argv, input, ctx) {
  // argv:  ["spec", "list", "--status", "draft"]
  // input: 调用时的 input，没有就是 null
  // ctx:   { projectRoot, cwd, command: "forge", plugin: { name, version }, readOnly }
  //        projectRoot 与 cwd 都是会话的工作目录（项目，或会话所在的 worktree）；ctx 是冻结的
  return { items: [/* … */] };
}
```

入口文件里没有这个函数（顶层声明的或挂在 `globalThis` 上的都认）时报 ``forge.js defines no
function `main` ``。

返回字符串原样交回；返回别的按 JSON 序列化。抛出的 `Error` 以工具错误交回模型，消息就是
`error.message`——用法错误时把用法写进消息，模型据此改正。别的异常（`TypeError`、
`ReferenceError`……）是插件的缺陷，交回 `<名字>: <消息>` 加调用栈；抛出的字符串原样交回。

全局与 `RunCode` 相同的那一套：

| 名字 | 说明 |
|---|---|
| `tools.<Name>(input)` / `callTool(name, input)` | 调本会话可从代码调用的工具（含 `mcp__*`），返回 Promise，失败 reject。远端数据服务走这里，连接与登录由引擎管 |
| `log(text)` / `console.*` | 进度行，宿主在任务进度里看到，末尾随结果交回 |
| `stopRequested()` | 是否被要求停下 |
| `agix.*` | 基础库：YAML 与 frontmatter、glob、路径、JSON Schema、SHA-256、UUID、参数解析（`agix_js.md`）。全局名是引擎名，改了引擎名的嵌入方里没有 `agix`（`agix_js.md` §2） |
| `<宿主库>.*` | 包顶层 `uses` 声明的宿主库，如 `codix.flow.start(input)`，见下面「宿主库」；没声明的包里没有 |

**没有**：`agent()`（判断留给模型）、`notify` / `messages` / `args`（命令不在后台跑，参数走
`main`）、`RunCode`、`RunExt`（命令里不起命令）——`callTool("Agent" | "RunCode" | "RunExt", …)`
直接 reject；`require` / `import`、npm 包、Node 与浏览器 API 都是一调用就抛错的
桩，同 `RunCode`。`agix` 没有的纯 JS 库，在构建期打包进入口文件。

### 宿主库

嵌入引擎的宿主可以给插件代码提供 JavaScript 库（`agix-sdk.md` §10「宿主 JS 库」）。包顶层声明
`uses = ["codix"]`（`plugin_packages.md`「宿主库」），命令的运行时里就有全局 `codix`，函数都是返回 Promise 的
`codix.<路径>(input)`，JSON 进、JSON 出：

```js
async function main(argv, input, ctx) {
  try {
    return await codix.flow.start({ change: argv[0] });
  } catch (e) {
    if (e.code === "denied") throw new Error(`cannot start a flow here: ${e.message}`);
    throw e;
  }
}
```

- 宿主返回错误时 reject 一个 `Error`，带 `code` 与 `message`；引擎自己的拒绝是 `limit`（输入或返回值超过单个工具
  结果的上限 200,000 字符，或嵌套超过 128 层）、`stopped`（命令被停）。命令被停时宿主收到取消，引擎不等它。
- 宿主库的调用不是工具调用：不占子调用名额，不经权限判定，`tool.around` 与 hooks 看不到它。宿主看调用上下文（包、
  命令、会话、`via`……）自己决定放不放行。
- 每次调用在任务输出里记一行：``codix.flow.start ok 12ms``、``codix.flow.start error denied 3ms``。
- 宿主库全局与基础库一样冻结、不可替换；宿主的 prelude（便利封装）在命令代码之前已经求值好。
- `RunCode` 的程序与 `[[script]]` 的九个点里没有宿主库；`[[function]]` 的运行时里有（§12）。

### 从代码里读写文件

- `tools.Read` 在代码调用时交回 `{ content, totalLines, truncated }`：原文，不带行号，不做
  "文件未变"的去重。超出单个结果的上限（§7）时仍是这个对象：`content` 截到放得下的整行，
  `truncated` 为真，余下的用 `offset` 再读。模型直接调用 `Read` 看到的不变。
- `tools.Write` / `tools.Edit` 与模型调用时规则相同，包括"写已有文件前要读过、且读过之后没被
  改过"。命令改一个文件前先读它，正好满足。

## 5. 结果

- 返回值序列化后至多 50,000 字符，超出截断并注明。
- 给模型的是文本：字符串原样，对象以 JSON；命令 `log` 过的，末尾跟一段 `Log:`（最后 4,000
  字符）。经 `RunCode` 调用时程序拿到的是值本身（对象，或返回的那个字符串），没有 `Log:`。
- 失败时结果是错误消息，加 `Already done before it failed` 清单——这次运行里成功了的、不可
  并行的子调用（写入都是），每条一行——已经发生的副作用不因失败消失；再跟 `Log:`。

## 6. 权限

**调用这条命令是授权点。** 权限规则按 `RunExt(<命令行>)` 写，匹配方式与 `Bash` 的前缀规则
相同：

```jsonc
"allow": ["RunExt(forge spec:*)", "RunExt(forge req list:*)"],
"ask":   ["RunExt(forge flow archive:*)"]
```

规则按子命令的路径写，匹配的是命令名后面的词序（§3）。引擎不解析命令的选项：写在子命令前面的
选项（`forge --x flow archive`）会让 `RunExt(forge flow archive:*)` 匹配不到，所以命令不应在子命令
之前收选项——这是写命令的约定（forge 不收），不是引擎替它守的。

没有规则命中时：

| 调用 | 判定 |
|---|---|
| 命中 `read_only` 的 | 按只读工具：允许，plan 模式下照常可用，可与其它只读调用并行 |
| 其余（会写） | 按会话的模式：`default` 询问一次，`acceptEdits` 允许（写入范围都在项目内），无人值守时按场景的缺省 |

**调用里面的子调用**：

- 写到 `writes` 之内的 `Write` / `Edit` / `NotebookEdit`：这次调用已经被授权，放行——调用级的
  授权回答的是模式要问的那个问题（与委托给子 Agent 的 `allowed-tools` 同一套），deny 规则、ask
  规则、凭据与受保护路径仍然胜出。这份授权只给这次调用自己的子调用，模型自己的调用看不到。
- 写到 `writes` 之外的（包括经符号链接写出去的）：**直接失败**，不去问会话。会话对项目内的写入本来不问，交给会话判定的话，`writes` 之外、项目之内的写入会成功，
  `writes` 就不设限了，「不写就是不写任何文件」与披露里的 `may write …` 也不成立。
- 写到配置路径的：**直接失败**，不论 `writes` 怎么写、这次调用是否已被批准。配置路径是会话
  会当作配置来问的那些（`.git/` 里的一切，含 hooks 与 config；`.agix/` 等配置根里工作区以外的
  条目；项目的 skill 目录），按写入路径的每一种写法（含经符号链接解析后的）判断。命令是装好后
  无人盯着运行的代码，写一个 `.git/hooks/pre-commit` 就是在下一次提交时执行代码；要改配置，
  由模型自己发起那次写入，照常询问。子 Agent 里跑的命令同样如此。别的工具会执行的文件——
  `.husky/` 里的钩子、`.vscode/tasks.json`、`Makefile`、`package.json` 的 scripts——不是配置路径，
  `writes` 写进来就能写；`.envrc` 例外，它对任何写入都受保护（不论谁写都被拒，除非
  `files.allow_write` 放开）。装包时看 `writes`，要把这类路径当作"能让别的程序执行代码"来看。
- 项目内的读取照常允许；其它工具（`Bash`、`WebFetch`、MCP 工具）按会话的判定走。
  所以 `writes` 是文件工具的边界，不是命令能做什么的边界：会话里有 `Bash` 的 allow 规则（或场景
  不问就放行的命令），命令经 `Bash` 能写的就不限于 `writes`；沙箱照常约束那条命令。
- 只读调用里的写入一律失败：子调用写文件，或跑一条不是只读的命令，就失败——声明了只读的子
  命令写了文件，是插件的缺陷，不是授权问题。其它工具（MCP 工具等）在只读调用里照常按会话的
  判定走（走远端后端的只读子命令，如 `forge req list`，不该因此被拦），plan 模式下由 plan 模式拦下。

**子调用要问的时候**：会话有人值守、这次 `RunExt` 是模型或用户（`/name args`）发起的，子调用里
会话要问的——命中 ask 规则的写入、`default` 模式下会改东西的 `Bash`、MCP 工具——照常发
`prompt_type:"permission"` 的提问，和模型自己调用时一样（子 Agent 里经父会话的宿主问）：

- 提问帧带 `origin`：`{"via": "RunExt:<命令名>", "tool_use_id": <这次 RunExt 的>, "command": <命令行>}`，
  宿主可以按命令而不是按模型来判。
- 不提供"总是允许"（`permit_always: false`），答了 `permit_always` 也只放行这一次、什么都不记：
  这份允许是这次命令的，不该变成模型自己的调用也能用的 allow。
- 拒绝或超时，这个子调用失败，命令拿到错误；这次运行之后的子调用不再问，要问的直接失败——
  命令不能靠捕获错误再问一遍来磨过一次"不"。等待算在这次 `RunExt` 的时限里，时限到了或这一轮
  被打断，提问撤回（`prompt_resolved`）。
- 只转权限提问。子调用里的 MCP server 提问（`mcp_elicitation`）、嵌套仓库要不要挪开这类问题照旧
  不送到宿主，和程序里一样。`PermissionRequested` 与 `Notification` hook 照常触发，前者可以像对
  模型的调用那样批准。
- 上面列的直接失败的情形（`writes` 之外、配置路径、只读调用里的写入）和 deny 规则在提问之前
  判定，不会被问到。

无人值守的会话照旧询问即失败，宿主经 `plugin.call` 调的函数（§12）也一样。宿主从 `daemon.ping`
的 `runext_subcall_prompt` 能力位看出引擎支持这一条。

从 `RunCode` 里调 `RunExt` 时，那次 `RunExt` 调用本身也是程序的子调用：会写的子命令要询问就
失败，要有 `RunExt(...)` 的 allow 规则或 `acceptEdits`；它的子调用同样不问，询问即失败。

`PreToolUse` / `PostToolUse` hook、`tool.around` 脚本、中间件、遥测看得到每一次子调用。
`tool.around` 的输入带 `via`：`"model"`、`"RunCode"` 或 `"RunExt:<命令名>"`，`via` 是后者时
`plugin` 是命令所在的包名（命令名可能被别的包占了，守卫要两样都核对），`projectRoot` 是会话的
工作目录（没有项目时为 `null`）。插件的守卫脚本据此放行自己的命令、拦下模型的直接读写
（`forge.md`「护栏」）。另有两个值：用户敲的 `/name` 那次
`RunExt` 调用本身是 `"user"`（模型写出的 `/name` 是 `"model"`，§10），skill 的内联命令是
`"skill"`。

## 7. 执行与限额

运行时同 `RunCode`：专门的线程、单线程的 QuickJS 异步运行时、子调用经通道交给引擎执行、
`Promise.all` 真并行。前台执行，登记为一件 `ext` 类型的运行任务（`detail` 带 `command`）。
与 `RunCode` 共用"同时在跑的脚本"那份进程级名额；只有模型在根会话里直接发起的才占一个，经
`RunCode` 或在子 Agent 里跑的不占。可以与其它只读调用并行的，只有只读调用。

| | 上限 |
|---|---|
| 总时长 | 2 分钟 |
| 子调用总数 | 1,000 |
| 同时在跑的子调用 | 8（不可并行的写入按提交顺序一个接一个） |
| 交回脚本的单个工具结果 | 200,000 字符 |
| 返回值 | 50,000 字符 |
| 脚本内存 | 64 MB |
| 入口文件 | 4 MiB |
| 子 Agent | 0（不能起） |

停止同 `RunCode`：用户中断这一轮、`TaskStop`，先置停止标志，宽限后强制打断。

## 8. 字节码

包里只收源码。QuickJS 读字节码时不做校验，一段构造过的字节码能破坏宿主进程的内存；字节码还绑定
QuickJS 的确切版本，引擎一升级就失效；安装时的披露也读不了它。

引擎在一个入口**第一次运行时**把它编译成字节码（不在发现插件时：发现在 `crates/packages`，那里不跑 QuickJS，
而每次重新扫描都去编译用不到的命令只是白花启动时间），缓存在这一层插件缓存的 `.bytecode/` 目录里
（`<层>/plugins/cache/.bytecode/<sha256>.qjsbc`），文件名是引擎版本、QuickJS 构建、入口文件名
与要编译的源码的 SHA-256。QuickJS 构建是它的版本号加上它为一段固定探测脚本写出的字节码的摘要：
同一个引擎版本换了 rquickjs（开发时 `cargo update`、SDK 嵌入方自己的 lockfile），操作码或预定义
atom 的编号可能不同，各用各的文件。之后每次调用加载缓存，跳过解析。缓存是本机生成的，引擎或
QuickJS 换了、文件变了就是另一个文件；读不了的缓存文件重编并覆盖。写入先写到一个本次独占新建的
临时文件（名字带进程号与序号）再改名，同一进程里并发的首次运行、并发的进程都不会装上别人写了
一半的文件；崩溃留下、超过一小时的临时文件在下次存入时清掉。

文件名是可以算出来的，所以挡住"有人预先放一个文件"的是这个目录只属于当前用户：

- 目录建成 0700，文件 0600。
- （Unix）读之前要求目录与文件都属于当前用户、组与其他人都没有任何权限（`mode & 0o077 == 0`），
  文件不是符号链接；不满足就当没有缓存，重新编译。不满足的目录不往里写，不属于当前用户的文件
  不覆盖。
- 会话里跑的东西碰不到它：沙箱里的 `Bash` 写不了状态根（插件缓存在状态根里），`Write` 写
  `<状态根>/plugins/**` 按配置路径询问，`acceptEdits` 下也问。
- 目录只留最近用过的 64 个文件（每次读到会刷新修改时间），存新文件时删掉更旧的。

包里只收源码，靠的是入口检查：带 NUL 或不是 UTF-8 的入口安装时拒装、发现时不加载。

## 9. 从 RunCode 调

`RunExt` 可从代码调用：

```js
const open = await tools.RunExt({ command: "forge req list --status proposed" });
for (const r of open.items) {
  await tools.RunExt({ command: `forge req update ${r.id} --status accepted` });
}
return open.items.length;
```

每一次计入 `RunCode` 的子调用限额；命令自己的子调用计入命令的限额。会写的子命令从程序里调
要事先允许（§6 末）。

## 10. 斜杠命令

用户输入 `/forge spec list`，走同一个入口：同样的解析、同样的权限（用户亲手发起，算作已批准这条
命令：本来要问的不再问；deny 规则、plan 模式的拒绝照样生效；`writes` 之外照样失败），结果作为
本地命令的输出交给宿主（`text_delta`，这一轮以 `command_executed` 结束），**不经过模型、不进
对话**。`commands.list` 里它的 `kind` 是 `ext`、`source` 是 `plugin`；场景的工具表里没有 `RunExt` 时
（不带 `session_id` 看 daemon 的场景，带了看那个会话的）不列。斜杠形式与 skill 同名时
skill 胜出，记进 `command_conflicts`（`{"command", "shadowed_by": "skill", "path"}`）；
`RunExt` 照样能跑这条命令。命令名不能取内建斜杠命令（`help`、`skills`、`clear`、`compact`、
`cost`）的名字，安装时拒。

只有人写的那一轮才算用户亲手发起：根会话里宿主送来的消息（`session.send` 等）。一轮的正文出自
模型或引擎时——子 Agent 的 prompt、团队成员收到的消息、发给后台 Agent 的消息、cron 与定时任务
触发的、goal 的自动轮——`/name` 照样解析、照样跑，但算模型自己的 `RunExt` 调用：按 §6 判定，
该问就问（子 Agent 里经父会话的宿主问），无人值守按场景的缺省；`tool.around` 看到的 `via` 是
`"model"`。否则模型写一句 `Agent({prompt: "/notes add …"})` 就能绕过 ask 规则。

## 11. 编写与打包

**不用构建。** YAML、校验、glob、参数解析都在 `agix` 里，大多数命令就是一个普通的 `.js` 文件，
写完打成 zip 就能装，作者和用户都不需要 Node：

```
my-plugin/
├── plugin.toml
├── my-command.js        # 顶层的 function main(argv, input, ctx)
└── types/               # runext.d.ts（全局与 ctx）、agix.d.ts（基础库），从 examples/plugins/forge 复制
```

文件头写 `// @ts-check` 并引用 `types/` 里的声明，编辑器就有类型提示与检查。

**要用 npm 上的库时**才需要构建：用 esbuild 之类把入口和依赖打成一个文件（IIFE、无外部依赖）。
打包产物里函数不在顶层，入口要显式挂到全局：`globalThis.main = main`。

打包与安装见 `plugin_packages.md`「打包与安装」：`zip -r` 加 `shasum -a 256`。开发时反复装同一个
版本，`plugin.install` 带 `checksum`，压缩包变了就替换。

完整的样例是 `examples/plugins/forge`，设计见 `forge.md`。

## 12. 插件函数

`[[command]]` 给模型和用户用；`[[function]]` 只给**宿主**用：嵌入引擎的程序按名字调包里的一个函数，
传结构化的 `input`，拿回结构化的结果。函数不进模型的工具表，也不进 `RunCode` 能调的表；要模型也能用，
另声明一个 `[[command]]`。

```toml
[[function]]
name = "check"                       # 调用名 <包名>.<name>，如 xflow.check
entry = "xflow.js:check"             # 同 [[command]] 的写法与校验
description = "门禁检查"              # 单行，至多 500 字符
writes = []                          # 同 [[command]].writes；缺省不写
input_schema = "schemas/check.json"  # 可选：包里的 JSON Schema，调用前校验 input
```

| 字段 | 规则 |
|---|---|
| `name` | 同命令名：1–32 个小写 ASCII 字母、数字、`-`，以字母开头。不经 `/name` 调，所以内建斜杠命令的名字也能用。同一个包里重名拒装；不同包之间不冲突，调用名带包名 |
| `entry` | 同 `[[command]]`：包内、不经符号链接出包、UTF-8 源码、至多 4 MiB，字节码拒装；编译结果与命令同一套缓存（§8） |
| `description` | 单行，非空，至多 500 字符 |
| `writes` | 同 `[[command]]`（§2、§6）：只在宿主让这次调用写时生效，且只写得了这里列的范围，配置路径照样不让写 |
| `input_schema` | 可选。包内的文件（规则同 `entry`），至多 64 KiB；安装时读成 JSON，并按 JSON Schema 编译，读不了、不是 JSON、编译不过的拒装 |

入口函数的签名是 `async function check(input, ctx)`。

**披露**：`disclosure.capabilities` 每个函数一条：``exposes function `xflow.check` to the host;
may write reports/**``，不写文件的是 ``…; writes no files``；`disclosure.functions` 列出调用名。函数的
`description` 不到达模型，不进 `model_visible`。`plugin.list` 每个包带 `functions`（`name`、
`description`、`writes`、`has_input_schema`）。

包顶层的 `uses`（`plugin_packages.md`「宿主库」）同样管函数：函数的运行时里装着包声明的宿主库。

### 调用

宿主按调用名调：协议上是 `plugin.call`（列表 `plugin.functions`、取消 `plugin.cancel`，见
`daemon_rpc_protocol.md` §6.5），Rust 上是 `SessionPool::call_plugin(PluginCall, CancellationToken)`
（`agix-sdk.md` §8）。只接受调用名与结构化的 `input`，没有命令行。

```js
async function check(input, ctx) {
  // input: 宿主给的 input（有 input_schema 时已校验过），没给是 null
  // ctx:   { via: "host", projectRoot, cwd, plugin: { name, version },
  //          function: "xflow.check", session: <会话 id 或 null>, readOnly }   冻结的
  return { passed: true };
}
```

返回值原样（字符串或可序列化的值）交回宿主：`{ ok: true, value, logs }`；`logs` 是 `log` / `console.*`
记下的行。失败是 `{ ok: false, error: { code, message }, logs }`：`invalid_input`（不符合 `input_schema`，
没跑）、`refused`（管线没让跑）、`answered_by_middleware`（`tool.around` 用 `respond` 替函数作答，
或 `tool.result` 改写了结果——宿主拿到的只会是函数自己的结果或失败）、`threw`（抛了错；`message` 是
错误消息，再跟已经做了的改动）、`timeout`、`cancelled`、`stopped`、`not_started`。

- **一次内部工具调用**：调用以 `PluginCall {function, input}` 走会话的整条工具管线——权限、
  `PreToolUse`、`tool.around`（`via: "host"`）、执行、`PostToolUse`、遥测；它不在模型的工具表里，
  也不进对话。权限规则写 `PluginCall(xflow.check)`。
- **调用是宿主的批准**：会话要问的视同允许（部署的托管层规定只能本人答的除外，那一类不被宿主调用答复），deny 规则、plan 模式照拦。`plan` 模式下只读的调用
  照常跑。
- **子调用**同命令的子调用（§6），`tool.around` 看到 `via: "PluginCall:xflow.check"`：宿主没让写
  （缺省）时经文件工具的写入、会改东西的命令（`Bash` 之类被判为写的）一律失败；让写时经文件工具只写得了 `writes`
  之内，配置路径照样不让写；命令的写入不受 `writes` 限制，按会话与这次调用的规则走（同 §6，`writes` 是文件工具的边界）。
  MCP 工具、`WebFetch` 这类不经文件工具也不跑命令的调用同样按规则走，对外的副作用由那些规则管。**要问人的一律失败**，
  错误说明哪次调用、为什么；要放行就在这次调用的 `permission_rules` 里写规则（`daemon_rpc_protocol.md` §4.6）。函数里也不能调
  `Agent` / `RunCode` / `RunExt`。
- **在哪跑**：带会话时用那个会话的工具、MCP、规则、目录与信任，不开新一轮，可以与正在跑的一轮并发，
  调用是会话的一件 `ext` 任务（`session.tasks` 看得到，`session.stopTask` 能停），调用没完时会话不算
  空闲，不被重建、回滚或驱逐，反过来会话正在重建或回滚时也不起调用（`SESSION_BUSY`）；不带会话时在给定目录下用场景的工具与这个目录的规则，在一个不写日志、
  不进会话列表、不连 MCP 的临时引擎上跑完即收，引擎关闭时取消。
- **限额**：与 `RunCode` / `RunExt` 共用同时运行脚本的名额（不带会话时先等到名额再建临时引擎），时限缺省两分钟、宿主可给到至多十分钟（`timeout_ms`，与 `Bash` 的上限一致；函数里 `tools.Bash` 的时限仍受这次调用的剩余时间约束），其余同命令（§7）。到时限时调用被叫停，至多再等 2 秒收尾就答 `timeout`，没收完的
  （如 hook）在后台跑完。

## 13. 和其他扩展方式的关系

| 要做的事 | 用什么 |
|---|---|
| 改模型看到的东西：提示词、工具结果、请求 | `[[script]]`，九个点，不碰 I/O（`extending_quickjs.md`） |
| 维护结构化数据、按规则推进、批量处理 | `[[command]]` + `RunExt`（本文） |
| 宿主程序按名字调包里的一段逻辑 | `[[function]]`（§12） |
| 一次性的批量活，由模型临场决定怎么做 | `RunCode`（`ptc.md`） |
| 外部系统、别的语言、要常驻进程 | `[[mcp]]` |
| 要状态、要进程级能力、要性能 | Rust 插件（`plugin_development.md`） |

## 14. 实现落点

| 内容 | 位置 |
|---|---|
| 运行时（线程、限额、取消、`tools.*` 桥）拆成 `RunCode` 与 `RunExt` 共用 | `crates/ptc` |
| 宿主库：类型 | `base::interface::host_js` |
| 宿主库：按 `uses` 选库、桩与 prelude、调用的上限、取消与观测 | `crates/ptc`（`host_libs.rs`，桥在 `engine.rs`，分发在 `calls.rs`） |
| 宿主库：启动时的命名校验、交给会话（`Builder::host_js_libs`，子 Agent 经 `HostContracts` 继承） | `crates/assembly`（`plugins/host_libraries.rs`）、`crates/runtime` |
| `agix` 基础库 | `crates/js-lib`（`agix_js.md` §5） |
| `[[command]]`、`[[function]]`、`uses` 的解析、校验、披露、同名处理、字节码缓存 | `crates/packages` |
| 函数表（`PluginSubsystem::functions`）、宿主自带的包 | `crates/assembly`（`plugins/`） |
| 命令表交给会话（`crates/assembly` 把它交给 `Builder::ext_commands`，子 Agent 经 `HostContracts` 继承） | `crates/runtime`、`crates/assembly` |
| 注册 `RunExt`（会话最后的工具表里有它才有 `/name`）；斜杠命令 | `crates/runtime` |
| 调用级的写入授权 | `crates/permissions`（`write_scope`、`Reach::EditsProject`），与插件 skill 的 `allowed-tools` 同一套收窄；子调用的拦截在 `crates/runtime`（`Caller::Command`） |
| 代码调用的 `Read` 交回原文、不去重 | `crates/tools/src/file_read.rs`（看 `ToolContext::from_code`） |
| `tool.around` 输入加 `via`、`plugin`、`projectRoot` | `crates/script-host/src/adapters/tool_around.rs` |
