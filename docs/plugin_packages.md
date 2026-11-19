# 插件包

插件包是一个带 `plugin.toml` 的目录,打成 zip 安装。它本身不带要跑的代码运行时:包里
声明的东西——MCP 服务器、绑到扩展点上的 QuickJS 脚本、一个声明式场景、agent 类型、一组
skill、经 `RunExt` 跑的命令、给宿主调的函数——都由引擎里已有的部分兑现。安装时,一切会到达模型的文本都摆到安装者面前。
嵌入引擎的宿主还可以带自己的包(「宿主自带的包」),并给包里的 JavaScript 提供库(「宿主库」)。

包方案是 `daemon` 的 `plugin-packages` feature,默认开启。不开时 `plugin.*` 这些 RPC 返回
`PLUGINS_DISABLED`(`-32016`)。脚本怎么写见 `extending_quickjs.md`;引擎的全部扩展点见
`extension_points.md`。一个完整的包在 `examples/plugins/quickjs/`,开发流程见
`plugin_development.md`。

## `plugin.toml`

```toml
# 包里的 JavaScript 要用的宿主库(宿主嵌入引擎时提供);TOML 里顶级键要写在第一个段之前。
uses = ["codix"]               # 可选;见下面「宿主库」

[plugin]
name = "github-tools"          # 必填
version = "1.2.0"              # 必填
api_version = "0.1"            # 必填;必须是这个构建实现了的版本
requires_engine = ">=0.3.12, <0.4"  # 可选;引擎版本要满足的 semver 条件
description = "GitHub tools"   # 会到达模型——安装时披露
author = ""
homepage = ""

# ── 依赖 ──
# 这个包要求同一层 cache 里已装的其他包;值是 semver 版本条件。
[dependencies]
base-tools = "^1.2"            # 也可以写 ">=1, <2"

# ── 脚本 ──
# 跑它的是进程里已有的 QuickJS 载体;包只发 JavaScript 并指名一个扩展点。
[[script]]
point = "tool.result"                   # 扩展点 id
entry = "scripts/annotate.js:onResult"  # <文件>:<函数>,文件相对插件根目录
timeout_ms = 100                        # 可选;默认 100,来自包的封顶 1000
calls_per_turn = 1000                   # 可选;默认 1000,来自包的不能更高

# ── MCP 服务器 ──
[[mcp]]
name = "github"
kind = "native"                # 必须给 `config`
config = "mcp/github.json"     # stdio server 以插件根目录为工作目录启动,相对的 command 与路径在那里解析;服务哪个项目见「包里的 MCP 服务器」

# ── 插件自己拥有的场景 ──
# 一个声明式场景目录(scene.json + prompt.md),格式见 extension_points.md「声明式场景」;
# scene.json 的 id 必须是 "plugin:<name>"。
[scene]
dir = "scene"

# ── agent 类型 ──
[[agent]]
name = "pr-reviewer"
description = "Reviews PRs"    # 会到达模型——披露
prompt = "agents/reviewer.md"  # 会到达模型——披露
allowed_tools = ["Read"]
disallowed_tools = []
model = "claude-opus-5"
permission_mode = "plan"
effort = "high"
max_turns = 30
scene = "plugin:github-tools"  # 让这个 agent 的子代理跑在插件的场景里

# ── skill ──
# 一个目录,里面每个 skill 一个子目录:skills/<名字>/SKILL.md,格式与磁盘上的 skill 相同,
# 附带的参考文件放在同一个子目录里。只有写了这一段的包才带 skill(见下面「包里的 skill」)。
[skills]
dir = "skills"

# ── 命令 ──
# 模型经 `RunExt` 调、用户敲 `/forge …` 的命令,JavaScript 跑在引擎的 QuickJS 里;见下面「包里的命令」。
[[command]]
name = "forge"                       # 1–32 个小写字母、数字、`-`,字母开头,不与内建斜杠命令重名
entry = "forge.js:main"              # <文件>:<函数>,文件相对插件根目录,必须是源码
description = "需求、规格与变更流程"   # 单行,进 RunExt 的工具描述——披露
read_only = ["", "help", "spec list"]  # 参数以这些整词前缀开头的调用算只读;"" 是不带参数的调用
writes = ["forge/**"]                # 能写的范围,glob 相对项目根;不写就是不写文件

# ── 函数 ──
# 只给宿主调的函数(`github-tools.check`),不进模型的工具表;见下面「包里的函数」。
[[function]]
name = "check"                       # 名字规则同命令名;同一个包里不重名
entry = "forge.js:check"             # 同 [[command]] 的 entry,必须是源码
description = "门禁检查"              # 单行,至多 500 字符——披露
writes = []                          # 同 [[command]] 的 writes;宿主让它写时也只能写这些
input_schema = "schemas/check.json"  # 可选;包里的 JSON Schema,调用前校验 input

# ── 写给宿主的段 ──
# `x-` 开头的顶级段:引擎不读、不执行、不警告;宿主自己约定它的形状。
[[x-ui]]
panel = "ui/panel.js"
```

