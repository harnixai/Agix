# forge：极简的规格驱动开发插件

forge 是 Agix 的插件样例，也是一个能直接用的工具：在一个项目里管需求、管规格，按阶段把一次
变更从提出走到归档。它演示一件事——**机械的活交给程序，判断与写作交给模型**——以及 Agix 的
插件能力怎么拼起来做成一个开箱可用的东西。

它刻意做得小：没有门禁、没有审批、没有治理校验。阶段只是一张顺序表，每个阶段一个 skill，上一
阶段的产出交给下一阶段；手动一步步推，也可以让它自己往下走。没有流程的小项目只用需求和规格两组
也成立。

代码在 `examples/plugins/forge/`。用到的引擎能力见 `runext.md`、`plugin_packages.md`。

## 1. 包里有什么

```
examples/plugins/forge/
├── plugin.toml
├── forge.js                 # 命令：一个普通的 JS 文件，不需要构建
├── skills/                  # 每个阶段一个 skill，外加 forge-req
├── agents/forge-verifier.md # 验证阶段的只读子 agent
├── scripts/guard.js         # tool.around：提醒模型别直接读写 forge/
├── scripts/rules.js         # prompt.block：一行规矩
└── types/                   # runext.d.ts、agix.d.ts，forge.js 以 // @ts-check 引用
```

`forge.js` 内部分几段：命令分发、本地后端（`forge/` 下的 Markdown 文件）、远端后端（经
`callTool` 调会话里配好的 MCP 服务）、规格条目的解析与改写、变更流程。frontmatter 与命令
`input` 的 JSON Schema 作为常量写在文件里。

```toml
[plugin]
name = "forge"
version = "0.1.0"
api_version = "0.1"
requires_engine = ">=0.3.19"
description = "需求、规格与变更流程：模型用命令读写结构化数据，不直接读写文件"

[[command]]
name = "forge"
entry = "forge.js:main"
description = "需求、规格与变更流程（forge help）"
read_only = ["", "help", "--help", "req list", "req get", "req --help", "req add --help", "req update --help",
             "spec", "flow list", "flow show", "flow next", "flow --help", "flow new --help",
             "flow delta --help", "flow plan --help", "flow check --help", "flow verify --help",
             "flow advance --help", "flow archive --help"]
writes = ["forge/**"]

[skills]
dir = "skills"

[[agent]]
name = "forge-verifier"
description = "逐条核对一次变更的验收标准，只读"
prompt = "agents/forge-verifier.md"
allowed_tools = ["Read", "Grep", "Glob", "RunExt"]
permission_mode = "plan"
max_turns = 30

[[script]]
point = "tool.around"
entry = "scripts/guard.js:onAround"
timeout_ms = 1000

[[script]]
point = "prompt.block"
entry = "scripts/rules.js:onBlock"
timeout_ms = 1000
```

## 2. 数据

三组：需求（req）、规格（spec）、变更（change，流程的载体）。任务不单独成组，实现计划是变更里的
一张清单。

### 存储布局（本地后端）

```
forge/                       # 随项目提交
├── config.yaml
├── reqs/R-001.md
├── specs/<能力>.md           # 现行规格
├── changes/C-003/
│   ├── change.md            # 阶段、历史、计划清单、验收记录
│   ├── delta.md             # 这次变更对规格的增、改、删
│   └── design.md            # 可选，flow plan 的 input.design
└── archive/C-001/           # 归档后的变更
```

命令只能新建和改写文件，删不了（`RunExt` 的子调用里没有删除文件的工具）。所以归档时变更
复制进 `archive/`，`changes/C-001/` 里的文件改写成一行指向 `archive/` 的说明（`change.md` 的
frontmatter 是 `{id, archived: true}`），列表与读取都跳过它们；要清掉用 `git rm -r`。

