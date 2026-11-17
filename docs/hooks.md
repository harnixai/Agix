# Hooks

hook 是写在设置里的外部回调:引擎走到一个具名时刻(调工具之前、一轮要结束时、压缩之前……)
就把这一刻的情况作为一份 JSON 交给它,它可以只是看看,也可以在允许的地方拦下、改写或者
把模型送回去接着干。一个会话里一共有二十九个这样的时刻,`HookEvent` 的每个变体是一个;
后端有四种:跑一条 shell 命令、问一次模型、POST 一个 URL、起一个子 Agent。

在扩展面里的位置见 [`extension_points.md`](extension_points.md) 的 `hooks` 一节;
嵌入方用 Rust 拦同样的东西,用的是那里的 `tool.around`、`prompt.submit` 等扩展点。

---

## 配置在哪

`settings.json` 的 `hooks_config`:事件名到一组 hook 的映射。

```json
{
  "hooks_config": {
    "PreToolUse": [
      { "type": "command", "command": "check-push.sh", "if": "Bash(git push:*)", "timeout": 5000 }
    ],
    "Stop": [
      { "type": "prompt", "prompt": "If the user's request is not fully done, answer {\"decision\":\"block\",\"message\":\"<what is left>\"}; otherwise answer {}." }
    ],
    "SessionEnd": [
      { "type": "http", "url": "https://hooks.example.com/agix", "headers": { "authorization": "Bearer …" } }
    ]
  }
}
```

- **按层合并。** 设置文件各层(全局 `~/.agix/settings.json` → 场景 → 项目
  `.agix/settings.json`,每层后面跟它自己的 `settings.local.json`)按 JSON 递归合并:
  不同事件各自保留,**同一个事件在高层出现时,高层的数组整个替换低层的**。
  唯一的例外是宿主连接登记的 hook(下面「宿主连接的 hook」):它不在设置里,永远叠加在设置
  hook 之上,不会被任何一层替换。
- **未信任的项目不算数。** 项目没被信任时(`ARCHITECTURE.md` §6.6),它两份设置文件里的
  `hooks_config` 整段忽略、记一条 warn,低层同名事件的 hook 照常生效;`project.trust` 之后
  这个项目的会话重建,项目的 hook 才加载。`project.status` 的 `requires_trust` 里有
  `hooks_config` 就说明项目配了 hook。
- **坏一条只丢一条。** 不认识的事件名(拼错、写给更新版本引擎的)、格式不对的条目,丢掉那一条、
  记一条 warn,其余照常生效。事件名区分大小写,不做"最接近的"猜测。
- **从不触发的事件会说出来。** 现在没有这样的事件;若将来有,表里会标 **从不触发**(清单是
  `hooks::UNWIRED_EVENTS`),给它配了 hook,会话建起来时记一条 warn。
- **整棵会话树一个 runner。** 子 Agent 用父会话的 hooks:子 Agent 里的工具调用、轮次照样
  触发配置的 hook,payload 带的是子 Agent 自己的 `session_id`、`cwd`、`permission_mode`、`depth`。

---

## 四种后端

`type` 字段选后端。四种都认 `timeout`(毫秒,不写就是 **60 秒**,最多 **10 分钟**——写得更大的
按 10 分钟算,加载时记一行 warn)、`if` 和 `on_error`(见"失败")。

### `command`

```json
{ "type": "command", "command": "./scripts/check.sh", "shell": "bash",
  "timeout": 5000, "if": "Bash", "only_on_error": false, "once": false, "async_rewake": false }
```

- 以 `<shell> -c <command>` 起进程,`shell` 缺省 `bash`;工作目录是 payload 的 `cwd`。
- `PATH` 前面依次加上项目的 `.agix/hooks/`(项目未被信任时没有这一项)、
  `~/.agix/scenes/<scene>/hooks/`、`~/.agix/hooks/`,所以 `command` 可以直接写放在那里的脚本名。环境与 Bash 工具同一套
  (`sandbox.subprocess_env`)。command hook **不进 OS 沙箱**,以 daemon 的身份运行。