### `api_version` 与 `requires_engine`

两个字段回答的问题不同:

- **`api_version`** 是插件契约的代际:破坏兼容的改动换一代,一个构建只认它实现了的那几代
  (`packages::manifest::SUPPORTED_API_VERSIONS`)。没有默认值,漏掉它解析不过;写了这个构建
  没实现的代际会被拒,消息里给出支持的集合。
- **`requires_engine`** 是包用到的增量能力从哪个引擎版本起才有,比如某个新的脚本点。可选,
  不写就只受 `api_version` 约束。写法是 semver 版本条件(`">=0.3.12"`、`"^0.3.12"`、
  `">=0.3.12, <0.4"`)。

比较时引擎版本只取 `x.y.z`:发布版本号带日期序号(`0.3.12-2026111001`),semver 会把这段
当成 pre-release,`0.3.12-2026111001` 排在 `0.3.12` 前面,`>=0.3.12` 就会误判不满足。同一
个原因,条件里写 pre-release(`>=0.3.12-2026111001`)的直接拒,不去猜它的意思。

不满足时报 ``requires engine `>=0.3.13`, this is 0.3.12 (0.3.12-2026111001)``,在三处检查:

- **解析时**:每条加载路径都过 manifest 校验,所以哪里都绕不开。
- **安装时**:`plugin.install` 在暂存包上就失败,缓存里什么都不写,错误消息带出要求的版本
  和当前版本。
- **发现时**:同一个包装了几个版本,用满足条件的版本里最高的那个;更高而不满足的版本列进
  `plugins.refused` 健康项,`reason` 里是上面那条消息。一个满足的都没有,这个包就不加载。

Rust 插件编进二进制,版本由 Cargo 管,不用这个字段。

### `[dependencies]`

键是别的包的名字,值是 semver 版本条件(`"^1.2"`、`">=1, <2"`、`"=1.4.0"`),写法同
`requires_engine`;条件写不合法,manifest 解析就失败,消息指到 `dependencies.<名字>`。

装包时(`plugin.install`)在暂存包上检查:同一层 cache 里要有这个名字的某个已装版本满足条件,
否则安装失败、缓存里什么都不写:``requires plugin `base-tools` `^1.2`, which is not installed``
或 ``requires plugin `base-tools` `^1.2`, installed: 1.0.0``。不去自动装依赖,先装依赖再装它。
这是装包时的门槛,不是运行时的依赖图:之后卸载依赖,引擎不会替依赖它的包拦着。

manifest 里出现引擎不认识的顶级段,不拒,但打一条 WARN 说它"什么也不贡献"——`[[scripts]]`
这种拼错,那是唯一的信号。有三类写法是**拒装**的,因为它们是写给已经移除的东西的:
`[[wasm]]`(引擎不跑 WebAssembly 组件)、`[scene.own]`(改用 `[scene] dir`)和
`[[mcp]] kind = "dsh"`(引擎不带 DSH 桥,把插件做成 stdio MCP 服务器、用 `kind = "native"` 声明)。