每个文件是 YAML frontmatter 加 Markdown 正文。frontmatter 是程序读写的结构化部分，正文是模型
写的内容，程序只存不改。读写用 `agix.yaml.parseFrontmatter` / `stringifyFrontmatter`，正文逐字节
保留；frontmatter 每次读进来、写出去前都按 `forge.js` 里的 schema 常量用 `agix.json.validate`
校验，人手改坏的文件在命令里报出是哪个文件、哪个字段，而不是让后面的操作在一份坏数据上继续。
文件名也要和内容对得上：`reqs/R-003.md` 里的 `id` 得是 `R-003`；`specs/report.md` 的
`capability` 得是 `report`，里面的条目编号也都是 `S-report-…`。`reqs/` 下文件名不是需求编号的
文件（`README.md` 之类）不算需求。

**需求** `reqs/R-001.md`

```markdown
---
id: R-001
title: 导出报表为 CSV
status: accepted          # proposed | accepted | rejected | done
priority: high            # high | medium | low
specs: [S-report-004]
changes: [C-003]
---
用户需要把月度报表导出成 CSV，在表格软件里继续处理……
```

**规格** `specs/report.md`：一个能力一个文件，条目带编号与验收标准。

```markdown
---
capability: report
---
## S-report-004 CSV 导出
reqs: R-001

导出的 CSV 用 UTF-8，带表头……

### 验收
- [A1] 导出文件第一行是表头
- [A2] 含逗号的字段用双引号包起来
```

条目以 `## S-<能力>-<序号> 标题` 开头，下面一行 `reqs:`，正文，`### 验收` 下每条 `- [A<n>]`。
程序按这个格式解析和改写，模型通过命令交来的是条目的正文与验收列表。规格文件的正文由程序重写
（条目之前的段落保留，去掉首尾空行），需求与变更的正文逐字节保留。

条目写进文件之前，程序把它渲染成 Markdown 再解析一遍，读回来和交来的不一样就拒绝，一个文件都
不写：正文里以 `## S-…` 开头的行会被当成另一个条目，单独一行的 `### 验收` 会被当成验收标题，
这样的正文要改写。标题与每条验收要有非空白的字符。

**增量** `changes/C-003/delta.md`：frontmatter 是 `{change, add: [S-…], modify: [S-…], remove: [S-…],
base: {S-…: <sha256>}}`，正文是 `add` 与 `modify` 的完整条目，格式同上——`modify` 交来的只是改了的
字段，写进增量时已和现行条目合成完整的一条，归档就是用它整条替换。`base` 给 `modify` 与 `remove`
的每个条目记下写增量那一刻现行条目（渲染成 Markdown 后）的 SHA-256，归档时据此发现它后来被别的
变更改过（§3）。

**变更** `changes/C-003/change.md`

```markdown
---
id: C-003
title: 报表 CSV 导出
stage: apply              # propose | spec | plan | apply | verify | archive | done
reqs: [R-001]
history:
  - { stage: propose, at: 2026-11-18T10:02:00Z }
  - { stage: spec, at: 2026-11-18T10:20:00Z }
plan:
  - { id: P1, text: 加 CsvWriter, done: true }
  - { id: P2, text: 报表页加导出按钮, done: false }
verification: []
---
为什么做、范围……
```

### 编号

`R-<三位序号>`、`S-<能力>-<三位序号>`、`C-<三位序号>`、计划项 `P<n>`、验收项 `A<n>`，由程序分配，
取现有最大序号加一。规格条目还要避开其它变更（未归档的与已归档的）增量里出现过的编号，所以删掉
的条目，编号不会再分给新条目。`mcp` 后端的需求由服务分配。模型从不自己编号，命令行里的编号也先按
格式校验。

### 后端

`forge/config.yaml` 按组选后端：

```yaml
reqs:  { backend: local }
specs: { backend: local }
# 需求放在团队的需求系统里：
# reqs: { backend: mcp, server: reqdb }
flow:
  auto: false               # 缺省 false
  stop_before: [apply]      # 缺省 []
```

没有 `config.yaml` 时两组都是本地、手动推进。它是人写的文件，命令只读不写。

`mcp` 后端经 `callTool("mcp__<server>__…")` 调会话里配好的 MCP 服务，连接、登录由 Agix 管。服务
要提供三个工具：