- **未信任的项目里不跑项目给的程序。** 工作目录照旧是项目里的 `cwd`,所以 `./scripts/check.sh`、
  `scripts/x.sh` 这类相对路径落到的是仓库放在那里的文件。项目未被信任时(ARCHITECTURE §6.6),
  在场的 hook 都来自全局层、场景层。它们若以相对路径起程序——`command` 里任何一段启动的程序
  (`cd` 之后、`sh -c` 里、`env` / `sudo` 等 wrapper 后面的都算)或 `shell` 本身——或者命令里
  有哪个相对路径的词此刻就是项目里的一个文件或目录——解释器读的脚本(`python3 scripts/check.py`、
  `node hook.js`、`awk -f x.awk`、`bash x.sh`、`. env.sh`)、解释器跑其入口文件的目录(`python3 pkg/`、
  `node .`、`deno run .`;`.`、`./` 也算)、选项的值(`--require=./x.js`,短选项紧跟的值
  `-r./x.js`)、
  `NAME=value` 的值、`<` 读进来的、`cat x.sh | sh` 里的,以及展开后落到项目文件上的未加引号的
  glob / 花括号(`sh scripts/*.sh`、`for f in scripts/*.py`)——就不跑,算这个 hook 失败(按 `on_error`
  处理,缺省记 warn、放行),失败原因里写着是哪个词。文件按工作目录和命令里 `cd` 去的目录找;
  哪个程序把哪个参数当代码执行不去猜,所以把项目文件、目录当数据读的(`grep x README.md`、
  `ls src`、`prettier --check .`)同样不跑。`cd` / `pushd` 去的那个目录、`>` 写到的文件、不存在的
  路径、选项本身不算;短选项后面的字母数字串(`-lscripts`)分不清是值还是几个开关,整词看,不拆。
  命令里 `cd` 到项目外的绝对路径之后,工作目录仍在查找之列:`cd /opt && python3 x.py` 在项目根下
  也有 `x.py` 时会被误拒。继承来的 `PATH` 里的相对条目(`.`、空条目)
  同时去掉,裸程序名只在 hooks 目录与绝对路径的目录里找。自己的脚本写绝对路径(`~/`、`$HOME/`
  也算)或放进 `~/.agix/hooks/` 写裸名。`$PWD` / `${PWD}` 就是 hook 的工作目录,`$PWD/x.sh`
  按 `./x.sh` 算;其余变量拼出来的路径(`$TOOLS/x.sh`)不在检查之列。项目信任之后照旧。

  这条只看命令里写出来的词。程序自己去项目里找来执行的东西不在其中:`git` 读项目的
  `.git/config`(`core.fsmonitor` 等)、`make` / `npm run` / `cargo build` 跑项目定义的步骤、
  `python3 -c` / `-m` 从工作目录导入模块。用户层 hook 要在未信任的项目里跑这类命令,自己先想清楚。
- stdin 收到一行 JSON payload(见下)。
- **退出码只认 0 和 2。**
  - `0`:stdout 是 JSON 回答;stdout 为空等于什么都不说;不是 JSON 的记 warn,当作没说。
  - `2`:拦下(等同 `{"decision":"block"}`),理由取 stderr,stderr 为空取 stdout;
    stdout 不当 JSON 读。
  - 其他(含 1、被信号杀掉):这个 hook 坏了,记 warn,**放行**,什么都不改。
- 只有 command 有的三个开关:
  - `only_on_error`:只在 payload 的 `is_error` 为 `true` 时跑(配在 `PostToolUse` 上用)。
  - `once`:在这个会话(连同它的子 Agent)里**真正跑过一次**后不再跑;`if`、`only_on_error`
    没放行的那几次不算。
  - `async_rewake`:回答里带 `"rewake": true` 时记下这个 hook,等后台工作结束后以
    `hook_event_name: "WakeEvent"` 再跑它一次(重跑的回答被丢弃)。"后台工作结束"指会话的一个
    运行中任务(子 Agent、后台命令、Monitor)结束;嵌入方给了 `Builder::wake_receiver` 的,
    改为在那条通道上收到信号。会话在两条输入之间查一次:处理完一条输入、等下一条之前,有
    信号就重跑。hook 要求唤醒之前就结束的工作不算。

### `prompt`

```json
{ "type": "prompt", "prompt": "…", "model": "claude-haiku-4-5", "timeout": 30000, "if": "",
  "may_approve": false }
```