校验规则:

- `[[script]]` 的 `entry` 必须是 `<文件>:<函数>`,文件必须在包里面。绝对路径、`..`、
  解析后指到包外的符号链接,都会被拒。
- `[scene] dir`、`[[agent]] prompt`、`[[mcp]] config`、`[skills] dir` 同样必须在包里面,
  规则同上;`[skills]` 段写了就必须给 `dir`。`scene.json` 按声明式场景的规则从严校验;
  agent 的提示词读不到、MCP 的 config 读不到或不是 JSON、目录里的场景校验不过、有一个
  skill 校验不过,包都**装不上**,而不是装上之后少一样东西。
- **哪些点能绑脚本由载体说了算**——`script_host::bindings::BINDABLE_POINTS`,
  `extending_quickjs.md` 里有。
- **来自包的脚本绑定逐条降级。** 一条兑现不了的绑定只让它自己消失,原因进 `plugin.list`
  的 `script_faults`。运维自己写在 `settings.json` 里的 `scripts` 仍是全有或全无。

## 包里的 MCP 服务器

一个 daemon 同时服务多个项目,stdio 服务又以插件根目录为工作目录启动,所以服务不能拿工作目录
当项目。引擎按 (项目, 场景) 各连一份,每条连接服务的项目是确定的,用两种方式告诉 stdio 服务
(对 `settings.json` 里声明的服务也一样):

- **MCP `roots`**:客户端声明 `roots` 能力,`roots/list` 答这个项目根,一个 `file://` URI;
  无项目会话(`project_root: null`)答空列表。2026-07-28 的服务在 `input_required` 里要,更早的
  自己发 `roots/list`,答的都一样。
- **环境变量 `AGIX_PROJECT_ROOT`**(只对 stdio 服务):项目根的绝对路径;无项目时没有这个变量,
  daemon 自己的环境里有也不传。它在 `sandbox.subprocess_env` 与服务自己的 `env` 之后设,
  allowlist 拿不掉它,配置也改不了它。

项目根是解析过符号链接的路径,同一个目录经链接或另一种写法打开,服务看到的是同一个;会话在
worktree 里时给的也是项目根。

远端服务(`streamable_http`、`sse`)两样都拿不到:客户端不声明 `roots` 能力,它照样来要时答空
列表。本机的绝对路径不交给网络另一头的服务;它要知道操作哪个项目,由工具参数或它自己的配置给。

服务自己的文件读写不经过 Agix:权限检查只管模型对它的那次工具调用,调用进了服务,它读哪里、
写哪里,引擎看不见也拦不住。服务应当把写入限在 roots 之内;装包的人要知道,它的进程有启动它的
用户的全部权限。

## 包里的 skill

```
my-plugin/
├── plugin.toml        # [skills] dir = "skills"
└── skills/
    ├── tour/
    │   ├── SKILL.md
    │   └── notes.md   # SKILL.md 的 files: [notes.md]
    └── audit/
        └── SKILL.md
```

**只认显式的 `[skills]` 段。** 没写这一段的包,包里就算有 `skills/` 目录也不读——今天恰好
带着这么个目录的旧包,升级 Agix 之后不会突然多出 skill。

**校验从严。** 安装时在临时目录里、发现时在 cache 里,`dir` 下的每一项都按下面的规则读,
有一项不过,整个包拒装(发现时则不加载,进 `plugins.refused`),错误里写明是哪个 skill、
哪一项:

- `dir` 下每一项(`.` 开头的除外)都得是一个 skill 目录,里面有 `SKILL.md`。散放的文件、
  没有 `SKILL.md` 的目录都算错,不跳过。目录名就是 skill 名:1–64 个 ASCII 字母、数字、
  `-`、`_`、`.`,以字母或数字开头,不以 `mcp__` 开头(那是 MCP 工具转成的 skill 用的)。
- skill 目录、`SKILL.md`、`files:` 里的每个文件解析后都必须在包里面,`files:` 的还必须在
  这个 skill 自己的目录里;符号链接指出去的拒。
