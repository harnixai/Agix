# `agix`：脚本里的基础库

Agix 里跑 JavaScript 的地方有三处，三处都有一个全局对象 `agix`，带着一组纯计算的函数：

| 载体 | 谁写的代码 | 文档 |
|---|---|---|
| 扩展点上的脚本（`[[script]]`、`settings.json` 的 `scripts`） | 运维者、插件包 | `extending_quickjs.md` |
| `RunCode` | 模型 | `ptc.md` |
| `RunExt` 跑的插件命令（`[[command]]`） | 插件包 | `runext.md` |

有了它，写插件的人不必各自打包一份 YAML 解析器、glob 匹配、JSON Schema 校验；更要紧的是，
脚本里的匹配与解析和引擎自己用的是同一套实现——一条 glob 在脚本里匹配的结果，和 `Glob` 工具、
权限规则里的一样。

`agix` 在运行时创建时就装好，不进提示词，不占上下文；`RunCode` 的工具描述里只有一行提到它，
完整清单在它的使用指南里（`ToolSearch select:RunCode`）。

## 1. 收录原则

一个函数进 `agix`，下面四条都要满足：

1. **纯计算，不碰 I/O。** 不读写文件、不联网、不起进程、不看时钟以外的环境。所以它能出现在
   "够不到任何 I/O"的扩展点脚本里而不破坏那条约束；读写文件只走 `tools.*`。
2. **和引擎的行为要一致。** 引擎自己也做这件事（glob、YAML、JSON Schema……），脚本各打包一份库
   会和引擎对不上。
3. **几乎每个结构化数据类的扩展都用得上。** 只有一个插件用得上的，留在那个插件里。
4. **格式与语义稳定。** 进来就是契约，只增不改（§4）。

明确不收：文件、网络、进程、定时器；Markdown 语法树、模板引擎这类大而各有取舍的东西；任何仿
Node 或浏览器的 API。

## 2. 命名约定

- **一个全局 `agix`，下面一层命名空间，再下面是函数。** 命名空间是小写名词，说的是格式或领域
  （`yaml`、`glob`、`json`）；函数是 camelCase 的动词。不再往下嵌套。
- **有家喻户晓的同名 API 就照它的名字和语义来，语义不同就不借它的名字。**
  `parse` / `stringify` 同 `JSON`；`agix.path` 同 Node 的 `path.posix`；`agix.cli.parseArgs` 是 Node
  `util.parseArgs` 的子集；返回布尔的匹配叫 `test`，同 `RegExp.prototype.test`。
- **规则在前，数据在后**：`glob.test(pattern, path)`、`json.validate(schema, value)`。
- **全部同步。** 纯计算没有理由返回 Promise。
- **代价有界。** YAML、glob、JSON Schema 是原生实现，脚本的时限打断不了一次原生调用，所以
  它们按输入设了上限（§3 各节），正常的输入远在其内；`glob.filter` 把长列表分段交给原生实现，
  段与段之间可以被打断。万一一次调用仍停不下来，载体到时不再等它（`ptc.md`、`extending_quickjs.md`）。
- **输入输出都是普通的 JSON 值与字符串**，不返回类实例。路径一律 POSIX 写法（`/`）。
- **出错抛 `AgixError`**（`Error` 的子类，`e.name === "AgixError"`），带 `code`，形如 `yaml.syntax`；
  "校验不通过"不算出错，以返回值表达。要认出这类错误按 `code` 判断，不按 `name`：改了产品名的
  嵌入方里 `name` 随别名（见本节末条）。参数类型不对（该传字符串的传了数字）抛 `TypeError`，同 Node。
- 嵌套超过 127 层的值报错（`yaml.depth`、`json.depth`），与引擎读 JSON 的上限相同。
- **改了产品名的嵌入方**（`Layout` 的产品名不是 Agix），同一个冻结对象还挂在产品名的小写形式下：
  只取 ASCII 字母和数字，如 `Codix` → `codix`。三处载体都装这个别名，`agix` 照常可用；模型读到的
  （`RunCode` 的工具描述与指南）只写别名。装了别名的载体里错误也随别名命名：别名首字母大写加
  `Error`，如 `CodixError`（`e.name`、`e.constructor.name` 与 `String(e)` 都是它），模型从程序输出里
  读不到 Agix；没有别名的仍是 `AgixError`。产品名化出的名字不以字母开头、是保留字、或与脚本已有的
  全局名（`tools`、`console`、`Math`……）重名时不装别名，模型读到的文字里也不提这些辅助函数。