一次不带工具的模型调用:`prompt` 作为系统提示词,payload 的 JSON 作为用户消息,模型回的
文本当作 JSON 回答解析(包在 ```` ```json ```` 里也认);解析不出来等于什么都不说。走会话
自己的 provider,`model` 换的是模型名。这次调用在遥测里记为 `api_request`,`purpose` 为
`hook`。

**模型只能拦,不能批。** 模型回的 `"decision": "approve"` 缺省被丢掉,只有写了
`"may_approve": true` 的 hook 才算替人答应权限询问(见"回答")。理由:模型读的 payload 里有
`tool_input`,写这份入参的(被注入的模型、仓库里的内容)可以对着它下功夫,让它说"没问题";
它说"不行"才可信。同一个理由,模型回的 `additional_context` 也只在 `may_approve: true` 时放进对话,
否则丢掉并记一条 warn。`block`、`continue`、`updated_input`、`message` 不受影响。

### `http`

```json
{ "type": "http", "url": "https://hooks.example.com/agix",
  "headers": { "authorization": "Bearer …" }, "timeout": 10000, "if": "*", "allow_local": false }
```

- `POST` payload 的 JSON(`content-type: application/json`),加上配置的 `headers`。
- 2xx:响应体当 JSON 回答解析,解析不出来等于什么都不说。其他状态码、连不上、超时:失败,
  记 warn,放行。
- **只能去公网。** url 写在一份仓库也能带进来的设置文件里,所以每一跳(最多 10 次重定向,
  每一跳都查)都不许落到本机或内网:回环、私有网段、链路本地、`0.0.0.0/8`、
  `100.64.0.0/10`、`198.18.0.0/15`、组播、保留段,IPv6 的 ULA、链路本地、站点本地,以及
  映射或内嵌这些 IPv4 的 IPv6 地址。主机名在发出前查一次、建连时由连接自己的解析器再查
  一次,第二次解析到内网同样拒绝。`sandbox.allowed_domains` 管不到它(它管的是模型选的
  目标),也放不开这条。重定向换了源(协议、主机、端口任一个不同)时,配置的 `headers` 与
  `authorization`、`cookie` 一律不跟过去,只留 `content-type`、`accept*`、`user-agent` 这类描述
  请求本身的头。
- **`allow_local: true` 放开 url 自己。** 写了它的 hook 可以指向本机或内网
  (`http://localhost:9000/hook`),放开的只是 url 里的那个主机与端口:重定向回到这个主机与端口
  的照样能去,重定向去别的内部地址(同一台机器的另一个端口也算)照旧被拒,去公网的照旧按上面
  检查。缺省 `false`。只写在自己管的设置文件里:仓库带来的设置同样能写它。

### `agent`

```json
{ "type": "agent", "prompt": "Review this change for secrets…", "timeout": 120000, "if": "Write|Edit",
  "may_approve": false }
```

用会话的 delegator 起一个子 Agent,任务是 `prompt` 后接 `Context:` 和 payload 的
JSON。子 Agent 的最终回答应当是 `{"ok": true|false, "reason": "…"}`:`ok: false` 或缺 `ok` 记为
`block`,`reason` 作 `message`;`ok: true` 缺省什么都不说,写了 `"may_approve": true` 的记为
`approve`(配在 `PreToolUse` 上就是替人答应权限询问,见"回答";为什么缺省不算见 `prompt`)。
不是 JSON 的回答只当 `message`,不拦不改。写了 `model` 的,子 Agent 跑在这个模型上,provider
按会话的路由选;不写用 `task_models.subagent` 的模型,没配就与子 Agent 一样(会话在
`default_provider` 上时用会话当前的模型,否则用设置的 `model.model_name`)。

这个子 Agent 不占 `execution.max_concurrent_subagents`,模型把名额占满了照样起。它自己的事件
(工具调用、`SubagentStart` 等)不再起 agent hook,按执行失败处理、走 `on_error`;command /
prompt / http hook 对它照旧跑。它读的是模型写的 payload,所以只有 `Read`、`Grep`、`Glob`
(外加 `ToolSearch`),要批准的调用直接拒、不问人。它一次性跑完就丢,不做记忆抽取。它在 payload 的
`cwd` 里干活——那是项目根或项目的一个 worktree(仓库根的 worktree 目录下,缺省 `<项目根>/.wkts/`)时;判 worktree 里的
子 Agent 的调用,它读的就是那个 worktree。别的目录不收,它照旧在根会话的工作目录里。

---

## `if`

决定这个 hook 对这一次事件跑不跑,写法同权限规则:

| 写法 | 命中 |
|---|---|
| `""`、`*`(或不写) | 一切,包括不带工具的事件 |
| `Bash` | `tool_name` 等于 `Bash` |
| `Bash(git push:*)` | 工具名相同,且命令启动的程序是 `git push` 或以 `git push ` 开头(见下) |
| `Write(*.rs)` | 内容匹配这个 glob(`*` 跨目录;文件路径的比法见下) |
| `mcp__github` | 这个 MCP server 的所有工具(`mcp__github__…`) |
| `Edit\|Write`、`Bash(git push:*)\|Write` | 任一个;括号里的 `\|` 属于内容 |

"内容"依次取 `tool_input` 的 `command`、`file_path`、`path`、`notebook_path`、`url`。除 `""` / `*` 之外的
写法都要求 payload 有 `tool_name`——表里哪些事件带它,写在 payload 一列。

**`Bash` 与 `Monitor` 的内容按 `Bash(...)` deny 规则的读法比**(`ARCHITECTURE.md` §6.1,
`permissions::invocation`):整行、按 `&&` / `;` / `|` 等切开的每一段、以及每一段实际启动的程序
(去引号、展开花括号、剥掉 `env` / `exec` / `command` / `sudo` / `timeout` / `xargs` 等 wrapper
与它们的选项、程序路径取名字,`sh -c` / `eval` / `find -exec` 里的也看)分别去比,任一个命中就
跑。所以 `Bash(rm:*)` 对 `cd . && rm -rf x`、`"rm" x`、`\rm x`、`/bin/rm x`、`env rm x`、
`bash -c 'rm x'` 都命中;`echo rm`、`git rm x` 不命中。**程序名读不出来的命令一律命中**
(`$CMD x`、命令名是通配或替换的),和 deny 规则在这种时候改为询问是同一个道理:守着某条命令的
hook 不能被换个写法绕过去,宁可多跑一次。**文件路径(`file_path`、`path`、`notebook_path`)** 先按 payload 的 `cwd` 解析成去掉 `.` 与 `..` 的
绝对路径,再连同它在磁盘上的实际落点、相对 `cwd` 的写法一起比,任一个命中就跑。所以守着
`/proj/src/billing/x.rs` 的 `Write(/proj/src/billing/**)` 或 `Write(src/billing/**)`,对模型写的
`./src/billing/x.rs`、`src/../src/billing/x.rs`、绝对路径都命中。`url` 与其他工具的内容按原文比。

---

## payload

每份 payload 都有五个字段:

| 字段 | 内容 |
|---|---|
| `hook_event_name` | 事件名,同配置里的键(`async_rewake` 的重跑是 `WakeEvent`) |
| `session_id` | 触发这一事件的会话;子 Agent 里触发的是子 Agent 的 id |
| `cwd` | 会话的工作目录;在 worktree 里跑的子 Agent 是那个 worktree |
| `permission_mode` | 触发时会话**当下**的权限模式:`default`、`plan`、`acceptEdits`、`bypassPermissions`、`dontAsk`、`bubble` |
| `depth` | 会话的委派深度:根会话 0,子 Agent 逐层加 1 |

其余字段按事件出现,没有的不写(不是 `null`):

| 字段 | 类型 | 说明 |
|---|---|---|
| `tool_name` | string | 工具名;生命周期事件借它放来源(`PushNotification`、`FileWatcher`、`mcp_elicitation_<server>`) |
| `tool_input` | object | 工具入参;`PreCompact` / `PostCompact` / `FileChanged` / `Elicitation*` 用它放各自的数据 |
| `tool_use_id` | string | 这次工具调用的 id |
| `tool_result` | string | 工具结果的文本(`PostToolUse`) |
| `is_error` | bool | 结果是不是失败,工具自己报告的失败(如 Bash 非零退出)也算 |
| `user_prompt` | string | 用户消息的文本 |
| `delivery` | string | `turn`(开一轮)或 `steer`(在运行中的一轮里插进来) |
| `turn_id` | string | 所在的轮 |
| `stop_reason` | string | 一轮或一个子 Agent 怎么结束的 |
| `agent_type` | string | 子 Agent 的类型,起它时选了才有 |
| `reason` | string | 人读的说明:询问的文字、拒绝的理由、错误文本、通知正文…… |
| `source` | string | `SessionStart`:这个引擎为什么启动(`new`、`resume`、`rebuild`) |
| `stop_hook_active` | bool | `Stop`:这一轮已经被 Stop hook 送回去过一次 |
| `team_name` / `teammate_name` | string | 团队成员的事件:团队名与成员名 |

---

## 回答

回答是一个 JSON 对象,所有字段可选,`{}` 或空输出等于什么都不说:

| 字段 | 作用 |
|---|---|
| `decision` | `"block"`:在能拦的事件上拦下(各事件含义见下表);`"approve"`:`PreToolUse` 上替人答应——权限检查要询问的这次调用不再问人、直接放行(无人值守会话同样),权限规则拒绝的照样拒绝,部署方 managed 层(`ARCHITECTURE.md` §6.1"用户规则与部署方规则")的 `ask` 照样问人、无人值守时照样拒绝;`PermissionRequested` 上同理(见下);别的事件上不起作用。`command` 与 `http` 的 `approve` 都算;`prompt` 与 `agent` 的只在写了 `may_approve: true` 时算 |
| `message` | 拦下的理由——交给模型或写进结果的那句话 |
| `continue` | `false`:在能结束一轮的事件上结束它 |
| `updated_input` | `PreToolUse`:新的入参(整个替换);`UserPromptSubmit`:新的消息文本(JSON 字符串) |
| `additional_context` | 一段文字,在模型的下一步之前放进一个 `<system-reminder>` 交给它(写法也认 `additionalContext`;不认 Claude Code 的 `hookSpecificOutput.additionalContext`,要写在顶层)。只有 `SessionStart`、`UserPromptSubmit`、`PreToolUse`、`PostToolUse`、`PostToolUseFailure`、`SubagentStart` 读它,别的事件给了会 warn 一次;`PreToolUse` 的 context 在权限判定通过之后才送,被 hook 拦下、被规则拒绝、询问被拒的调用不收;`Stop` 不收——让一轮接着做用 `decision: "block"`。每个 hook 最多 10,000 字符、一个事件合计最多 20,000,超出截断并注明;文字里的 `</system-reminder>` 会被拆开;`prompt` / `agent` hook 的只在写了 `may_approve: true` 时收 |
| `rewake` | 见 `async_rewake` |

`SessionStart` 的 context 在第一条用户消息之前送进,会话重建、恢复时该事件再发一次,就再注入一次。
`UserPromptSubmit` 的 context 是同一条用户消息里提示词之后的额外文本块;`SubagentStart` 的接在子 Agent 自己的第一条消息之后。
`PreToolUse`、`PostToolUse`、`PostToolUseFailure` 的在这批工具结果之后的下一步送进。

`PermissionRequested` 能裁决:`decision: "approve"` 不问人直接放行(managed 层的 `ask` 不认,`prompt` / `agent` hook 要 `may_approve: true`,skill 的内联命令不认),`decision: "block"` 拒绝并把 `message` 交给模型。

hook 的 stdout 只用来读回答,引擎不把它交给模型或宿主;要让模型看到的话写进 `message` 或 `additional_context`。
所以没有 `suppress_output` 这样的字段,写了也不报错。

**一个事件配了几个 hook,它们并发跑,结果按配置顺序合并:** 有一个说 `block` 就拦(理由取
第一个拦的,`block` 压过别的 hook 的 `approve`);有一个说 `continue: false` 就结束;
`updated_input` 取配置顺序里第一个给出的,后面的不看(日志记一条 warn);有改写时,`approve` 只认给出这条改写的那个 hook 自己的——别的 hook 批准的是它看到的原入参,不替改写后的入参答应询问。一个 hook 慢或者坏了,不耽误同一事件的其他 hook。

### 失败

失败指:超时(到时 command 的进程被杀掉)、起不来、command 退出码不是 0 或 2、HTTP 非 2xx
或连不上、回答不是 JSON(空输出不算,那是什么都不说)、agent 子 Agent 的回答不是 JSON、
会话没接上 prompt / agent 后端。

**缺省失败就是放行**(`"on_error": "allow"`):这个 hook 不拦、不改、不结束任何东西,只记一条
warn。

写了 **`"on_error": "block"`** 的 hook,失败等于它回答了 `{"decision": "block"}`,`message` 是
``Hook `<事件名>` #<序号> failed and its on_error is block.``(序号是它在这个事件的 hook 里
的位置,从 1 起)。命令、URL 和失败原因不进这条消息——它会到达模型和转录,而它们可能带令牌;
它们只进 daemon 日志(过 `scrub_secrets` 脱敏)。拦下的效果按事件走,同回答 `block`:
`PreToolUse` 上这次调用不执行,`Stop` 上这一轮不结束、模型被送回去,只通知的事件上不起作用。
`if`、`only_on_error` 没放行的那次不算失败。