- `SKILL.md` 必须以 `---` 包起来的 frontmatter 开头,`description` 必填,正文不能为空。
- frontmatter 每行一个 `key: value`:不能缩进(不支持嵌套映射和多行值),不能是 `|` / `>`
  块,值不能为空,同一个键(含 `allowed_tools` / `allowed-tools` 这类别名)不能写两次,键必须
  是引擎读的那些(`prompt_files::SKILL_FRONTMATTER_KEYS`)——拼错的键在磁盘上
  的 skill 里是悄悄不生效,在包里是拒装。
- `name` 可以不写;写了就必须等于目录名。`context` 只能是 `inline` / `fork`;`background`、
  `disable_model_invocation`、`user_invocable` 只能是 `true` / `false`(或 `yes` / `no`、`1` / `0`)。

**名字与优先级。** 包里的 skill 用裸名,以 `SkillSource::Plugin` 的来源、`Fallback` 优先级进
技能目录,与 agent 类型"包 < 磁盘"的规则一致:

```
内建 skill < 插件包的 skill(按包名排序,先到的赢) < 磁盘三层(全局、场景、项目 .agents/skills/)
```

被遮住的不报错,包照常装上;`plugin.list` 每个包的 `skill_conflicts` 列出它哪些 skill 被遮住、
被谁遮住(按这个 daemon 自己的项目和场景算,和其余 `plugin.*` 一样):

```json
{"name":"touring", "skills":["guide","tour"],
 "skill_conflicts":[
   {"skill":"tour",  "shadowed_by":"user",   "path":"/home/u/.agix/skills/tour/SKILL.md"},
   {"skill":"guide", "shadowed_by":"plugin", "plugin":"another", "path":"/home/u/.agix/plugins/cache/another/1.0.0/skills/guide/SKILL.md"}]}
```

`shadowed_by` 是占着这个名字的那份的来源:`user`(全局层或场景层)、`project`(项目的
`.agents/skills/`)、`plugin`(另一个包,`plugin` 给出包名)。

**生命周期跟着包走。** `plugin.install`(启用着的)、`plugin.enable` 之后有,`plugin.disable`、
`plugin.uninstall` 之后没有;`scope` 决定包落在哪一层,skill 随包所在那层的启用状态出现或消失。
这些调用返回时技能目录已经换好:`commands.list` 立刻是新的,正在跑的会话在下一轮开始时看到
变化(发 `skills_changed`,模型收到一段说明)。已经展开进对话的正文留在转录里。整个过程不往
任何 skill 目录里写东西。

skill 展开用的正文是包加载时读进来、披露过的那份,不是调用时再去 cache 里读;`files:` 的参考
文件照常在调用时读。

## 包里的命令

`[[command]]` 让模型写一行命令行(`forge spec list --status draft`)、用户敲 `/forge spec list`,
引擎在内嵌的 QuickJS 里跑包带来的 JavaScript,结果交回。工具、权限、限额、脚本里有什么见
`runext.md`;这里只说包这一侧。

- **入口只收源码。** `entry` 的文件解析后必须在包里(符号链接指出去的拒),至多 4 MiB,
  是 UTF-8 文本;带 NUL 或不是 UTF-8 的——QuickJS 字节码——安装时拒装,发现时不加载。
  字节码不经校验,构造过的能破坏宿主内存。引擎第一次跑一个入口时在本机把它编译成字节码,
  存在这一层 cache 的 `.bytecode/` 下,文件名是引擎版本、QuickJS 构建、入口文件名与入口源码的
  SHA-256;入口、引擎或 QuickJS 变了就是另一个文件。目录只属于当前用户(0700,文件 0600),别人可写或不属于当前用户的缓存不读、
  不写,只留最近用过的 64 个,见 `runext.md` §8。