## 3. 函数

### `agix.version`

字符串，引擎版本的 `x.y.z`。插件包要求某个版本以上，在 `plugin.toml` 写 `requires_engine`；
脚本里要兼容更早的引擎，按成员是否存在判断（`if (agix.uuid?.v7)`）。

### `agix.yaml`

| 函数 | 说明 |
|---|---|
| `parse(text) → value` | 一个 YAML 文档解析成 JSON 值 |
| `stringify(value) → text` | JSON 值写成 YAML |
| `parseFrontmatter(text) → { data, body }` | 拆出文首 `---` 包起来的 YAML 与其后的正文；没有 frontmatter 时 `data` 为 `null`、`body` 是全文 |
| `stringifyFrontmatter(data, body) → text` | 反过来；`data` 为 `null` 时只有正文 |

- YAML 1.2 core schema。结果只有 JSON 能表达的值：没有日期类型（日期就是字符串），
  超出 ±2⁵³ 的整数与 `.inf`、`.nan` 报错（`yaml.number_range`），core schema 以外的 tag 报错（`yaml.tag`）。
- 键是字符串：标量键一律按字面取（`1: a` 的键是 `"1"`），映射或序列作键报错（`yaml.key`），
  重复的键报错（`yaml.duplicate_key`）。
- 锚点与别名展开，经别名展开的节点数（十万）与字节数（8 MiB）有上限，防"十亿笑声"（`yaml.alias_limit`）。
- 多文档的输入报错（`yaml.multiple_documents`）。
- `parse` 至多读 8 MiB 的文本，更长的报 `yaml.too_large`（`parseFrontmatter` 按 frontmatter 那段算）。
- `stringify` 的输出：块风格、缩进 2、键按插入顺序、不折行、只在必要时加引号、以换行结尾。
  `undefined`、函数、非有限数报错（`yaml.unrepresentable`）。
- frontmatter 的起止行是单独一行的 `---`；接受 BOM 与 CRLF。起始行之后没有结束行报 `yaml.syntax`；
  frontmatter 为空时 `data` 为 `{}`。`stringifyFrontmatter` 不改正文的任何一个字节，
  `parseFrontmatter(stringifyFrontmatter(d, b)).body === b`；`data` 为 `null` 而正文以 `---` 行开头时
  读回来会被当成 frontmatter，所以报 `yaml.unrepresentable`。
- 语法错误的 `AgixError` 带 `line`、`column`（都从 1 起；frontmatter 里的错误按全文计行）。

### `agix.glob`

| 函数 | 说明 |
|---|---|
| `test(pattern, path) → boolean` | `path` 是否匹配 |
| `filter(pattern, paths) → string[]` | 匹配的那些，保持原顺序 |

`pattern` 是一个字符串或字符串数组（任一匹配即可）。通配的语义与 `Glob` 工具、权限规则里的 glob
相同：`*`、`?` 不跨 `/`，`**` 跨目录，支持 `{a,b}` 与 `[abc]`，大小写敏感，`.` 开头的名字不特殊
对待。不同在于这里只按字符串比，不碰文件系统：相对的模式配相对的路径，绝对的配绝对的；权限规则的
deny 与 ask 还拿解析后的真实路径再比一次（macOS 上那是磁盘上的大小写，所以 `Docs/**` 也拦得住写成
`docs/x` 的已有文件），而且不带 `*`、`?`、`[` 的条目按路径前缀算（`docs` 管住 `docs/` 下的一切），
这里要整串匹配。模式写错抛 `glob.pattern`；单个模式长于 4,096 字符、一次调用的模式合计长于
16,384 字符，同样抛 `glob.pattern`。

### `agix.path`

Node `path.posix` 里不依赖当前目录、不碰文件系统的那几个，语义相同：

`join(...parts)`、`normalize(p)`、`relative(from, to)`、`dirname(p)`、`basename(p, ext?)`、
`extname(p)`、`isAbsolute(p)`。

没有 `resolve`：它要一个当前目录，脚本里的"当前目录"由载体以参数给出（`RunExt` 的
`ctx.projectRoot`），不是一个隐含的全局状态。同样的原因，`relative` 的答案要靠当前目录才定得下来时
——一个绝对一个相对，或相对路径用 `..` 爬出了起点——抛 `path.relative`，其余情况与 Node 相同。
只做字面处理，不解析符号链接。