回答能起作用的 hook 跟着它所属的东西停:回合里的事件(`PreToolUse`、`Stop`、`UserPromptSubmit`、
`PreCompact` 等)随回合被打断,其余的随会话结束。被叫停的 hook 不等超时:command 的进程被杀掉,
agent hook 的子 Agent 以 `cancelled` 停下;它按失败算,写了 `"on_error": "block"` 的照样拦下。
报告已经发生的事的 hook 不被叫停,跑到它自己的超时为止:下表里"只通知"的事件(子 Agent 随会话
一起停下时的 `SubagentStop`、会话结束时的 `SessionEnd`、回合被打断时已经跑完的工具的
`PostToolUseFailure` 等),以及工具已经跑完的 `PostToolUse`——回合在工具跑的时候被打断也照发。
不叫停也不等:回合被打断、会话关闭、被驱逐、为新的 prompt 或设置重建、daemon 关停时,"只通知"
的 hook 留在后台跑到它自己的超时为止,回合照常结束,引擎照常停下。所以重建出来的引擎的
`SessionStart` 可能比旧引擎的 `SessionEnd` 先跑完。daemon 关停时等这些后台 hook,和等引擎停下
共用同一个宽限(5 秒,从关停开始算);宽限用完还没跑完的被叫停——command hook 的进程组先收
`SIGTERM`,半秒后还在的收 `SIGKILL`——每个在日志里留一行 warn(事件、哪个 hook、跑了多久)。
进程被直接杀掉(`SIGKILL`、崩溃)时它们无从收尾;下次启动时清掉上次留下的进程组,并记一行日志。
`PreToolUse` 跑完时回合已经被打断的,这次调用不执行。