| 工具 | 输入 | 输出 |
|---|---|---|
| `items_query` | `{ kind, filter?, fields?, limit?, cursor? }` | `{ items, next_cursor? }` |
| `items_get` | `{ kind, id }` | `{ item }` |
| `items_put` | `{ kind, item }` | `{ item }`（带服务分配的 `id`） |

`kind` 是 `req` 或 `spec`。`req` 的条目就是需求的 frontmatter 加 `body`；`spec` 的条目是
`{id, capability, title, reqs, body, criteria: [{id, text}]}`，删除的条目写成 `removed: true`，列表
里不再出现。服务交回的条目同样按 schema 校验；`items_get` 交回的条目编号得是问的那个；
`items_query` 交回的没有 `items`，或者同一个 `next_cursor` 交回两次，都按服务出错报出来。`server`
只能是字母、数字、`_`、`-`。变更（流程状态）总是本地的：它属于这个仓库的这次开发。

`callTool` 交回 MCP 工具的结构化结果（`structuredContent`）；工具只给了文本时，forge 从文本里取
JSON。

命令里的 MCP 调用按会话的判定走（`runext.md` §6）：有人值守时每次都问，无人值守或从程序里跑
时直接失败，所以用 `mcp` 后端要给这三个工具一条 allow 规则：

```json
"permissions": {
  "allow": ["mcp__reqdb__items_query", "mcp__reqdb__items_get", "mcp__reqdb__items_put"]
}
```

plan 模式下 MCP 工具照样被拦，远端那一组的只读子命令在 plan 模式下用不了。

## 3. 命令

模型经 `RunExt` 调用，用户可以直接 `/forge …`。

| 命令 | 只读 | 做什么 |
|---|---|---|
| `forge help [组]`、`forge --help`、`forge <组> [动词] --help` | ✓ | 用法 |
| `forge req list [--status s] [--priority p]` | ✓ | 列表：编号、标题、状态、优先级 |
| `forge req get <id>` | ✓ | 一条需求，含正文与关联 |
| `forge req add` | | `input: { title, priority?, body }` 或 `input: { items: [...] }` 批量；返回分配的编号 |
| `forge req update <id> [--status s] [--priority p] [--title t]` | | 改字段；正文放 `input.body` |
| `forge spec list [--capability c]` | ✓ | 条目列表 |
| `forge spec get <id>` | ✓ | 一个条目，含验收 |
| `forge spec coverage` | ✓ | 没有规格的已接受需求、没有需求的规格条目、验收未通过的（只看已归档的变更，按归档先后，后归档的记录覆盖先前的） |
| `forge flow new` | | `input: { title, reqs?, body }`，建变更，进入 `propose`；关联的需求从 `proposed` 变成 `accepted` |
| `forge flow list` / `flow show <id>` | ✓ | 变更列表；一次变更的阶段、计划、验收 |
| `forge flow next <id>` | ✓ | 下一阶段、该用哪个 skill、自动模式下要不要停 |
| `forge flow delta <id>` | | `input: { add: [{capability, title, body, criteria, reqs?}], modify: [{id, …改了的字段}], remove: [id] }`，写规格增量；再交一次整个替换。从 `spec` 起都能写，包括 `archive`（见下）；同一条目在 `modify`、`remove` 里出现两次拒绝 |
| `forge flow plan <id>` | | `input: { items: [...], design? }`，写计划清单（同样整个替换，文字没变的项保留勾选）；`design` 写进 `design.md` |
| `forge flow check <id> <P#>...` | | 勾掉计划项，一次可以几个 |
| `forge flow verify <id>` | | `input: { results: [{ spec, criterion, pass, note }] }`，记验收结果；有不通过的，变更回到 `apply`；同一条验收给了两个结果拒绝 |
| `forge flow advance <id>` | | 进入下一阶段，记历史；结果带着 `flow next` 的那几项 |
| `forge flow archive <id>` | | 把增量合进现行规格、关联需求标为 `done`、变更移入 `archive/`；先核对再写（见下） |