### `agix.json`

| 函数 | 说明 |
|---|---|
| `validate(schema, value) → { valid, errors }` | `errors` 每项 `{ instancePath, schemaPath, message }`，路径是 JSON Pointer；至多 100 项 |

- 缺省按 draft 2020-12；schema 里写了 `$schema` 的按它（draft 4、6、7、2019-09）。
- `format` 参与校验（`date-time`、`date`、`time`、`email`、`uri`、`uuid` 等）。
- `$ref` 只解析 schema 内部的；指到外部的抛 `json.external_ref`——这里不联网也不读文件。
- `schema` 与 `value` 按 `JSON.stringify` 的语义取值（`undefined` 的属性略去，`NaN` 成 `null`）。
- schema 本身不合法抛 `json.schema`。
- 校验前先数一遍代价：把 schema 的各部分套到值的各部分上，`anyOf` 等的每个分支、
  `patternProperties` 的每个模式都算，超过一百万次（或一路引用嵌套超过 1,024 层）抛
  `json.too_complex`，不开始校验。两个分支都对同一处递归的 schema 代价随深度指数增长，
  会在这里被拒；普通的递归 schema（树、链表）不受影响。

### `agix.hash`

| 函数 | 说明 |
|---|---|
| `sha256(input) → string` | 小写十六进制。`input` 是字符串（按 UTF-8）或 `Uint8Array` |

### `agix.uuid`

| 函数 | 说明 |
|---|---|
| `v4() → string` | 随机 UUID |
| `v7() → string` | 按时间有序的 UUID，适合当记录的键 |

小写、带连字符。这一组是唯一不确定的函数：随机数与时钟，仍不碰 I/O。

### `agix.cli`

| 函数 | 说明 |
|---|---|
| `parseArgs(argv, config) → { values, positionals }` | 解析命令行参数 |

Node `util.parseArgs` 的子集：`config.options` 每项 `{ type: "string" | "boolean", short?, multiple?, default? }`，
`config.allowPositionals` 缺省 `true`。总是严格模式：不认识的选项抛 `cli.unknown_option`，缺值抛
`cli.missing_value`（下一个参数以 `-` 开头也算缺值，要传这样的值写 `--name=-x`），布尔选项带了值抛
`cli.unexpected_value`，不收位置参数时出现位置参数抛 `cli.unexpected_positional`，`config` 本身写错抛
`cli.config`。消息里带选项名，命令可以直接把它交回模型。

```js
function main(argv, input, ctx) {
  const [group, verb, ...rest] = argv;
  if (group === "req" && verb === "list") {
    const { values } = agix.cli.parseArgs(rest, {
      options: { status: { type: "string" }, priority: { type: "string" } },
      allowPositionals: false,
    });
    // …
  }
}
```

## 4. 稳定性

- 只增不改：已有的名字不改名、不改语义、不删。要换语义就起一个新名字，旧的留着。
- `api_version` 不变的前提下，新增的成员随引擎版本出现；插件以 `requires_engine` 声明下限。
- 加一个函数要同时做完五件事：本文一节；`types/agix.d.ts`；作者用的 Node 替身；三个载体里各一条
  用例；`agix` 的成员清单与本文对账的守卫测试跟着过。

## 5. 实现

| 内容 | 位置 |
|---|---|
| 绑定的实现（Rust 函数、注册、`AgixError`） | 新 crate `crates/js-lib`，`ptc` 与 `script-host` 都依赖它 |
| `agix.cli` 等用 JS 写更自然的部分 | 同一 crate 里的一个 JS 模块；进程第一次用到时编译成字节码缓存在内存里，此后每个运行时直接加载 |
| YAML | `saphyr-parser`（`yaml-rust2` 的后继，纯 Rust，过 YAML 测试集）只出事件流；core schema 解析、别名展开与上限、输出都在 `js-lib` 里，行为以 §3 为准 |
| glob、JSON Schema、SHA-256、UUID | 工作区已有的 `globset`、`jsonschema`、`sha2`、`uuid`（加 `v7` 特性） |
| 装进运行时 | 三个载体创建上下文时第一件事装 `agix`，深度冻结，之后才求值用户代码 |

`agix` 不随 `scripts` / `ptc` 特性以外的东西出现：没有 JavaScript 引擎的构建里也没有它。
