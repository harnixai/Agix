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
├── scripts/guard.js         # tool.around：模型不直接读写 forge/
├── scripts/rules.js         # prompt.block：一行规矩
└── types/                   # runext.d.ts、agix.d.ts，forge.js 以 // @ts-check 引用
```

`forge.js` 内部分几段：命令分发、本地后端（`forge/` 下的 Markdown 文件）、远端后端（经
`callTool` 调会话里配好的 MCP 服务）、规格条目的解析与改写。frontmatter 的 JSON Schema 作为常量
写在文件里。

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
read_only = ["help", "req list", "req get", "spec list", "spec get", "spec coverage",
             "flow list", "flow show", "flow next"]
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
│   └── design.md            # 可选
└── archive/C-001/           # 归档后的变更
```

每个文件是 YAML frontmatter 加 Markdown 正文。frontmatter 是程序读写的结构化部分，正文是模型
写的内容，程序只存不改。读写用 `agix.yaml.parseFrontmatter` / `stringifyFrontmatter`，正文逐字节
保留；frontmatter 每次读进来、写出去前都按 `schemas/` 用 `agix.json.validate` 校验，人手改坏
的文件在命令里报出是哪个文件、哪个字段，而不是让后面的操作在一份坏数据上继续。

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
程序按这个格式解析和改写，模型通过命令交来的是条目的正文与验收列表。

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
取现有最大序号加一。模型从不自己编号。

### 后端

`forge/config.yaml` 按组选后端：

```yaml
reqs:  { backend: local }
specs: { backend: local }
# 需求放在团队的需求系统里：
# reqs: { backend: mcp, server: reqdb }
flow:
  auto: false
  stop_before: [apply]
```

`mcp` 后端经 `callTool("mcp__<server>__…")` 调会话里配好的 MCP 服务，连接、登录由 Agix 管。服务
要提供三个工具：

| 工具 | 输入 | 输出 |
|---|---|---|
| `items_query` | `{ kind, filter?, fields?, limit?, cursor? }` | `{ items, next_cursor? }` |
| `items_get` | `{ kind, id }` | `{ item }` |
| `items_put` | `{ kind, item }` | `{ item }`（带服务分配的 `id`） |

`kind` 是 `req` 或 `spec`。变更（流程状态）总是本地的：它属于这个仓库的这次开发。

## 3. 命令

模型经 `RunExt` 调用，用户可以直接 `/forge …`。

| 命令 | 只读 | 做什么 |
|---|---|---|
| `forge help [组]` | ✓ | 用法 |
| `forge req list [--status s] [--priority p]` | ✓ | 列表：编号、标题、状态、优先级 |
| `forge req get <id>` | ✓ | 一条需求，含正文与关联 |
| `forge req add` | | `input: { title, priority?, body }` 或 `input: { items: [...] }` 批量；返回分配的编号 |
| `forge req update <id> [--status s] [--priority p]` | | 改字段；正文放 `input.body` |
| `forge spec list [--capability c]` | ✓ | 条目列表 |
| `forge spec get <id>` | ✓ | 一个条目，含验收 |
| `forge spec coverage` | ✓ | 没有规格的已接受需求、没有需求的规格条目、验收未通过的 |
| `forge flow new` | | `input: { title, reqs?, body }`，建变更，进入 `propose` |
| `forge flow list` / `flow show <id>` | ✓ | 变更列表；一次变更的阶段、计划、验收 |
| `forge flow next <id>` | ✓ | 下一阶段、该用哪个 skill、自动模式下要不要停 |
| `forge flow delta <id>` | | `input: { add: [...], modify: [...], remove: [...] }`，写规格增量 |
| `forge flow plan <id>` | | `input: { items: [...] }`，写计划清单 |
| `forge flow check <id> <P#>` | | 勾掉一个计划项 |
| `forge flow verify <id>` | | `input: { results: [{ spec, criterion, pass, note }] }`，记验收结果 |
| `forge flow advance <id>` | | 进入下一阶段，记历史 |
| `forge flow archive <id>` | | 把增量合进现行规格、关联需求标为 `done`、变更移入 `archive/` |