列表默认只给编号、标题、状态等短字段，正文要 `get`。选项用 `agix.cli.parseArgs` 解析；选项或
参数个数错了，错误消息里带这一组的用法；`--help` 在哪一组都给那一组的用法。`read_only` 把
`--help` 的这几种写法都列上了，看用法不询问，plan 模式下也行；只敲一个 `forge` 也给用法，
`read_only` 里的空串 `""` 就是这种不带参数的调用，同样不询问。`input` 按约定是
对象，模型常把它写成 JSON 或 YAML 的文本，命令照收（先按 JSON 读，不是 JSON 再按 YAML）。

`advance` 只因结构错误拒绝（没有增量就进 `plan`、计划为空就进 `apply`、还有没勾掉的计划项就进
`verify`、有验收项没有通过记录就进 `archive`、`archive` 阶段要用 `flow archive`），不因内容好坏
拒绝。

验收记录跟着条目的文本走：重交增量时，渲染出来和上一版不一样的条目，它们的验收记录作废，
`advance` 要等它们重新验过；`archive` 阶段的变更因此退回 `verify`。

`flow archive` 先核对、再写。增量里 `modify`、`remove` 的条目，现行的样子得还是 `base` 记下的那样；
有一条在这之后被别的变更改过（两个变更改同一个条目，另一个先归档了），整个归档拒绝，一个文件都
不写，提示模型看现行条目、照它重交增量——重交时 `modify` 合在新的现行条目上，文本变了，于是回
`verify` 再验一遍，后归档的那次不会把先归档的改动覆盖掉。归档中途失败（写了一半）可以再跑一遍：
已经是增量里那样的条目跳过，已经删掉的条目跳过，`archive/` 下已经写好的文件先读再覆盖。

`flow next` 与 `advance` 交回 `{change, stage, skill, auto, continue, say}`：`stage` 是变更现在
所处、接下来要做的阶段，`skill` 是它的 skill，`continue` 说自动模式下要不要接着做，`say` 是给
模型的一句话（接着调哪个 skill，或者停下、告诉用户在等什么）。

## 4. 阶段

| 阶段 | skill | 模型做 | 程序做 | 交给下一阶段 |
|---|---|---|---|---|
| propose | `/forge-propose` | 和用户把需求谈清楚 | `req add`、`flow new`，编号、关联 | 变更与需求编号 |
| spec | `/forge-spec` | 写规格条目的增、改、删与验收标准 | `flow delta`：校验格式、补编号、记追溯 | 规格增量 |
| plan | `/forge-plan` | 拆实现步骤 | `flow plan`：存清单 | 计划清单 |
| apply | `/forge-apply` | 写代码，每做完一项 `flow check` | 记进度 | 代码与勾完的清单 |
| verify | `/forge-verify` | `forge-verifier` 子 agent 逐条核对验收 | `flow verify`：汇总，未通过的给出回到 apply 的提示 | 验收记录 |
| archive | `/forge-archive` | — | `flow archive`：合并增量、改需求状态、移目录 | 更新后的现行规格 |

另有 `/forge-req`：不走流程，单独整理需求（录入、改状态、查覆盖）。

每个阶段的 skill 正文写三件事：这一阶段要做什么、用哪几条命令、做完调 `forge flow advance` 并照它的
`say` 做。命令的完整用法不进 skill，要时 `forge help`。

`verify` 阶段由模型先自己跑一遍测试，再把变更号和测试结果交给 `forge-verifier`；子 agent 在
plan 模式下只读，`forge flow show` 拿到验收项，逐条找证据，交回 `{results: [...]}`，模型原样交给
`flow verify`。

**手动**：用户按顺序调 skill，或者 `/forge-next C-003`——它调 `flow next` 再展开对应的 skill。

**自动**：`config.yaml` 里 `flow.auto: true`。每个 skill 的最后一步是 `flow advance`；它的结果说可以
继续，就接着用 Skill 工具展开下一个阶段的 skill；到 `stop_before` 里的阶段停下，告诉用户在等什么。

## 5. 程序与模型的分工

程序做的，模型不做：编号、关联与追溯、字段校验、状态与阶段的推进、列表与筛选、覆盖统计、
把增量合进现行规格、移目录。

模型做的，程序不做：和用户谈需求、写规格正文与验收标准、拆计划、写代码、判断验收是否通过。