```json
{ "type": "command", "command": "policy-check.sh", "timeout": 5000, "on_error": "block" }
```

hook 是策略的补充,不是唯一的门:要硬拦的东西写成权限规则。

---

## 宿主连接的 hook

嵌入 daemon 的宿主可以不改设置文件,经 `daemon.setHooks`(`daemon_rpc_protocol.md` §6.1)在
**自己的连接**上登记 hook:引擎走到事件时,把 payload 作为 `hook_call` 帧发给这条连接,宿主用
`session.respondHook` 回答。它和上面四种后端并列,但规则不同:

- **叠加,不替换。** 同一事件上,设置里的 hook 先跑;其中有一个 `block`,宿主不再被问;
  否则宿主看到的是设置 hook 用 `updated_input` 改写之后的入参。宿主 hook 从不被高层设置替换,
  也不替换任何设置 hook。
- **事件只有五个**:`PreToolUse`、`PostToolUse`、`PostToolUseFailure`、`UserPromptSubmit`、
  `Stop`。`tools` 用工具名模式(`*`、`Prefix*`、完整名字、`mcp__<server>`、`mcp__*`)选工具,
  只对工具事件有效。
- **回答里只有三个字段算数**:`decision:"block"`(带 `message`)、`message`、
  `additional_context`,效果同上表「回答」。`approve`(宿主不能替人批准)、`updated_input`、
  `continue`、`rewake` 不收,出现就被拒(`INVALID_PARAMS`)。`PreToolUse` 的拦下消息保持
  `Blocked by PreToolUse hook: <message>` 前缀;它在权限检查之前,所以 `bypassPermissions` 下也拦得住。
- **失败按 `on_error`。** 登记时每条 hook 带 `timeout_ms`(1000..=30000,缺省 10000)与
  `on_error`(`allow` 缺省 / `block`);超时、连接没了、帧发不出去、同一连接上悬着的调用超过
  256 个都算失败,语义同上「失败」。这里的 `block` 失败消息同样是
  ``Hook `<事件名>` #<序号> failed and its on_error is block.``。