- **`writes` 在两处收窄。** 安装时拒绝绝对路径、`~`、带 `..` 的、以及第一段就是通配(`**`、
  `*.md`,覆盖整个项目)的;用到时再按插件 skill 的 `allowed-tools` 路径规则解析到会话的项目
  里,经符号链接越出项目的那一条不生效。**配置路径不在任何命令的 `writes` 之内**:写到 `.git/`
  (git 的 hooks 与 config)、`.agix/` 等配置根里非工作区的条目、项目的 skill 目录这类会话
  会当作配置来问的地方(`SensitivePaths::names_configuration`),无论 `writes` 怎么写都直接
  失败——命令是装好后无人盯着运行的代码,写一个 git hook 就等于在下一次提交时执行代码。
- **同名。** 两个启用着的包声明了同名命令,按包名排序先到的保留,后到的不注册;一个 skill 与
  命令同名时 `/名字` 归 skill,`RunExt` 照样能跑这条命令。两种情况都记进 `plugin.list` 那个包
  的 `command_conflicts`:

```json
{"name":"zz-twin", "commands":[{"name":"notes","description":"…","read_only":[],"writes":[]}],
 "command_conflicts":[
   {"command":"notes", "shadowed_by":"plugin", "plugin":"notes-plugin"}]}
{"name":"notes-plugin", "command_conflicts":[
   {"command":"notes", "shadowed_by":"skill", "path":"/proj/.agents/skills/notes/SKILL.md"}]}
```

- **生命周期跟着包走。** 启用着的包的命令进新建的会话(子 Agent 跟着它的会话);已经在跑的会话
  不变。会话里至少有一个命令才有 `RunExt`。

## 包里的函数

`[[function]]` 是只给宿主调的函数,调用名是 `<包名>.<name>`(`github-tools.check`)。它不进模型的工具表,
也不进 `RunCode`。字段规则、披露与 `plugin.list` 的 `functions` 见 `runext.md` §12;入口与命令同一套检查
(只收源码、至多 4 MiB、不出包),编译结果同一个 `.bytecode/`。`input_schema` 安装时读成 JSON 并按 JSON
Schema 编译,过不了拒装;发现时再查一次,入口或 schema 后来变坏的包不加载。同一个包里函数不重名;
不同包的函数调用名带包名,不冲突。启用着的包的函数随包的启停进出函数表。

## 宿主库

```toml
uses = ["codix"]

[plugin]
name = "xflow"
```

`uses` 是包顶层的键(TOML 里要写在第一个段之前),列出包里的 JavaScript 要用的宿主库,按命名空间。
名字是小写 ASCII 字母开头,其后小写字母或数字;不能是 `agix`——引擎的基础库总在,不用声明。重复的只算
一次。宿主库由嵌入引擎的宿主提供(`agix-sdk.md`);没声明的包,它的运行时里没有这个全局。

- **装包时**:`uses` 里有宿主没提供的库,`plugin.install` 拒装,cache 里什么都不写,错误说缺哪个:
  ``needs host library `codix`, which this host does not provide``(缺几个就列几个)。
- **已经装着的包**:换了个不提供这个库的宿主(或宿主不再提供),包不加载;`plugin.list` 里它
  `enabled: false`,`disabled_reason` 是上面那句;`daemon.doctor` 的 `plugins.refused` 检查报
  `degraded` 并列出它。宿主重新提供这个库(库在池建起来时定下),不用重装就加载。
- **披露**:每个库一条能力,``uses host library `codix` (<宿主给的描述>)``;宿主没给描述的只写名字。

## 宿主自带的包

嵌入引擎的宿主可以带自己的包:Rust 侧池级装配的 `Assembly::host_plugins`(`EmbedConfig.plugins` 透传),
或运行中 `SessionPool::register_host_plugin(dir)`,每项是一个解开的包目录(根上有 `plugin.toml`)。协议
不提供登记。

- **登记即生效**:清单校验、`uses` 检查、披露的上限、入口与 schema 的检查同 `plugin.install`,过不了
  登记就失败(装配时整个装配失败);过了就进启用集,`[[command]]`、`[[script]]`、`[[function]]`、`uses`
  与普通包一样;`[dependencies]` 不检查。编译结果放在 global 层的 `.bytecode/`。同名再登记一次,替换前一个。