批量的机械活：一次导入三十条需求是一次 `forge req add`（`input.items`）；把一批需求改成
`accepted`，模型写一个 `RunCode` 程序循环调 `RunExt`，一次工具调用。程序里调会写的子命令要有
`RunExt(forge req:*)` 这样的 allow 规则（程序不能询问），没有就逐条调。

## 6. 护栏

`scripts/guard.js` 绑在 `tool.around` 上。它是**给合作的模型的提醒，不是边界**：模型照着
`prompt.block` 那行规矩和 skill 本来就走命令，顺手想直接打开 `forge/` 下的文件时，守卫拦这一下，
理由里写该用哪条 `forge` 命令，下一次它就改用命令。模型因此拿不到散文式的数据文件，也不会顺手
改坏它们的结构。

**拦什么**：`via` 是 `"model"`、`"RunCode"`，或者别的插件命令（`"RunExt:<别的命令>"`；输入带
`plugin` 字段时，`plugin` 不是 `forge` 的 `"RunExt:forge"` 也算），对项目 `forge/` 的 `Read`、
`Write`、`Edit`、`NotebookEdit`、`Glob`（`path` 与 `pattern`）、`Grep`（`path` 与 `glob`），以及
命令行里明摆着写到 `forge/` 的 `Bash`、`Monitor`：有一个参数是 `forge`、以 `forge/` 开头，或是
`forge/` 下的绝对路径。命令名本身不算，`forge build` 照常跑。

**放行**：forge 自己命令的子调用（`"RunExt:forge"`）、用户敲的 `/forge`（`"user"`）、skill 的内联
命令（`"skill"`：skill 作者写好的命令，不是模型临场写的）；路径里有 `/plugins/cache/` 的文件，即
装好的插件自己的文件，比如 forge 的 skill 与脚本。

**哪里是项目的 `forge/`**：输入带 `projectRoot`（项目根的绝对路径）时，相对路径按它解析，只认
`<projectRoot>/forge` 与其下，`src/forge/mod.rs` 不受影响。不带时退回猜：相对路径以 `forge/` 开头
才算，绝对路径里有一段叫 `forge` 的目录就算——项目本身放在一个叫 `forge` 的目录里时会误拦。比较
一律转小写：macOS 与 Windows 的文件系统默认不分大小写，`Forge/reqs/R-009.md` 写的就是 `forge/`
下的文件。

**拦不住的**：shell 换个写法就过得去——变量、命令替换、通配、先 `cd` 到别处再用相对路径、换一个
解释器去读；子进程、别的 MCP 服务、符号链接也不经过它。真要不许模型改 `forge/`，没有现成的边界：
`Write(forge/**)` 这样的 deny 规则同样拦下 forge 命令自己的写入（deny 规则胜过命令调用的授权，
`runext.md` §6）。可靠的办法是在评审里看 `forge/` 的 diff——这些文件随项目提交，人照常可以在编辑器
里打开、在 git 里看。

`scripts/rules.js` 在 `prompt.block` 加一行：这个项目的需求、规格、变更都在 forge 里，用
`RunExt` 的 `forge` 命令读写，不要直接打开 `forge/` 下的文件。

## 7. 测试

**桩层**（`tests/blackbox/tests/forge.rs`，CI 每次 push 都跑）：从 `examples/plugins/forge` 现打包、
`plugin.install`，模型那端是桩。

- 披露里有 `forge` 命令与它的写入范围、八个 skill、`forge-verifier` 的提示词；`plugin.list` 报出
  命令的 `read_only` 与 `writes`，脚本没有故障
- 模型桩经 `RunExt` 走完 propose → archive：需求是 `done` 且关联了条目与变更，`specs/` 里是合并后的
  条目，`archive/` 下有变更与验收记录；`default` 下每个写子命令问一次，只读的不问，命令自己的写入
  不再问；提示词里有规矩那一行
- 自动模式：`advance` 到 `spec` 时 `continue` 为真，到 `stop_before` 的 `apply` 时停；计划为空时
  `advance` 拒绝