- **当策略用的 hook 写 `on_error:"block"`。** 256 个的上限由这条连接的所有会话共用,别的连接
  起的会话也能把它占满;`allow` 之下,占满之后的调用不经这条 hook 直接放行。
- **`Stop` 被拦没有次数上限**,同设置里的 hook:每拦一次这一轮多跑一步;一直拦(或在
  `on_error:"block"` 下一直不答)这一轮就一直不结束。
- **帧只发给登记者**,不发给该会话的别的订阅者。
- **范围是整个 daemon。** 它作用于每个会话、子 Agent 里的工具调用,以及 skill 的内联命令
  (`` !`cmd` `` 是一次 `Bash` 调用)。登记了 hook 的连接能看到每个会话每次相应工具调用的输入、
  能拦下它——只登记给被信任的宿主。
- **立即生效,包括已经建好的会话**;连接断开,它的 hook 随即消失,重连后要重新登记。断开时
  还没处理完的 `daemon.setHooks` 不算数。
- 一条连接至多 16 条,daemon 里至多 8 条连接同时登记 hook。

---

## 事件

下表由 `hooks::catalog` 生成,`daemon/tests/hook_events_doc.rs` 保证它和代码一致——改
`HookEvent::spec`,别改表(`AGIX_UPDATE_DOCS=1` 重写这张表)。"只通知"的事件,hook 的回答
在触发处被丢掉,拦不了也改不了。