- **`plugin.list`**:标 `source: "host"`(装的包是 `"installed"`),`root` 是宿主给的目录。
- **写保护**:登记之后这个目录(宿主给的写法与解析后的路径)同宿主 skill 目录一样受保护:任何会话、任何
  权限模式下,写工具与点名它的会改东西的命令都被拒(`Stage::HostSkillDir`),沙箱里的命令写不了它,在可写
  根里时也挪不走它上面的目录。目录放在项目里也是这样。
- **身份**:包里代码调宿主库时,`HostCallContext.source` 是 `Host`;装的包是 `Installed`,不论它叫什么名字。
- **管不到它**:`plugin.uninstall`、`plugin.disable`(以及 `plugin.enable`)对宿主包的名字报错:
  ``` `xflow` is the host's own package; it cannot be uninstalled through the protocol ```。
- **同名**:用户装了与宿主包同名的包,宿主包优先;装的那个照样在 `plugin.list` 里,`shadowed_by: "host"`,
  `enabled: false`,`disabled_reason` 说明宿主带了同名包。
- **插件关着时**:`settings.json` 的 `plugins.enabled: false`(或构建里没有包方案)时宿主包也不加载:装配时
  跳过并记一条 warn,`register_host_plugin` 返回 `PLUGINS_DISABLED`。`plugins.allow` 白名单同样管宿主包,
  不在白名单里的列出来但不加载。

## 打包与安装

压缩包就是插件目录的一个普通 zip,`plugin.toml` 在根上;`tests/runner/src/plugin_fixture.rs`
是完整的打包步骤,测试就用它打包 `examples/plugins/quickjs/`。

```bash
cd my-plugin && zip -r ../my-plugin.zip . && shasum -a 256 ../my-plugin.zip
```

```json
{"jsonrpc":"2.0","id":1,"method":"plugin.install",
 "params":{"name":"github-tools","version":"1.2.0",
           "download_url":"file:///abs/path/my-plugin.zip",
           "checksum":"<sha256 hex>",
           "scope":"global"}}
```

- `download_url` 接受 `https://`/`http://` 和 `file://`。**网络来源必须带 checksum**,没有就
  在下载前拒掉;`file://` 可以不带。
- `scope` 是 `"global"`(默认)或 `"scene"`,决定落进哪一层的 cache。
- `name`、`version` 是 cache 里的目录名,只收 ASCII 字母、数字、`.`、`_`、`-`、`+`,不以
  `.` 开头;`name` 还得与包里 `[plugin] name` 一致。
- 压缩包最大 128 MiB;解开后合计不超过 512 MiB、条目不超过 10 000 个,单个大于 1 MiB 的
  条目压缩比不超过 200 倍。超了就拒,什么都不留下。
- 先解到同一目录下的临时目录,在里面按发现时的方式把 manifest 读回来、检查披露(读不回来,
  包括上面那几类拒装的写法,或披露超限,就报错,cache 里不会出现这个包),都过了才改名成
  `<version>/`,并写一个完成标记 `.agix-installed`,里面是压缩包的 SHA-256;同一层 cache 的
  安装与卸载互斥,扫描 cache 的一方(`cache/.lock` 的共享锁)等它们做完再读。没有完成标记
  的版本目录不算装过,再装一次会替换它。
- 已经装过的同一版本:这次给的 `checksum` 与完成标记里记的不一致,cache 里的就不算要装的
  这个包,会被替换;一致或没给就算装过,照样读回、照样披露,读不回来就报错(不删,它不是
  这次放进去的)。
- zip 里带 `..` 或绝对路径的条目跳过,不解压。

其余的生命周期:`plugin.list`(每个已安装的插件连同启用状态、`source`、`root`、`skills` 与
`skill_conflicts`、`commands` 与 `command_conflicts`、`functions`,包括被禁用的,以及宿主自带的)、
`plugin.enable` / `plugin.disable`(按层持久化在 `enabled.json`;scene 层压过 global,没设过
的算启用)、`plugin.uninstall`(`version` 可选,不给就全删)、`plugin.reload`。