- plan 模式下只读子命令可用，写子命令被拒，什么都不问
- 人手改坏的文件按文件与字段报错，文件名与 `id`、`capability` 对不上的也报；不认识的选项交回那一组
  的用法；不是编号的参数被拒
- `--help` 的几种写法在 plan 模式下可用；参数个数不对交回用法；`reqs/README.md` 不算需求；
  `forge req constructor` 是不认识的命令
- 模型桩直接 `Read`、`Glob`、`Grep`、`Bash`、`Write` `forge/` 与程序里的 `Read`，都被守卫拒绝且
  理由里有命令，没有询问；`RunExt` 自己的读不受影响。`src/forge/mod.rs` 照常能读，`Forge/reqs/…`
  的写入与 `rm -r forge` 被拒
- 正文里有 `## S-…` 行或 `### 验收` 行、标题全是空白、同一条目出现两次、`propose` 阶段交增量，
  都被拒且什么都不写；另一个变更的增量不受影响
- 验收不通过回到 `apply`；同一条验收两个结果被拒；验过之后重交改了文本的增量，验收记录作废，
  `advance` 拒绝
- 两个变更改同一个条目：后归档的被拒，照现行条目重交增量、回 `verify` 再验后归档，两处改动都在；
  一次归档里既改又删；删掉的条目编号不再分出去
- 归档写了一半（要删的条目已删、`archive/` 下的 `change.md` 已写）时再跑一遍能走完
- `spec coverage` 只看已归档的变更：未归档变更里的不通过记录不影响现行条目
- `reqs` 切到 `mcp` 后端（toy 服务的 `--store` 模式）：同一组命令的结果与本地后端相同，需求在
  store 里，`forge/reqs` 不存在，变更仍在本地；`specs` 切到 `mcp` 后端，走完整个流程，条目在 store
  里，`forge/specs` 不存在
- `RunCode` 循环调 `RunExt` 批量改状态；`input` 以 JSON 文本交来照样收
- `/forge req list` 不经模型、不进对话
- 把 `writes` 收窄到 `forge/reqs/**` 的副本开不了变更；带字节码的副本拒装

**LIVE**（`tests/blackbox/tests/live_forge.rs`，Flash，`default` 模式由宿主批准，不开 bypass）：

- 手动：在空项目里 `/forge-propose` 一段需求（`ledger`，一个只用标准库、读 CSV 记账文件的命令行
  程序，四个子命令），之后每轮 `/forge-next C-001`，直到归档。断言只看外部结果：测试框架自己在
  项目里跑 `cargo build` 与 `cargo test` 通过；Rust 代码在 150 到 3000 行之间；`/forge flow show`
  （不经模型）里变更是 `done`，每个条目都在 `specs/` 里、每条验收都有通过的记录；关联的需求是
  `done`；代理记下的请求里，没有一次对 `forge/` 的直接读写成功过（子 agent 的也算）
- 自动：`flow.auto: true`、`stop_before: [apply]` 时，一条 `/forge-propose`（消息里说"有不清楚的
  自己定，别问"）走到 `apply` 前停下：有增量、有计划，计划项一个都没勾。模型还是问了、没开出变更
  时，再发一句"你自己定，接着做"
- 需求走远端后端：`/forge-req` 录三条、改一条状态，store 里是三条且一条 `accepted`，`forge/reqs`
  不存在
- 让模型直接打开 `forge/` 下的文件：它要是试了，被拒的理由里有该用的命令；口令出现在一次成功的
  `RunExt`（`forge req get`）结果里，不出现在别的工具结果里，答案里有它
- 对比（只出报告）：同一个小库（`roman`），装 forge（自动模式一条消息）与不装，打出调用次数与
  prompt token

## 8. 打包

不需要构建：YAML、校验、glob、参数解析都用引擎的 `agix.*`，`forge.js` 里只有 forge 自己的代码。

```bash
cd examples/plugins/forge && zip -r ../forge.zip . && shasum -a 256 ../forge.zip
```

再按 `plugin_packages.md`「打包与安装」发 `plugin.install`。

拿它做自己的插件：复制目录，改 `[plugin] name` 与 `[[command]] name`，换掉 `forge.js` 里的领域
代码，留下 `types/`。