<!-- BEGIN GENERATED TABLE — see hooks::catalog::render_markdown -->
| 事件 | 何时触发 | payload(四个公共字段之外) | 回答能做什么 |
|---|---|---|---|
| `PreToolUse` | 每次工具调用分发之前,在输入校验与权限检查之前;子 Agent 的调用也算,skill 正文里的 `` !`cmd` `` 是这个会话的一次 `Bash` 调用(`tool_use_id` 由展开它的 `Skill` 调用派生为 `<id>.<n>`,`/name` 展开时为 `slash-<turn_id>.<n>`) | `tool_name`、`tool_input`、`tool_use_id` | `decision: "block"`(或 exit 2):这次调用不执行,模型收到 `Blocked by PreToolUse hook: <message>`;`updated_input`:换掉入参,换过的照常校验、过权限;`decision: "approve"`:权限检查要询问时不再问人,直接放行(规则拒绝的照样拒绝,managed 层的 `ask` 照样问人;`prompt` / `agent` hook 要写 `may_approve: true`);skill 的内联命令只认拦下,批准不算替人答应,要询问的照样不跑,`updated_input` 换过的命令照样按这个规矩判);`additional_context`:这段文字在模型的下一步之前,放进一个 system-reminder 交给它(权限判定通过之后、调用执行之前才送;被 hook 拦下、被规则拒绝、询问被拒的调用不收) |
| `PostToolUse` | 工具真正跑完之后(成功或失败);被 PreToolUse、权限拦下的调用不发;skill 正文里的 `` !`cmd` `` 跑完也发,是这个会话的一次 `Bash` 调用(`tool_use_id` 为派生的 `<id>.<n>`) | `tool_name`、`tool_input`、`tool_use_id`、`tool_result`(结果文本)、`is_error` | `decision: "block"`:结果换成 `Denied by PostToolUse hook: <message>`,同一批并发的其他调用被取消(skill 的内联命令:正文里换成 "[`cmd` failed: Denied by PostToolUse hook: <message>]");`continue: false`:这一批工具跑完后结束这一轮(`stopped_by_hook`;skill 的内联命令:模型调 `Skill` 时同样结束这次调用所在的一轮,用户打 `/name` 展开时还没有一轮,不起作用,展开出的那一轮照常跑);`additional_context`:这段文字在模型的下一步之前,放进一个 system-reminder 交给它 |
| `PostToolUseFailure` | 工具调用以错误结束(工具返回错误或 panic,或同批的另一个调用出错时被中途放弃)时,在 PostToolUse 之前;工具自己报告失败的结果(如 Bash 非零退出)不算;skill 正文里的 `` !`cmd` `` 是这个会话的一次 `Bash` 调用 | `tool_name`、`tool_input`、`reason`(错误文本)、`turn_id` | `additional_context`:这段文字在模型的下一步之前,放进一个 system-reminder 交给它 |
| `TurnStart` | 一轮开始,UserPromptSubmit 放行之后、第一次调模型之前 | `turn_id` | 只通知,回答被丢弃 |
| `TurnComplete` | 一轮正常走完(模型不再要工具,Stop hook 也放行了) | `turn_id`、`stop_reason` | 只通知,回答被丢弃 |
| `SessionStart` | 会话开始运行,紧接在 `Setup` 之后 | `source`:宿主为什么启动这个引擎——`new`(`session.create`、第一条消息建的会话)、`resume`(从日志接回来,包括重启或被驱逐之后再用)、`rebuild`(`setPrompt`、跨 provider 的 `setModel`、设置变了而给还在内存里的会话换引擎);嵌入方启动时没给原因的不带 | `additional_context`:这段文字在模型的下一步之前,放进一个 system-reminder 交给它(第一轮开头、第一条用户消息之前;会话重建、恢复时再发一次就再注入一次) |
| `SessionEnd` | 会话的主循环退出:收到关闭、输入通道关闭、被取消,三种都发 | `reason`:宿主为什么停下这个引擎——`close`(`session.close` / `session.delete`)、`evict`(空闲回收或池满挤出)、`rebuild`(`setPrompt`、跨 provider 的 `setModel`、设置变了而换引擎)、`shutdown`(daemon 关停);引擎自己结束的(输入通道关闭、收到 `Shutdown` 命令)、嵌入方停下时没给原因的不带 | 只通知,回答被丢弃 |
| `Stop` | 模型这一步不再要工具、这一轮要结束时;每次要结束都问一遍 | `stop_hook_active`(这一轮已经被 Stop hook 送回去过) | `continue: false`:结束这一轮(`stopped_by_hook`,随后发 `StopFailure`);`decision: "block"`(或 exit 2):不让结束,`message` 作为提醒交给模型接着做 |
| `StopFailure` | 一轮被 Stop hook 的 `continue: false` 截停时 | `reason` | 只通知,回答被丢弃 |
| `UserPromptSubmit` | 用户消息进转录之前:开一轮的那条,以及运行中 steer 进来的每一条 | `user_prompt`、`delivery`(`turn` / `steer`) | `decision: "block"`:这条消息不处理——开一轮的直接以 `stopped_by_hook` 结束、不调模型,steer 的不进转录;`updated_input`(JSON 字符串):换掉消息文本;`additional_context`:这段文字在模型的下一步之前,放进一个 system-reminder 交给它(同一条用户消息的附加文本块) |
| `PreCompact` | 每次压缩之前(按阈值自动压、`/compact`、请求因体积被拒后的压缩)。修剪不算压缩,不触发它 | `tool_input`:`messages_before`、`token_count`、`threshold`、`trigger`(`auto` / `manual`) | `decision: "block"` 或 `continue: false`:这次不压缩 |
| `PostCompact` | 压缩完成之后;修剪之后不发 | `tool_input`:`strategy`、`messages_before`、`messages_after`、`tokens_before`、`tokens_after` | 只通知,回答被丢弃 |
| `PermissionRequested` | 权限检查判为"询问"、开始等宿主回答之前(无人值守会话不问,直接按拒绝处理) | `tool_name`、`tool_input`、`reason`(询问的文字)、`turn_id` | `decision: "approve"`:不问人直接放行(managed 层的询问照样问人;`prompt` / `agent` hook 要写 `may_approve: true`);`decision: "block"`:拒绝,`message` 交给模型,不再询问 |
| `PermissionDenied` | 一次工具调用被拒:规则拒绝、`PreToolUse` 拦下、没人可问时的询问、宿主答拒绝或没给出决定、询问超时;skill 的内联命令被拒也算,它是这个会话的一次 `Bash` 调用 | `tool_name`、`tool_input`、`reason`、`turn_id` | 只通知,回答被丢弃 |
| `Setup` | 会话开始运行,早于 `SessionStart` | `reason`(`scene=<id> model=<模型>`) | 只通知,回答被丢弃 |
| `Notification` | 需要人注意时:一个权限询问在等回答;模型调了 `PushNotification` | `tool_name`、`tool_input`(被询问的调用,或 `PushNotification` 的入参)、`reason`、`turn_id` | 只通知,回答被丢弃 |
| `SubagentStop` | 子 Agent 跑完(成功、出错、被取消);常驻团队成员在被停下时 | `session_id` 是子 Agent 的;`agent_type`、`stop_reason`(`end_turn` / `error` / `cancelled` / `shutdown`) | 只通知,回答被丢弃 |
| `SubagentStart` | 子 Agent 开始跑之前,每一种起法都发:前台、后台、宿主 `session.startAgent`、`TeamCreate` 成员、常驻成员 | `session_id` 是子 Agent 的;`agent_type` | `additional_context`:这段文字在模型的下一步之前,放进一个 system-reminder 交给它(进子 Agent 自己的第一条消息,不进父的对话) |
| `TeammateIdle` | 团队成员的一轮结束(不论怎么结束),在这一轮的 `TaskCompleted` 之后 | `team_name`、`teammate_name`、`turn_id`、`stop_reason` | 只通知,回答被丢弃 |
| `TaskCreated` | 一个运行中的任务开始:子 Agent、后台 shell 命令、Monitor、团队成员 | `reason`(这个任务在 `session.tasks` 里的 JSON) | 只通知,回答被丢弃 |
| `TaskCompleted` | 一个运行中的任务结束(完成、失败、被停下),与 `TaskCreated` 成对;另外团队成员的一轮正常结束时也发一次 | 任务结束:`reason`(这个任务在 `session.tasks` 里的 JSON:`task_id`、`kind`、`status` 为 `done` / `failed` / `stopped`……);团队成员的一轮:`team_name`、`teammate_name`、`turn_id`、`stop_reason` | 只通知,回答被丢弃 |
| `Elicitation` | MCP server 在工具调用中要向用户询问时(2026-07-28 的 MRTR 询问,或老版本 server 的 `elicitation/create`),在推给宿主之前;无人值守的会话直接回 server `cancel`,不触发 | `tool_name`(`mcp_elicitation_<server>`)、`tool_input`:`server_name`、`mode`(`form` / `url`)、`message`、`requested_schema` 或 `url`;`tool_use_id` | `decision: "block"`:不问人,回 server `decline` |
| `ElicitationResult` | `Elicitation` 那次询问有了结局、答案回给 server 之前:宿主答了、超时、宿主没声明接这种询问、被 hook 拦下都算;这一轮被中断时整个调用丢弃,不触发 | 同 `Elicitation`,`tool_input` 另加 `action`(`accept` / `decline` / `cancel`);`tool_result`:接受的表单内容 | 只通知,回答被丢弃 |
| `WorktreeCreate` | `EnterWorktree` 成功之后 | `tool_name`、`tool_input`、`turn_id` | 只通知,回答被丢弃 |
| `WorktreeRemove` | `ExitWorktree` 成功之后 | `tool_name`、`tool_input`、`turn_id` | 只通知,回答被丢弃 |
| `InstructionsLoaded` | 会话上下文(项目说明)注入转录时,说明非空才发;一个会话一次 | `reason`(注入了多少字节) | 只通知,回答被丢弃 |
| `CwdChanged` | 会话开始运行时报一次工作目录;活着的会话不会换目录 | `reason`(`session working directory: <路径>`) | 只通知,回答被丢弃 |
| `FileChanged` | 根会话的 `instruction_file`,或它载入的某份 `AGENTS.md`,被改、建、删;300 ms 去抖,一次变化一个事件 | `tool_name`(`FileWatcher`)、`tool_input`:`file_path`、`change_type`(`created` / `modified` / `deleted`) | 只通知,回答被丢弃 |
| `PostSampling` | 一次模型调用的回复完整收完、写进转录之后(被取消的不发) | `reason`(`stop_reason` 与输出 token 数) | 只通知,回答被丢弃 |
<!-- END GENERATED TABLE -->

