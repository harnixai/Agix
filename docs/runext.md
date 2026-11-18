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
| 语言 | JavaScript（ES2023），内嵌 QuickJS，**作为脚本**执行（不是模块），可以顶层 `await` |
| 生命周期 | 一次调用一个全新的运行时，跑完即弃，不留状态。状态在文件或远端服务里 |
| 何时出现 | 会话里至少有一个启用着的插件声明了 `[[command]]` 时才注册，此时常驻；没有就不注册，零开销 |
| 编译 | 要 `ptc` 与 `plugin-packages` 两个特性 |
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
writes = ["forge/**"]                # 这个命令能写的范围，相对项目根
```

| 字段 | 规则 |
|---|---|
| `name` | 1–32 个小写 ASCII 字母、数字、`-`，以字母开头。不能与内建斜杠命令重名（安装时拒） |
| `entry` | 必须在包内，解析后不经符号链接指到包外；文件必须是 UTF-8 文本的 JavaScript，至多 4 MiB。**包里带来的 QuickJS 字节码一律拒装**（§8） |
| `description` | 单行，至多 500 字符，同其它披露文本 |
| `read_only` | 子命令前缀：命令行的参数部分以其中一项开头（整词匹配）时，这次调用算只读（§6） |
| `writes` | glob，相对项目根，收窄规则同插件 skill 的 `allowed-tools` 路径：解析后必须在项目之内，`**`、`../` 越出、绝对路径不生效（`ARCHITECTURE.md` §6.1）。不写就是不写任何文件 |

**同名**：两个启用着的包声明了同名命令，按包名排序先到的保留，后到的不注册，`plugin.list` 在
那个包的 `command_conflicts` 里写明被谁占着。

**披露**：`plugin.install` 的 `disclosure.capabilities` 每个命令一条
（``adds command `forge` that may write forge/**``），`description` 进 `model_visible`。

## 3. 命令行

`command` 按 shell 的引号规则切词（单引号、双引号、反斜杠转义），第一个词是命令名，其余交给
命令。**它不是 shell**：出现 `|`、`>`、`<`、`;`、`&&`、`||`、`$(`、`` ` ``、未加引号的 `$VAR`
或 `*` 时整条拒绝，错误说明"RunExt 不是 shell，参数原样传给命令；大段内容放进 `input`"。

命令名不认识时，错误列出已装的命令。

## 4. 脚本里有什么

入口文件先被求值，再调用 `entry` 指名的函数：

```js
async function main(argv, input, ctx) {
  // argv:  ["spec", "list", "--status", "draft"]
  // input: 调用时的 input，没有就是 null
  // ctx:   { projectRoot, cwd, command: "forge", plugin: { name, version }, readOnly }
  return { items: [/* … */] };
}
```

返回字符串原样交回；返回别的按 JSON 序列化。抛出的错误以工具错误交回模型，消息就是
`error.message`——用法错误时把用法写进消息，模型据此改正。

全局与 `RunCode` 相同的那一套：

| 名字 | 说明 |
|---|---|
| `tools.<Name>(input)` / `callTool(name, input)` | 调本会话可从代码调用的工具（含 `mcp__*`），返回 Promise，失败 reject。远端数据服务走这里，连接与登录由引擎管 |
| `log(text)` / `console.*` | 进度行，宿主在任务进度里看到，末尾随结果交回 |
| `stopRequested()` | 是否被要求停下 |
| `agix.*` | 基础库：YAML 与 frontmatter、glob、路径、JSON Schema、SHA-256、UUID、参数解析（`agix_js.md`） |

**没有**：`agent()`（判断留给模型）、`notify` / `messages`（命令不在后台跑）、`RunCode`、
`RunExt`（命令里不起命令）；`require` / `import`、npm 包、Node 与浏览器 API 都是一调用就抛错的
桩，同 `RunCode`。`agix` 没有的纯 JS 库，在构建期打包进入口文件。

### 从代码里读写文件

- `tools.Read` 在代码调用时交回 `{ content, totalLines, truncated }`：原文，不带行号，不做
  "文件未变"的去重。模型直接调用 `Read` 看到的不变。
- `tools.Write` / `tools.Edit` 与模型调用时规则相同，包括"写已有文件前要读过、且读过之后没被
  改过"。命令改一个文件前先读它，正好满足。

## 5. 结果

- 返回值序列化后至多 50,000 字符，超出截断并注明。
- 给模型的是文本：字符串原样，对象以 JSON。经 `RunCode` 调用时程序拿到的是对象本身。
- 失败时结果是错误消息，加已完成的写入清单——已经发生的副作用不因失败消失。

## 6. 权限

**调用这条命令是授权点。** 权限规则按 `RunExt(<命令行>)` 写，匹配方式与 `Bash` 的前缀规则
相同：

```jsonc
"allow": ["RunExt(forge spec:*)", "RunExt(forge req list:*)"],
"ask":   ["RunExt(forge flow archive:*)"]
```

没有规则命中时：

| 调用 | 判定 |
|---|---|
| 命中 `read_only` 的 | 按只读工具：允许，plan 模式下照常可用，可与其它只读调用并行 |
| 其余（会写） | 按会话的模式：`default` 询问一次，`acceptEdits` 允许（写入范围都在项目内），无人值守时按场景的缺省 |

**调用里面的子调用**：

- 写到 `writes` 之内的 `Write` / `Edit`：这次调用已经被授权，放行；deny 规则仍然胜出。
- 写到 `writes` 之外的：按会话的判定走，要询问的**直接失败**，同 `RunCode`。
- 项目内的读取照常允许；其它工具（`Bash`、`WebFetch`、MCP 工具）按会话的判定走，询问即失败。
- 只读调用里的写入一律失败：声明了只读的子命令写了文件，是插件的缺陷，不是授权问题。

`PreToolUse` / `PostToolUse` hook、`tool.around` 脚本、中间件、遥测看得到每一次子调用。
`tool.around` 的输入带 `via`：`"model"`、`"RunCode"` 或 `"RunExt:<命令名>"`，插件的守卫脚本据此
放行自己的命令、拦下模型的直接读写（`forge.md`「护栏」）。

## 7. 执行与限额

运行时同 `RunCode`：专门的线程、单线程的 QuickJS 异步运行时、子调用经通道交给引擎执行、
`Promise.all` 真并行。前台执行，登记为一件 `ext` 类型的运行任务。

| | 上限 |
|---|---|
| 总时长 | 2 分钟 |
| 子调用总数 | 1,000 |
| 同时在跑的子调用 | 8（不可并行的写入按提交顺序一个接一个） |
| 交回脚本的单个工具结果 | 200,000 字符 |
| 返回值 | 50,000 字符 |
| 脚本内存 | 64 MB |
| 入口文件 | 4 MiB |

停止同 `RunCode`：用户中断这一轮、`TaskStop`，先置停止标志，宽限后强制打断。

## 8. 字节码

包里只收源码。QuickJS 读字节码时不做校验，一段构造过的字节码能破坏宿主进程的内存；字节码还绑定
QuickJS 的确切版本，引擎一升级就失效；安装时的披露也读不了它。

引擎在发现插件时把入口编译成字节码，缓存在本机的插件缓存目录里，键是引擎版本加入口文件的
SHA-256。之后每次调用加载缓存，跳过解析。缓存是本机生成的，引擎换版本或文件变了就重编。

## 9. 从 RunCode 调

`RunExt` 可从代码调用：

```js
const open = await tools.RunExt({ command: "forge req list --status proposed" });
for (const r of open.items) {
  await tools.RunExt({ command: `forge req update ${r.id} --status accepted` });
}
return open.items.length;
```

每一次计入 `RunCode` 的子调用限额；命令自己的子调用计入命令的限额。

## 10. 斜杠命令

用户输入 `/forge spec list`，走同一个入口：同样的解析、同样的权限（用户亲手发起，算作已批准这条
命令；`writes` 之外照样询问即失败），结果作为本地命令的输出交给宿主，**不经过模型、不进对话**。
`commands.list` 里它的 `kind` 是 `ext`。斜杠形式与 skill 同名时 skill 胜出，记进
`command_conflicts`。

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

## 12. 和其他扩展方式的关系

| 要做的事 | 用什么 |
|---|---|
| 改模型看到的东西：提示词、工具结果、请求 | `[[script]]`，九个点，不碰 I/O（`extending_quickjs.md`） |
| 维护结构化数据、按规则推进、批量处理 | `[[command]]` + `RunExt`（本文） |
| 一次性的批量活，由模型临场决定怎么做 | `RunCode`（`ptc.md`） |
| 外部系统、别的语言、要常驻进程 | `[[mcp]]` |
| 要状态、要进程级能力、要性能 | Rust 插件（`plugin_development.md`） |

## 13. 实现落点

| 内容 | 位置 |
|---|---|
| 运行时（线程、限额、取消、`tools.*` 桥）拆成 `RunCode` 与 `RunExt` 共用 | `crates/ptc` |
| `agix` 基础库 | `crates/js-lib`（`agix_js.md` §5） |
| `[[command]]` 的解析、校验、披露、同名处理、字节码缓存 | `crates/packages` |
| 注册 `RunExt`、把命令表交给它；斜杠命令 | `crates/assembly` |
| 调用级的写入授权 | `crates/permissions`，与插件 skill 的 `allowed-tools` 同一套收窄 |
| 代码调用的 `Read` 交回原文、不去重 | `crates/tools/src/file_read.rs`（看 `ToolContext::from_code`） |
| `tool.around` 输入加 `via` | `crates/script-host/src/adapters/tool_around.rs` |