`enabled.json` 是包唯一的关闭开关,所以它坏了往安全的一边倒:文件在、但读不了或不是
`{"名字": bool}` 的那一层,当作这一层**每个包都禁用**;`plugin.enable` / `plugin.disable`
拒绝覆盖它(覆盖会把它关掉的包重新打开),要人手工修好或删掉;`daemon.doctor` 的
`plugins.enable_state` 检查报 `degraded` 并给出是哪一层、什么原因。文件是先写临时文件再改名
写进去的,写到一半不会留下坏文件。

`plugin.install` 的结果带 `enabled`,是装完之后这个包解析出的启用状态。`settings.json` 的
`plugins.enable_on_install` 缺省 `true`,装完即启用;设为 `false` 时,这一层还没有它启用
记录的新包装完保持禁用,要宿主读过披露后显式 `plugin.enable`。`daemon.doctor` 的
`plugins.enable_on_install` 检查在开关为 `true` 时提示:计划在后续 0.3.x 把缺省改成
`false`。

插件的场景装上就是 active,不需要 `scene.activate`;进入它仍然要显式的
`session.create {"scene": "plugin:<name>"}`。

## 它落在哪

```
~/.agix/plugins/cache/<name>/<version>/                   # global 层
~/.agix/scenes/<scope>/plugins/cache/<name>/<version>/    # scene 层,同名时压过 global
```

同一个名字装了多个版本时,semver 最高的赢。加载不了的 manifest、披露超过下面上限的包,
带一条警告被跳过,其他插件照常加载;手工放进目录或旧版本装进来的超限包同样不加载,
`daemon.doctor` 的 `plugins.refused` 检查报 `degraded` 并列出名字、版本和原因。

## 安装期披露

`plugin.install` 成功时返回一个 `disclosure`,安装器应当把它摆到安装者面前:

```json
{"plugin":"github-tools","version":"1.2.0",
 "capabilities":["run its own JavaScript at the `tool.result` extension point",
                 "adds command `forge` that may write forge/** through the file tools; its other calls (Bash, MCP, web) get what the session's rules allow without asking",
                 "exposes function `github-tools.check` to the host; writes no files"],
 "scene":"plugin:github-tools",
 "skills":["triage"],
 "commands":["forge"],
 "functions":["github-tools.check"],
 "mcp_servers":["github (native): runs `npx -y github-mcp`; env: GITHUB_TOKEN"],
 "model_visible":[{"origin":"plugin description","text":"GitHub tools"},
                  {"origin":"agent `pr-reviewer` system prompt","text":"…"},
                  {"origin":"skill `triage` description","text":"Triage new issues"},
                  {"origin":"skill `triage` body","text":"…"},
                  {"origin":"command `forge` description","text":"需求、规格与变更流程"}],
 "inert":false}
```