几处补充:

- **skill 的内联命令。** skill 正文里的 `` !`cmd` `` 和模型的 `Bash` 调用走同一条分发:
  中间件(`tool.around`)、结果变换(`tool.result`)、遥测、外部内容围栏都照常经过,只是没人
  可问——要询问的不跑,hook 的批准不算替人答应。没跑成的,正文里换成
  `` [`cmd` failed: <原因>] ``。
- **`Stop` 的送回。** 被 `block` 时,`message`(为空时用一句默认的话)以
  `<system-reminder>` 交给模型,这一轮接着跑;下一次要结束时 `Stop` 再问一遍,payload 的
  `stop_hook_active` 为 `true`。一个无条件拦的 hook 应当看这个字段放行;不看的,由每轮的
  调用上限截住。
- **压缩与修剪。** hook 只管压缩:改写历史的那种——丢掉旧轮次、折叠成摘要或让模型写摘要,
  事后对话里少了消息,transcript 记一个压缩标记。另有三种修剪不经过 `PreCompact`/`PostCompact`,
  也拦不住:清掉很久以前的工具结果正文(`compaction` 的时间与缓存档)、把工具结果截到预算之内、
  开了 `compaction.reactive` 时在轮中提前清旧工具结果。修剪不删消息,只把旧工具结果的正文换成占位,
  `tool_use` 与 `tool_result` 的配对不变。想在修剪时做点什么,看 `compact` 帧上的 `"trim": true`;
  想少做修剪,嵌入方用 `OnlyAtThreshold` 包住压缩器(关掉时间档、缓存档和提前修剪,工具结果预算那一档
  仍在,要改它覆盖 `trim_tool_results`),或者关掉 `compaction.reactive`。
- **任务的开始与结束。** `TaskCreated` / `TaskCompleted` 成对:任务在 `session.tasks` 里出现时
  发前者,结束(`status` 为 `done`、`failed`、`stopped`)时发后者,`reason` 是同一个任务的 JSON,
  `task_id` 对得上。两者都在会话自己的一个任务里发,和引发它们的那一步没有固定先后。后台 shell
  命令、Monitor 在一秒之内就结束的不算任务,两个都不发。
- **团队成员。** 成员(`TeamCreate` 的成员、`Agent` 带 `team_name` 的常驻成员)的会话里,
  每一轮结束还发一对:先 `TaskCompleted`(只在正常结束时)后 `TeammateIdle`,payload 带
  `team_name` / `teammate_name` 而不带 `reason`,都在这一轮落盘之后,比这一轮的 `TurnComplete`
  hook 晚。所以一个 `TaskCompleted` hook 看 payload 里有没有 `team_name` 来分这两种:有的是成员的
  一轮干完了,没有的是一个任务结束了(常驻成员作为任务结束时,`kind` 是 `teammate`)。
- **`FileChanged`。** 只有根会话起监视,子 Agent 不重复发;监视的是 `instruction_file`
  和根会话载入过的每一份 `AGENTS.md`,不是任意文件。不存在的路径不监视。
- **`UserPromptSubmit` 之后还有一道。** 嵌入方的 `prompt.submit` 扩展点跑在 hook 之后,
  hook 的改写是它看到的原文,它的改写 hook 改不回来。