列表默认只给编号、标题、状态等短字段，正文要 `get`。选项用 `agix.cli.parseArgs` 解析；参数错了，
错误消息就是这条命令的用法。

`advance` 只因结构错误拒绝（编号不存在、计划为空就进 `apply`、没有验收记录就进 `archive`），
不因内容好坏拒绝。

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

每个阶段的 skill 正文写三件事：这一阶段要做什么、用哪几条命令、做完调 `forge flow advance`。
命令的完整用法不进 skill，要时 `forge help`。

**手动**：用户按顺序调 skill，或者 `/forge-next C-003`——它调 `flow next` 再展开对应的 skill。

**自动**：`config.yaml` 里 `flow.auto: true`。每个 skill 的最后一步是 `flow next`；它说可以继续，
就接着展开下一个阶段的 skill；到 `stop_before` 里的阶段停下，告诉用户在等什么。

## 5. 程序与模型的分工

程序做的，模型不做：编号、关联与追溯、字段校验、状态与阶段的推进、列表与筛选、覆盖统计、
把增量合进现行规格、移目录。

模型做的，程序不做：和用户谈需求、写规格正文与验收标准、拆计划、写代码、判断验收是否通过。

批量的机械活，模型写一个 `RunCode` 程序循环调 `RunExt`：一次导入三十条需求、把一批需求改成
`accepted`，都是一次工具调用。

## 6. 护栏

`scripts/guard.js` 绑在 `tool.around` 上：`via` 是 `"model"` 或 `"RunCode"`，且路径落在
`forge/` 下的 `Read`、`Write`、`Edit`、`Glob`、`Grep`，拒绝，理由里写该用哪条 `forge` 命令；
`via` 是 `"RunExt:forge"` 的放行。模型因此拿不到散文式的数据文件，也改不坏它们的结构。

`scripts/rules.js` 在 `prompt.block` 加一行：这个项目的需求、规格、变更都在 forge 里，用
`RunExt` 的 `forge` 命令读写，不要直接打开 `forge/` 下的文件。

人照常可以在编辑器里打开、在 git 里看这些文件。

## 7. 测试

**桩层**（`tests/blackbox`，CI 每次 push 都跑）：

- 打包、`plugin.install`，`plugin.list` 与披露里有 `forge` 命令和它的写入范围
- 模型桩经 `RunExt` 走完 propose → archive：文件落在项目的 `forge/` 下，归档后现行规格含增量
- 只读子命令在 plan 模式下可用；写入子命令在 `default` 下询问一次
- `writes` 之外的写入失败；包里带字节码拒装
- 模型桩直接 `Read` `forge/` 被守卫拒绝，`RunExt` 自己的读写不受影响
- `reqs` 切到 `mcp` 后端（toy 服务的 `--store` 模式），同一组命令结果一致
- `RunCode` 循环调 `RunExt` 批量改状态
- `/forge req list` 不经模型、不进对话

**LIVE**（`live_forge.rs`，Flash，不开 bypass）：

- 手动：从一段需求走到归档——在一个空项目里，按阶段做出一个只用标准库、几百行的 Rust 命令行程序。
  断言只看外部结果：测试框架自己跑 `cargo test` 通过；`forge/` 里需求是 `done`、规格含这次的条目、
  变更在 `archive/` 下且每条验收都有记录；模型没有成功直接读写过 `forge/`
- 自动：`stop_before: [apply]` 时停在 apply 前，等用户
- 需求走远端后端
- 模型读 `forge/` 被拒后改用命令
- 对比（只出报告）：同一件事，装与不装 forge，比较 prompt token 与轮数

## 8. 打包

不需要构建：YAML、校验、glob、参数解析都用引擎的 `agix.*`，`forge.js` 里只有 forge 自己的代码。

```bash
cd examples/plugins/forge && zip -r ../forge.zip . && shasum -a 256 ../forge.zip
```

再按 `plugin_packages.md`「打包与安装」发 `plugin.install`。

拿它做自己的插件：复制目录，改 `[plugin] name` 与 `[[command]] name`，换掉 `forge.js` 里的领域
代码，留下 `types/`。