`[[script]]` 列在 `capabilities` 里:它是宿主进程里的代码,跑在一个能改写模型读到什么的
点上,没有东西把它和宿主隔开。skill 的正文里有 `` !`cmd` `` / ` ```! ` 块的、`allowed-tools`
非空的,也各记一条能力。每个 `[[command]]` 一条:``adds command `<名字>` that may write
<writes> through the file tools; its other calls (Bash, MCP, web) get what the session's rules
allow without asking``,不写文件的把 `may write <writes>` 换成 `writes no files`。`writes` 只管住
命令经 `Write` / `Edit` / `NotebookEdit` 的写入;它调的 `Bash`、MCP 工具按会话的规则走,会话不问就
放行的(比如有一条 `Bash` 的 allow 规则),命令也能做(`runext.md` §6)。它的 `description` 进 `model_visible`
(来处 ``command `<名字>` description``),`commands` 是包带的命令名。每个 `[[function]]` 一条:
``exposes function `<包名>.<名字>` to the host; may write <writes>``(不写文件的是 `writes no files`),
`functions` 是调用名;函数的描述不到达模型,不进 `model_visible`。`uses` 的每个库一条
``uses host library `<名字>` (<描述>)``。

`model_visible` 列出每一段会一字不差到达模型的文本——插件描述、agent 描述与提示词、场景
描述与提示词、skill 的 `description` / `when_to_use` / 正文 / `files:` 参考文件(来处写成
``skill `<名字>` description`` / `when_to_use` / `body` / ``file `<路径>` ``)——每段标明来处。
"文本到达模型"是隔离解决不了的,唯一的防线是有人去读。`skills` 是包带的 skill 名。

`mcp_servers` 每项是 `<名字> (<kind>): ` 加它解析出的命令行(`runs …`)或地址
(`connects to <url> (<type>)`);`env`、`headers` 只列键名,值通常是凭据。

两个上限是拒绝,不是警告:单行描述(含 skill 的 `description`、`when_to_use`)超过
**500 字符**;提示词(含 skill 正文和它的每个参考文件)超过 **40 000 字符**,
`plugin.install` 就返回错误,包不会进 cache;扫描时也按同样的上限,超限的包不加载。

`inert: true` 表示这个包不声明能力、没有场景、没有 MCP 服务器、没有任何对模型可见的文本。
带命令的包总有能力条目,不会是 inert。

## 信任

- 包里的脚本以 `BlockOrigin::Plugin` 的身份运行,在提示词那几个点上"只能新增";它们的
  每一次工具调用照常过权限规则。QuickJS 的限额限的是开销,不是恶意代码,所以包里的脚本
  能做什么由这份身份决定,而不是由虚拟机决定。
- 包声明的 agent 类型带 `AgentTypeSource::Plugin`,它对 `permission_mode` / `max_turns` 的
  覆盖被钳制——网上来的定义不能给子代理比会话本身更多。
- 包带的 skill 来源是 `SkillSource::Plugin`:`allowed-tools` 里只写工具名的整类授权不生效,
  要写成 `Tool(内容)`,而且内容要真正收窄,够得和整类授权一样远的也不生效:路径要解析到项目
  之内(`Write(src/**)` 生效,`Write(**)`、`Write(x/../../**)`、`Edit(/U*/**)`、`Write(/tmp/x/**)`
  不生效),shell 命令要以一个完整的程序名加空格开头、且不是 `bash` / `env` / `sudo` / `xargs`
  这类跑别的命令的程序(`Bash(cargo test:*)` 生效,`Bash(*)`、`Bash(b*)`、`Bash(bash:*)` 不生效),
  URL 的主机通配要落在可注册的域名之内(`WebFetch(https://*.example.com/**)` 生效,
  `https://*.co.uk/**`、`https://*.github.io/**` 不生效),见 `ARCHITECTURE.md` §6.1;正文里的 `` !`cmd` `` 和模型的一次 `Bash` 调用一样先过会话的权限规则,
  要询问的不跑。项目信任管的是项目自己的 `.agents/skills/`,不改变包里 skill 的这些规则;
  未信任项目的 `.agents/skills/` 仍按磁盘层压过包里的同名 skill(它的命令不跑、授权不给)。
- 包带的命令(`[[command]]`)跑在宿主进程的 QuickJS 里,它的每一次子调用照常过权限规则;调用
  命令是授权点(会写的在 `default` 下问一次),授权只盖到这次调用自己写进 `writes` 的文件,
  `writes` 之外的写入与对配置路径的写入直接失败,见 `runext.md` §6。
- 用户没信任的项目,它自己的配置按同一条规则对待(`ARCHITECTURE.md` §6.6):克隆来的仓库
  和下载来的包一样,只能新增、只能收紧。
- `settings.json` 的 `plugins` 段是部署级开关:`{"plugins": {"enabled": false}}` 一个都不
  加载;非空的 `{"plugins": {"allow": ["a", "b"]}}` 是白名单。完全不要插件的部署优先用
  构建 flag,那样没有东西可以再打开。白名单按 manifest 里的名字比,发现时检查,手工放进
  目录的包也躲不过。
